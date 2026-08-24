.PHONY: build web test test-install fmt clippy check run install-ctl install-let

# Build the web UI into crates/server/ui (embedded by the server) and then
# the whole workspace. Run `make web` alone to rebuild just the dashboard.
build: web
	cargo build --workspace

web:
	cd web && npm install && npm run build

test:
	cargo test --workspace

# End-to-end test of install.sh against a fake release served over localhost.
test-install:
	sh scripts/test-install.sh

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

check:
	cargo fmt --all --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test --workspace
	sh scripts/test-install.sh

run:
	cargo run -p velos-server

# Build the velosctl CLI in release mode and install it to ~/.cargo/bin.
install-ctl:
	cargo install --path crates/velosctl --force

# Build the veloslet worker daemon in release mode and install it to ~/.cargo/bin.
install-let:
	cargo install --path crates/veloslet --force
