//! Watch the configuration file and hand changes to the universal process.
//!
//! The process keeps running: the supervisor in `node` stops the current
//! listener and upstream session, applies the new configuration and starts them
//! again. A change written by the management API is not a reload: the API
//! already applied it live and persisted it, so [`ReloadMarker`] lets the
//! watcher tell those writes apart from an operator editing `ht.toml`.

use crate::config::{Config, ConfigSource};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, warn};

/// How often the configuration file is checked for a new content.
///
/// Polling is deliberate: it behaves the same on every platform, survives the
/// rename-based atomic save of `Config::save`, and coalesces the several writes
/// an editor performs while saving a file.
pub(crate) const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// The last configuration the management API wrote itself.
///
/// Its writes already reached the running server, so the watcher must not tear
/// the running generation down for them. The marker is compared by value, not
/// by file timestamp, so a formatting-only save and an editor rewrite do not
/// trigger a reload either.
#[derive(Default)]
pub(crate) struct ReloadMarker {
    self_write: Mutex<Option<Config>>,
}

impl ReloadMarker {
    /// Remember the effective configuration the process is about to persist.
    #[cfg(feature = "management")]
    pub(crate) async fn mark_self_write(&self, config: Config) {
        *self.self_write.lock().await = Some(config);
    }

    /// Whether `config` is the one the management API wrote itself.
    pub(crate) async fn is_self_write(&self, config: &Config) -> bool {
        match self.self_write.lock().await.as_ref() {
            Some(marked) => same_effective_config(marked, config),
            None => false,
        }
    }

    /// Forget the mark once a reload was applied, so a later external edit back
    /// to the same content is not mistaken for another management write.
    pub(crate) async fn clear(&self) {
        *self.self_write.lock().await = None;
    }
}

/// The outcome of the most recent configuration watch/apply attempt.
///
/// It is shared across generations: a failed reload leaves the old generation
/// running, and the management page of that generation must be able to show
/// why the file change was not applied.
pub(crate) struct ReloadStatus {
    inner: StdMutex<ReloadStatusInner>,
}

#[derive(Default)]
struct ReloadStatusInner {
    last_error: Option<String>,
    last_error_at: Option<Instant>,
    last_success_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReloadSnapshot {
    pub(crate) last_error: Option<String>,
    pub(crate) last_error_at: Option<Instant>,
    pub(crate) last_success_at: Option<Instant>,
}

impl ReloadStatus {
    pub(crate) fn new() -> ReloadStatus {
        ReloadStatus {
            inner: StdMutex::new(ReloadStatusInner {
                last_success_at: Some(Instant::now()),
                ..ReloadStatusInner::default()
            }),
        }
    }

    /// Remember a file read/parse failure or an apply/bind failure. A later
    /// success clears it.
    pub(crate) fn record_error(&self, error: impl Into<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_error = Some(error.into());
            inner.last_error_at = Some(Instant::now());
        }
    }

    /// Remember a configuration that the running process accepted.
    pub(crate) fn record_success(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_error = None;
            inner.last_error_at = None;
            inner.last_success_at = Some(Instant::now());
        }
    }

    pub(crate) fn snapshot(&self) -> ReloadSnapshot {
        match self.inner.lock() {
            Ok(inner) => ReloadSnapshot {
                last_error: inner.last_error.clone(),
                last_error_at: inner.last_error_at,
                last_success_at: inner.last_success_at,
            },
            Err(_) => ReloadSnapshot {
                last_error: Some("The reload status lock is poisoned".to_string()),
                last_error_at: Some(Instant::now()),
                last_success_at: None,
            },
        }
    }
}

impl Default for ReloadStatus {
    fn default() -> Self {
        ReloadStatus::new()
    }
}

/// Whether two configurations would run the same process.
///
/// [`ConfigSource`] is load metadata, not configuration: the file path is
/// excluded.
pub(crate) fn same_effective_config(left: &Config, right: &Config) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.source = ConfigSource::default();
    right.source = ConfigSource::default();
    left == right
}

/// Whether two configurations differ only in settings the running process can
/// apply without rebuilding its listener and upstream session.
///
/// `[[proxies]]` and `[upstream].allow_modify` are already applied live by the
/// management API; a file edit of those fields can use the same path, so an
/// operator adding a service does not disconnect the current visitors.
#[cfg(feature = "management")]
pub(crate) fn only_live_fields_changed(running: &Config, next: &Config) -> bool {
    let mut running = running.clone();
    let mut next = next.clone();
    running.proxies.clear();
    next.proxies.clear();
    if let Some(upstream) = running.upstream.as_mut() {
        upstream.allow_modify = false;
    }
    if let Some(upstream) = next.upstream.as_mut() {
        upstream.allow_modify = false;
    }
    same_effective_config(&running, &next)
}

/// Start watching `path` for changes.
///
/// The receiver yields a fully parsed and validated configuration each time the
/// file changes to content that is not the last management write. A file that is
/// temporarily invalid, missing or half-written is logged once and ignored: the
/// running configuration stays in place until the next complete save.
pub(crate) fn watch(
    path: PathBuf,
    marker: Arc<ReloadMarker>,
    status: Arc<ReloadStatus>,
    interval: Duration,
) -> (mpsc::Receiver<Config>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        // Start with no baseline so a file edited between the initial load and
        // this task is still seen. The supervisor drops a config equal to the
        // one it is already running.
        let mut last: Option<Vec<u8>> = None;
        let mut reported_read_error = false;
        loop {
            time::sleep(interval).await;
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    if reported_read_error {
                        // The file is readable again even if its content did
                        // not change; the previous read error is no longer
                        // relevant to the running configuration.
                        status.record_success();
                    }
                    reported_read_error = false;
                    bytes
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    // A file that disappears is not a reason to stop serving;
                    // the running configuration stays in place.
                    last = None;
                    continue;
                }
                Err(err) => {
                    if !reported_read_error {
                        warn!(
                            path = ?path,
                            "Failed to read the configuration file, keeping the running \
                             configuration: {:#}",
                            err
                        );
                        status.record_error(format!(
                            "Failed to read the configuration file: {:#}",
                            err
                        ));
                        reported_read_error = true;
                    }
                    continue;
                }
            };
            if last.as_deref() == Some(bytes.as_slice()) {
                continue;
            }
            last = Some(bytes);

            let config = match crate::load_config(&path).await {
                Ok(config) => config,
                Err(err) => {
                    warn!(
                        path = ?path,
                        "The configuration file is invalid, keeping the running \
                         configuration: {:#}",
                        err
                    );
                    status.record_error(format!("The configuration file is invalid: {:#}", err));
                    continue;
                }
            };
            if marker.is_self_write(&config).await {
                debug!(
                    path = ?path,
                    "Ignoring the configuration written by the management API"
                );
                continue;
            }
            if tx.send(config).await.is_err() {
                break;
            }
        }
    });
    (rx, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MaskedString;
    use anyhow::Result;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "http-tunnel-reload-{}-{}-{}.toml",
            std::process::id(),
            tag,
            count
        ))
    }

    fn config_text(port: u16) -> String {
        format!(
            "name = \"reload-test\"\n\n\
             [server]\n\
             port = {}\n\
             domain = \"control.example.com\"\n\
             token = \"secret\"\n",
            port
        )
    }

    #[test]
    fn test_same_effective_config_ignores_the_load_source() {
        let mut left = Config::parse(&config_text(17001)).unwrap();
        let mut right = Config::parse(&config_text(17001)).unwrap();
        left.source.path = Some(PathBuf::from("ht.toml"));

        assert!(same_effective_config(&left, &right));

        right.server.as_mut().unwrap().port = 17002;
        assert!(!same_effective_config(&left, &right));
    }

    #[cfg(feature = "management")]
    #[tokio::test]
    async fn test_marker_only_matches_the_self_write() {
        let marker = ReloadMarker::default();
        let written = Config::parse(&config_text(17001)).unwrap();
        let other = Config::parse(&config_text(17002)).unwrap();

        assert!(!marker.is_self_write(&written).await);
        marker.mark_self_write(written.clone()).await;
        assert!(marker.is_self_write(&written).await);
        assert!(!marker.is_self_write(&other).await);

        marker.clear().await;
        assert!(!marker.is_self_write(&written).await);
    }

    #[tokio::test]
    async fn test_watch_yields_a_validated_change() -> Result<()> {
        let path = temp_path("watch");
        tokio::fs::write(&path, config_text(17001)).await?;
        let marker = Arc::new(ReloadMarker::default());
        let status = Arc::new(ReloadStatus::new());
        let (mut rx, task) = watch(path.clone(), marker, status, Duration::from_millis(20));

        // The first observation may be the config the process already runs;
        // the supervisor filters it.
        let _ = time::timeout(Duration::from_secs(2), rx.recv()).await?;

        tokio::fs::write(&path, config_text(17004)).await?;
        let config = time::timeout(Duration::from_secs(2), rx.recv())
            .await?
            .expect("the watcher stopped");
        assert_eq!(config.listen_port(), Some(17004));

        task.abort();
        let _ = tokio::fs::remove_file(&path).await;
        Ok(())
    }

    #[cfg(feature = "management")]
    #[tokio::test]
    async fn test_watch_ignores_a_management_write() -> Result<()> {
        let path = temp_path("marker");
        tokio::fs::write(&path, config_text(17001)).await?;
        let marker = Arc::new(ReloadMarker::default());
        let written = crate::load_config(&path).await?;
        marker.mark_self_write(written).await;

        let mut rx = watch(
            path.clone(),
            marker.clone(),
            Arc::new(ReloadStatus::new()),
            Duration::from_millis(20),
        )
        .0;
        // Same effective configuration, different formatting: still the
        // management write, so the watcher stays silent.
        let reformatted = format!("# saved by the API\n{}", config_text(17001));
        tokio::fs::write(&path, reformatted).await?;
        assert!(time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err());

        // A real edit after the marker is cleared is delivered.
        marker.clear().await;
        tokio::fs::write(&path, config_text(17006)).await?;
        let config = time::timeout(Duration::from_secs(2), rx.recv())
            .await?
            .expect("the watcher stopped");
        assert_eq!(config.listen_port(), Some(17006));

        let _ = tokio::fs::remove_file(&path).await;
        Ok(())
    }

    #[cfg(all(feature = "management", feature = "client"))]
    #[test]
    fn test_only_live_fields_changed() {
        let running = Config::parse(&config_text(17001)).unwrap();
        let mut next = running.clone();
        next.proxies.push(crate::config::ServerProxyEntry {
            name: "nas".to_string(),
            domain: "nas.example.com".to_string(),
            local_addr: "127.0.0.1:80".to_string(),
            x_auth: None,
        });
        assert!(only_live_fields_changed(&running, &next));

        next.server.as_mut().unwrap().port = 17002;
        assert!(!only_live_fields_changed(&running, &next));
    }

    #[cfg(all(feature = "management", feature = "client"))]
    #[test]
    fn test_only_allow_modify_is_live() {
        let source = "name = \"leaf\"\n\n\
                      [server]\nport = 17001\ndomain = \"127.0.0.1\"\n\
                      token = \"secret\"\n\n\
                      [upstream]\nurl = \"ws://a.test:7777\"\ntoken = \"upstream-secret\"\n";
        let running = Config::parse(source).unwrap();
        let mut next = running.clone();
        next.upstream.as_mut().unwrap().allow_modify = true;
        assert!(only_live_fields_changed(&running, &next));

        next.upstream.as_mut().unwrap().url = "ws://b.test:7777".to_string();
        assert!(!only_live_fields_changed(&running, &next));
    }

    #[test]
    fn test_reload_status_records_and_clears_errors() {
        let status = ReloadStatus::new();
        assert!(status.snapshot().last_error.is_none());
        assert!(status.snapshot().last_success_at.is_some());

        status.record_error("bad toml");
        let snapshot = status.snapshot();
        assert_eq!(snapshot.last_error.as_deref(), Some("bad toml"));
        assert!(snapshot.last_error_at.is_some());

        status.record_success();
        let snapshot = status.snapshot();
        assert!(snapshot.last_error.is_none());
        assert!(snapshot.last_error_at.is_none());
    }

    #[tokio::test]
    async fn test_watch_records_an_invalid_config() -> Result<()> {
        let path = temp_path("invalid");
        tokio::fs::write(&path, config_text(17001)).await?;
        let marker = Arc::new(ReloadMarker::default());
        let status = Arc::new(ReloadStatus::new());
        let (_rx, task) = watch(
            path.clone(),
            marker,
            status.clone(),
            Duration::from_millis(20),
        );

        tokio::fs::write(&path, "this is not toml = [").await?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while status.snapshot().last_error.is_none() {
            if Instant::now() >= deadline {
                panic!("the watcher did not record the invalid configuration");
            }
            time::sleep(Duration::from_millis(20)).await;
        }

        task.abort();
        let _ = tokio::fs::remove_file(&path).await;
        Ok(())
    }

    #[test]
    fn test_config_text_is_valid() {
        let config = Config::parse(&config_text(17001)).unwrap();
        assert_eq!(
            config.server.as_ref().unwrap().token,
            Some(MaskedString::from("secret"))
        );
    }
}
