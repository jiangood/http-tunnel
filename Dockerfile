FROM rust:bookworm as builder
WORKDIR /home/rust/src
COPY . .
ARG FEATURES
RUN cargo build --locked --release --features ${FEATURES:-default}
RUN mkdir -p build-out/
RUN cp target/release/http-tunnel build-out/



FROM gcr.io/distroless/cc-debian12
WORKDIR /app
COPY --from=builder /home/rust/src/build-out/http-tunnel .
USER 1000:1000
ENTRYPOINT ["./http-tunnel"]
