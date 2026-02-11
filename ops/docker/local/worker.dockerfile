# Build stage
FROM rust:1.85-slim AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src

# Build worker (specify package since we have workspace with root crate)
RUN cargo build --release -p browser-hive-worker --bin worker

# Runtime stage with Chromium
FROM debian:bookworm-slim

# Install Chromium and dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    chromium \
    chromium-driver \
    # Required libraries for Chromium
    libnss3 \
    libxss1 \
    libasound2 \
    libatk-bridge2.0-0 \
    libatk1.0-0 \
    libcups2 \
    libdbus-1-3 \
    libdrm2 \
    libgbm1 \
    libgtk-3-0 \
    libnspr4 \
    libx11-xcb1 \
    libxcomposite1 \
    libxdamage1 \
    libxfixes3 \
    libxrandr2 \
    fonts-liberation \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /app/target/release/worker /usr/local/bin/worker

# Create directory for Chrome data
RUN mkdir -p /chrome-data && chmod 777 /chrome-data

EXPOSE 50052 9090

CMD ["worker"]
