use backoff::ExponentialBackoff;
use std::time::Duration;

pub fn run_session_backoff(interval: u64) -> ExponentialBackoff {
    ExponentialBackoff {
        randomization_factor: 0.2,
        max_elapsed_time: None,
        multiplier: 3.0,
        max_interval: Duration::from_secs(interval),
        ..Default::default()
    }
}
