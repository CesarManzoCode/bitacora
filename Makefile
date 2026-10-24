.PHONY: check criteria ublk

# Single check command (kiln check_command and CI). Before the Cargo workspace
# exists (K0) there is no product to check.
check:
	@if [ -f Cargo.toml ]; then \
		cargo fmt --all --check && \
		cargo clippy --workspace --all-targets --locked -- -D warnings && \
		cargo test --workspace --locked; \
	else \
		echo "check: no Cargo workspace yet"; \
	fi

# C23: build and run every criterion possible without root.
criteria:
	cargo build --release --workspace --locked
	./target/release/bitacora-verify all

# Binary with the ublk frontend (needs libclang), used by C19 and C20.
ublk:
	cargo build --release -p bitacora --features ublk --locked --target-dir target/ublk
