FROM rust:latest@sha256:271849e998ffce5776454bbf98c5dc21baafc854ff8e566197908d3aca9a81e8 AS builder

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
FROM alpine:latest@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b AS runtime
WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/server /app/server
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=2m --retries=3 \
  CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/app/server"]
