//! Benchmarks of the HTTP path of `http-tunnel`.
//!
//! They are run with `cargo bench --bench throughput` (or `--bench copy`) and
//! are plain Rust: no external benchmarking crate is used, so the repository
//! has no extra dev-dependency.
//!
//! Every scenario starts a full server + client + echo backend, warms the tunnel
//! up, then measures. The numbers are wall-clock and are meant to be compared
//! between revisions, not to be an absolute reference.

mod common;

use std::time::{Duration, Instant};

use common::{keep_alive_batch, run, short_connection_request, Harness, HTTP_ENTRY_ADDR};

/// Scenario 1 (G3): many small requests on keep-alive connections, then many
/// short-lived connections. The first shows the request throughput, the second
/// the per-connection setup cost, which is where the connection pool and the
/// channel handshake show up.
pub fn small_requests() -> Duration {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let harness = Harness::start().await.expect("failed to start the harness");

        // Warm up
        keep_alive_batch(HTTP_ENTRY_ADDR, 100, 64)
            .await
            .expect("warmup failed");

        let iterations = 4_000;
        let start = Instant::now();
        run(iterations, 32, || {
            keep_alive_batch(HTTP_ENTRY_ADDR, 8, 64)
        })
        .await;
        let elapsed = start.elapsed();
        println!(
            "keep-alive small requests: {} in {:?} ({:.0} req/s, {} requests reached the backend)",
            iterations * 8,
            elapsed,
            (iterations * 8) as f64 / elapsed.as_secs_f64(),
            harness.requests_reached_backend(),
        );

        let start = Instant::now();
        run(1_000, 32, || short_connection_request(HTTP_ENTRY_ADDR, 64)).await;
        let elapsed = start.elapsed();
        println!(
            "short-lived connections: 1000 in {:?} ({:.0} req/s)",
            elapsed,
            1_000.0 / elapsed.as_secs_f64(),
        );

        harness.shutdown().await;
        elapsed
    })
}

/// Scenario 2 (G3): several medium bodies in flight at once. This is where the
/// single-TCP-per-tunnel design is expected to win against a multiplexed one.
pub fn large_bodies() -> Duration {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let harness = Harness::start().await.expect("failed to start the harness");

        const BODY: usize = 1024 * 1024;
        const CONCURRENCY: usize = 8;
        const ROUNDS: usize = 8;

        let start = Instant::now();
        run(ROUNDS * CONCURRENCY, CONCURRENCY, || {
            keep_alive_batch(HTTP_ENTRY_ADDR, 1, BODY)
        })
        .await;
        let elapsed = start.elapsed();

        let bytes = (ROUNDS * CONCURRENCY * BODY) as f64;
        println!(
            "{} concurrent bodies of 1 MiB: {:?} ({:.0} MiB/s)",
            ROUNDS * CONCURRENCY,
            elapsed,
            bytes / elapsed.as_secs_f64() / (1024.0 * 1024.0),
        );

        harness.shutdown().await;
        elapsed
    })
}

fn main() {
    // The benchmarks are functions so they can also be driven from a test if
    // needed; the binary runs all of them in sequence.
    println!("== small requests ==");
    small_requests();
    println!("== large bodies ==");
    large_bodies();
}
