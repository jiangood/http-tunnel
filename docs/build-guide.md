# Build Guide

This is for those who want to build `http-tunnel` themselves, possibly because they need the latest features or the minimal binary size.

## Build

The universal `ht` binary is built from the `node` feature, which enables both the embedded server and the upstream client role:

```sh
cargo build --release --bin ht
```

## Customize the Build

`http-tunnel` comes with some *crate features* that determine whether a certain feature is compiled or not. Supported features can be checked out in `[features]` of [Cargo.toml](../Cargo.toml).

| Feature | Effect |
| --- | --- |
| `server` | Embedded listener, HTTP routing, v4 session server |
| `client` | v4 session client and upstream reconnection |
| `node` | `server` + `client`; builds the `ht` binary |
| `client-api` | Web management page and API (adds `axum`) |

Build the universal binary with the API included (the default):

```sh
cargo build --release --bin ht
```

Build it without the web management API for a smaller image:

```sh
cargo build --release --bin ht --no-default-features --features node
```

A library-only build without `server` + `client` has no `ht` binary; the
`run` entry point reports that the `node` feature is required.

## Minimalize the binary

1. Build with the `minimal` profile

The `release` profile optimizes for run time, not binary size. The `minimal`
profile enables size-oriented optimization:

```sh
cargo build --profile minimal --bin ht
```

2. `strip` and `upx`

The binary can be made even smaller with `strip` and `upx`:

```sh
strip ht
upx --best --lzma ht
```

## Benchmarking

The HTTP path has a small benchmark harness in [`benches/`](../benches). It
starts an echo backend behind a leaf, an ingress, and a leaf with one upstream
session, then measures three scenarios: many small keep-alive requests, many
short-lived connections (the per-connection setup cost, where stream setup shows
up), and several 1 MiB bodies in flight at once (the large-transfer throughput).

```sh
cargo bench --bench throughput
```

It uses the same ports as the integration tests, so `cargo bench` and
`cargo test` must not run at the same time. `RUST_LOG=error` keeps the output
readable.
