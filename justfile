check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    sh scripts/test-receipt-contract.sh

check-all:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-features
    sh scripts/test-receipt-contract.sh

fmt:
    cargo fmt
