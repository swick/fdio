# SPDX-License-Identifier: MIT OR Apache-2.0
# SPDX-FileCopyrightText: Copyright © the fdio contributors

# CI runs: make check && make test
#
# Local targets:
#   make check            - fmt + clippy + doc warnings (same as CI)
#   make test             - run all tests
#   make msrv             - verify build with minimum supported Rust version
#                           (requires: rustup toolchain install 1.77)
#   make minimal-versions - verify build with minimum dependency versions
#                           (requires: cargo install cargo-minimal-versions cargo-hack)
#   make coverage-report  - generate HTML coverage and serve on localhost:8080
#                           (requires: cargo install cargo-llvm-cov)

.PHONY: check fmt clippy doc reuse test msrv minimal-versions coverage coverage-report

check: fmt clippy doc reuse

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

reuse:
	reuse lint

test:
	cargo test --workspace --all-features

msrv:
	cargo +1.77 check --workspace --all-features

minimal-versions:
	cargo minimal-versions check --workspace --all-features --direct

coverage:
	cargo llvm-cov --workspace --all-features --lcov --output-path target/lcov.info

coverage-report:
	cargo llvm-cov --workspace --all-features --html
	python3 -m http.server -d target/llvm-cov/html 8080
