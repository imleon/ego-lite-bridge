# ego-lite-bridge task runner

fmt:
    cargo fmt --check

lint:
    cargo clippy --all-targets --locked -- -D warnings

test:
    cargo test --locked

check: fmt lint test installer-test release-test

build:
    cargo build --release --locked

installer-test:
    python3 -m unittest scripts.test_unix_installer

release-test:
    python3 -m unittest scripts.test_prepare_release

e2e-manual:
    python3 -m unittest scripts.test_manual_e2e
