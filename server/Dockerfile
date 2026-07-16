FROM rust:latest AS builder

WORKDIR /app
COPY src/ ./src/
COPY Cargo.* ./

RUN apt-get update
RUN apt-get install -y protobuf-compiler musl-tools
RUN rustup target add x86_64-unknown-linux-musl
RUN CARGO_INCREMENTAL=0 cargo build --release --target x86_64-unknown-linux-musl
# Verify the binary is statically linked
RUN ldd target/x86_64-unknown-linux-musl/release/server || true

FROM gcr.io/distroless/static:latest
WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/server ./
EXPOSE 8080
CMD ["/app/server"]
