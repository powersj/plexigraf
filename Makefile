# Build the agent and the example plugins.
#
#   make            build tern and all three example plugins
#   make demo       run all three plugins once through one pipeline
#   make test       run the core's unit and integration tests
#   make lint       formatting, clippy, docs, dependency checks; run before every commit
#   make fmt        reformat every Rust crate in place
#   make coverage   test coverage report, opened in a browser
#   make setup      install the lint tools and the git pre-commit hook

PLUGINS := examples/plugins
WIT     := $(CURDIR)/wit
COMPONENTIZE_PY := uvx componentize-py==0.25.1

# Rust crates outside the workspace: they build for wasm32-wasip2. The SDK is
# linted once per world, since each feature compiles different code.
WASM_CRATES := examples/random-rust crates/tern/tests/plugins/chaos crates/tern/tests/plugins/rename crates/tern/tests/plugins/sink
SDK_FEATURES := scraper processor exporter
LINT_TOOLS := cargo-nextest cargo-llvm-cov cargo-deny cargo-machete cargo-insta

.PHONY: all tern plugins rust go python demo test lint fmt fmt-check clippy doc deny machete coverage setup clean

all: tern plugins

tern:
	cargo build --release -p tern

plugins: rust go python

$(PLUGINS):
	mkdir -p $@

rust: | $(PLUGINS)
	cd examples/random-rust && cargo build --target wasm32-wasip2 --release
	cp examples/random-rust/target/wasm32-wasip2/release/tern_scraper_random.wasm $(PLUGINS)/random_rust.wasm
	cp examples/random-rust/tern-plugin.toml $(PLUGINS)/random_rust.tern-plugin.toml

go: | $(PLUGINS)
	cd examples/random-go && go tool componentize-go -d $(WIT) -w tern:plugin/scraper --ignore-toml-files build -o random_go.wasm
	cp examples/random-go/random_go.wasm $(PLUGINS)/random_go.wasm
	cp examples/random-go/tern-plugin.toml $(PLUGINS)/random_go.tern-plugin.toml

python: | $(PLUGINS)
	cd examples/random-python && $(COMPONENTIZE_PY) -d $(WIT) -w tern:plugin/scraper componentize random_scraper -p . -p ../../sdk/python -o random_py.wasm
	cp examples/random-python/random_py.wasm $(PLUGINS)/random_py.wasm
	cp examples/random-python/tern-plugin.toml $(PLUGINS)/random_py.tern-plugin.toml

demo: all
	./target/release/tern once --config examples/random-all.toml

test:
	cargo nextest run -p tern

lint: fmt-check clippy doc deny machete

fmt:
	cargo fmt --all
	cargo fmt --manifest-path sdk/rust/Cargo.toml
	for c in $(WASM_CRATES); do cargo fmt --manifest-path $$c/Cargo.toml || exit 1; done

fmt-check:
	cargo fmt --all --check
	cargo fmt --manifest-path sdk/rust/Cargo.toml --check
	for c in $(WASM_CRATES); do cargo fmt --manifest-path $$c/Cargo.toml --check || exit 1; done

clippy:
	cargo clippy --workspace --all-targets -- -D warnings
	for f in $(SDK_FEATURES); do \
		cargo clippy --manifest-path sdk/rust/Cargo.toml --target wasm32-wasip2 --all-targets --features $$f -- -D warnings || exit 1; \
	done
	for c in $(WASM_CRATES); do \
		cargo clippy --manifest-path $$c/Cargo.toml --target wasm32-wasip2 --all-targets -- -D warnings || exit 1; \
	done

# Warnings in doc comments, such as broken links, fail the build.
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
	RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path sdk/rust/Cargo.toml --target wasm32-wasip2 --features scraper --no-deps

deny:
	cargo deny check
	cargo deny --manifest-path sdk/rust/Cargo.toml check

machete:
	cargo machete

coverage:
	cargo llvm-cov nextest -p tern --html --open

setup:
	rustup target add wasm32-wasip2
	rustup component add clippy rustfmt llvm-tools-preview
	cargo install --locked $(LINT_TOOLS)
	git config core.hooksPath .githooks

clean:
	cargo clean
	rm -rf $(PLUGINS) examples/random-rust/target examples/random-go/random_go.wasm examples/random-python/random_py.wasm
