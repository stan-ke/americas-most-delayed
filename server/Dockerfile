FROM rust:latest@sha256:c92b3414e418f5b250f13c5bd393f90192f0b4bb4767e64f29c9e583197b27cb AS builder

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
FROM alpine:latest@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6 AS runtime
WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/server /app/server
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=2m --retries=3 \
  CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/app/server"]
