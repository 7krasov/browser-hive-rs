.PHONY: help up down restart logs rebuild-coordinator rebuild-worker test clean build build-proto build-all build-coordinator build-worker check fmt fmt-check clippy

# Default target - show help
help:
	@echo "Browser Hive - Makefile commands:"
	@echo ""
	@echo "Docker commands:"
	@echo "  make up                 - Start all services (coordinator + worker)"
	@echo "  make down               - Stop all services"
	@echo "  make restart            - Restart all services"
	@echo "  make logs               - Show logs from all services"
	@echo "  make logs-coordinator   - Show coordinator logs"
	@echo "  make logs-worker        - Show worker logs"
	@echo ""
	@echo "  make rebuild-coordinator - Rebuild and restart coordinator"
	@echo "  make rebuild-worker      - Rebuild and restart worker"
	@echo ""
	@echo "Build commands (native):"
	@echo "  make build              - Build all crates"
	@echo "  make build-proto        - Build proto crate (regenerate proto files)"
	@echo "  make build-coordinator  - Build coordinator binary"
	@echo "  make build-worker       - Build worker binary"
	@echo "  make build-all          - Build all crates in release mode"
	@echo ""
	@echo "Code quality:"
	@echo "  make check              - Check code without building"
	@echo "  make fmt                - Format code with rustfmt"
	@echo "  make clippy             - Run clippy linter"
	@echo ""
	@echo "Testing:"
	@echo "  make test               - Make test scraping request"
	@echo "  make test-worker        - Make test request directly to worker"
	@echo ""
	@echo "  make clean              - Stop services and remove volumes"

# Start all services
up:
	docker-compose up -d

# Stop all services
down:
	docker-compose down

# Restart all services
restart:
	docker-compose restart

# Show logs from all services
logs:
	docker-compose logs -f

# Show coordinator logs
logs-coordinator:
	docker-compose logs -f coordinator

# Show worker logs
logs-worker:
	docker-compose logs -f worker

# Rebuild and restart coordinator
rebuild-coordinator:
	@echo "Rebuilding coordinator..."
	docker-compose up -d --build --force-recreate coordinator
	@echo "Coordinator rebuilt and restarted"

# Rebuild and restart worker
rebuild-worker:
	@echo "Rebuilding worker..."
	docker-compose up -d --build --force-recreate worker
	@echo "Worker rebuilt and restarted"

# Make test scraping request via coordinator
test:
	@echo "Making test request to coordinator..."
	@grpcurl -plaintext \
		-import-path ./crates/proto/proto \
		-proto coordinator.proto \
		-d '{"scope_name": "local_dev", "url": "https://httpbin.org/ip", "timeout_seconds": 30}' \
		localhost:50051 \
		scraper.coordinator.ScraperCoordinator/ScrapePage

# Make test request directly to worker
test-worker:
	@echo "Making test request to worker..."
	@grpcurl -plaintext \
		-import-path ./crates/proto/proto \
		-proto worker.proto \
		-d '{"url": "https://example.com"}' \
		localhost:50052 \
		scraper.worker.WorkerService/ScrapePage

# Stop services and remove volumes
clean:
	docker-compose down -v

# Build commands (native Rust compilation)

# Build all crates in debug mode
build:
	@echo "Building all crates..."
	cargo build

# Build proto crate (regenerate proto files from .proto definitions)
build-proto:
	@echo "Building proto crate and regenerating proto files..."
	cargo build -p browser-hive-proto

# Build coordinator binary
build-coordinator:
	@echo "Building coordinator..."
	cargo build -p browser-hive-coordinator --bin coordinator

# Build worker binary
build-worker:
	@echo "Building worker..."
	cargo build -p browser-hive-worker --bin worker

# Build all crates in release mode (optimized)
build-all:
	@echo "Building all crates in release mode..."
	cargo build --release

# Code quality commands

# Check code without building (faster than build)
check:
	@echo "Checking code..."
	cargo check

# Format code with rustfmt
fmt:
	@echo "Formatting code..."
	cargo fmt

# Check formatting without modifying files
fmt-check:
	@echo "Checking code formatting..."
	cargo fmt --check

# Run clippy linter
clippy:
	@echo "Running clippy..."
	cargo clippy --all-targets --all-features
