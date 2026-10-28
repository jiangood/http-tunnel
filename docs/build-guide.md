# Build Guide

This is for those who want to build `http-tunnel` themselves, possibly because the need of latest features or the minimal binary size.

## Build

To use default build settings, run:

```sh
cargo build --release
```

## Customize the Build

`http-tunnel` comes with some *crate features* that determine whether a certain feature will be compiled or not. Supported features can be checked out in `[features]` of [Cargo.toml](../Cargo.toml).

For example, to build `http-tunnel` as a client only:

```sh
cargo build --release --no-default-features --features client
```

The `node` feature builds the `htn` binary (one process per relay hop). It
enables both the `server` and the `client` roles, because a node embeds both:

```sh
cargo build --release --no-default-features --features node
```

## Minimalize the binary

1. Build with the `minimal` profile

The `release` build profile optimize for the program running time, not the binary size.

However, the `minimal` profile enables lots of optimization for the binary size to produce a much smaller binary.

For example, to build `http-tunnel` with `client` feature with the `minimal` profile:

```sh
cargo build --profile minimal --no-default-features --features client
```

2. `strip` and `upx`

The binary that step 1 produces can be even smaller, by using `strip` and `upx` to remove the symbols and compress the binary.

Like:

```sh
strip http-tunnel
upx --best --lzma http-tunnel
```

## Benchmarking

The HTTP path has a small benchmark harness in [`benches/`](../benches). It
starts an echo backend behind the NAT, a server and a client, then measures three
scenarios: many small keep-alive requests, many short-lived connections (the
per-connection setup cost, where stream setup shows up), and several
1 MiB bodies in flight at once (the large-transfer throughput).

```sh
cargo bench --bench throughput
```

It uses the same ports as the integration tests, so `cargo bench` and
`cargo test` must not run at the same time. `RUST_LOG=error` keeps the output
readable.
