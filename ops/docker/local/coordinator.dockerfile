# Build stage
FROM rust:1.85-slim AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace files (context is repo root via docker-compose)
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src

# Build coordinator (specify package since we have workspace with root crate)
RUN cargo build --release -p browser-hive-coordinator --bin coordinator

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /app/target/release/coordinator /usr/local/bin/coordinator

EXPOSE 50051

CMD ["coordinator"]
