FROM rust:latest@sha256:4c4c8ec55be09785e6d6ac45ea9853525a73c15db74009100ac08952e773dfb8 AS builder

WORKDIR /app
COPY src/ ./src/
COPY Cargo.* ./

RUN apt-get update
RUN apt-get install -y protobuf-compiler musl-tools
RUN rustup target add x86_64-unknown-linux-musl
RUN CARGO_INCREMENTAL=0 cargo build --release --target x86_64-unknown-linux-musl
# Verify the binary is statically linked
RUN ldd target/x86_64-unknown-linux-musl/release/server || true

# FROM gcr.io/distroless/static:latest
# WORKDIR /app
# COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/server ./
# EXPOSE 8080
# CMD ["/app/server"]

# Go alpine instead for healthcheck
FROM alpine:latest@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a AS runtime
WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/server /app/server
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=2m --retries=3 \
  CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/app/server"]
