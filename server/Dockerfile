FROM rust:latest@sha256:1f65e0abcce9b449f9533df5994f2a62391059196c481a09b8e1b522dd6d75af AS builder

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
FROM alpine:latest@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf AS runtime
WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/server /app/server
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=2m --retries=3 \
  CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/app/server"]
