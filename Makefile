# Build the agent and the example plugins.
#
#   make            build plexigraf and all three example plugins
#   make demo       run all three plugins once through one pipeline
#   make test       unit, integration, property, snapshot, and doc tests
#   make lint       formatting, clippy, docs, dependency and workflow checks; run before every commit
#   make fmt        reformat every Rust crate in place
#   make coverage   test coverage report, opened in a browser
#   make bench      encoder benchmarks
#   make fuzz       fuzz the line protocol decoder (FUZZ_SECONDS, default 300)
#   make msrv       check the agent and SDK build on their minimum Rust versions
#   make public-api regenerate sdk/rust/public-api/*.txt after an SDK API change
#   make semver     check plexigraf-sdk for breaking changes against BASELINE (default main)
#   make size       release build, failing if the binary is over its size budget
#   make health     numbers for the quarterly health review in CONTRIBUTING.md
#   make update     update every lockfile to the newest compatible versions
#   make changelog  regenerate CHANGELOG.md from git history
#   make setup      install the toolchains, tools, and git pre-commit hook

PLUGINS := examples/plugins
WIT     := $(CURDIR)/wit
COMPONENTIZE_PY := uvx componentize-py==0.25.1
ZIZMOR := uvx zizmor==1.31.1

# Rust crates outside the workspace: they build for wasm32-wasip2. The SDK is
# linted once per world, since each feature compiles different code.
WASM_CRATES := examples/random-rust crates/plexigraf/tests/plugins/chaos crates/plexigraf/tests/plugins/rename crates/plexigraf/tests/plugins/sink
SDK_FEATURES := scraper processor exporter
FUZZ := crates/plexigraf/fuzz
# Every manifest with its own lockfile.
MANIFESTS := . sdk/rust $(WASM_CRATES) $(FUZZ)
LINT_TOOLS := cargo-nextest cargo-llvm-cov cargo-deny cargo-machete cargo-insta \
	cargo-fuzz cargo-public-api cargo-semver-checks git-cliff

AGENT_MSRV := $(shell sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)
SDK_MSRV := $(shell sed -n 's/^rust-version = "\(.*\)"/\1/p' sdk/rust/Cargo.toml)
BASELINE ?= main
FUZZ_SECONDS ?= 300
# Release binary budget. Measured 14.1 MB on macOS arm64 with the release
# profile in Cargo.toml; raise it deliberately, in the PR that needs it.
SIZE_BUDGET_MB ?= 18

.PHONY: all plexigraf plugins rust go python demo test lint fmt fmt-check clippy doc deny machete lockfiles \
	workflows coverage bench fuzz msrv public-api semver size health update changelog setup clean

all: plexigraf plugins

plexigraf:
	cargo build --release -p plexigraf

plugins: rust go python

$(PLUGINS):
	mkdir -p $@

rust: | $(PLUGINS)
	cd examples/random-rust && cargo build --target wasm32-wasip2 --release
	cp examples/random-rust/target/wasm32-wasip2/release/plexigraf_scraper_random.wasm $(PLUGINS)/random_rust.wasm
	cp examples/random-rust/plexigraf-plugin.toml $(PLUGINS)/random_rust.plexigraf-plugin.toml

go: | $(PLUGINS)
	cd examples/random-go && go tool componentize-go -d $(WIT) -w plexigraf:plugin/scraper --ignore-toml-files build -o random_go.wasm
	cp examples/random-go/random_go.wasm $(PLUGINS)/random_go.wasm
	cp examples/random-go/plexigraf-plugin.toml $(PLUGINS)/random_go.plexigraf-plugin.toml

python: | $(PLUGINS)
	cd examples/random-python && $(COMPONENTIZE_PY) -d $(WIT) -w plexigraf:plugin/scraper componentize random_scraper -p . -p ../../sdk/python -o random_py.wasm
	cp examples/random-python/random_py.wasm $(PLUGINS)/random_py.wasm
	cp examples/random-python/plexigraf-plugin.toml $(PLUGINS)/random_py.plexigraf-plugin.toml

demo: all
	./target/release/plexigraf once --config examples/random-all.toml

# nextest does not run doctests, so the SDK's runs separately.
test:
	cargo nextest run -p plexigraf
	cargo test --doc --manifest-path sdk/rust/Cargo.toml --features scraper

lint: lockfiles fmt-check clippy doc deny machete workflows

fmt:
	cargo fmt --all
	for c in sdk/rust $(WASM_CRATES) $(FUZZ); do cargo fmt --manifest-path $$c/Cargo.toml || exit 1; done

fmt-check:
	cargo fmt --all --check
	for c in sdk/rust $(WASM_CRATES) $(FUZZ); do cargo fmt --manifest-path $$c/Cargo.toml --check || exit 1; done

clippy:
	cargo clippy --workspace --all-targets -- -D warnings
	for f in $(SDK_FEATURES); do \
		cargo clippy --manifest-path sdk/rust/Cargo.toml --target wasm32-wasip2 --all-targets --features $$f -- -D warnings || exit 1; \
	done
	for c in $(WASM_CRATES); do \
		cargo clippy --manifest-path $$c/Cargo.toml --target wasm32-wasip2 --all-targets -- -D warnings || exit 1; \
	done
	cargo clippy --manifest-path $(FUZZ)/Cargo.toml -- -D warnings

# Warnings in doc comments, such as broken links, fail the build.
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
	RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path sdk/rust/Cargo.toml --target wasm32-wasip2 --features scraper --no-deps

deny:
	cargo deny check
	cargo deny --manifest-path sdk/rust/Cargo.toml check

machete:
	cargo machete

# Fails if any Cargo.lock is out of date with its Cargo.toml, so builds use
# exactly the committed versions.
lockfiles:
	for m in $(MANIFESTS); do cargo metadata --locked --format-version 1 --manifest-path $$m/Cargo.toml >/dev/null || exit 1; done

workflows:
	$(ZIZMOR) --offline .github

coverage:
	cargo llvm-cov nextest -p plexigraf --html --open

bench:
	cargo bench -p plexigraf --bench encoding

fuzz:
	mkdir -p $(FUZZ)/corpus/influx_decode
	cd $(FUZZ) && cargo +nightly fuzz run influx_decode corpus/influx_decode seeds/influx_decode -- -max_total_time=$(FUZZ_SECONDS)

msrv:
	rustup toolchain install $(AGENT_MSRV) $(SDK_MSRV) --profile minimal -t wasm32-wasip2
	cargo +$(AGENT_MSRV) check -p plexigraf --locked
	for f in $(SDK_FEATURES); do \
		cargo +$(SDK_MSRV) check --manifest-path sdk/rust/Cargo.toml --target wasm32-wasip2 --features $$f || exit 1; \
	done

public-api:
	for f in $(SDK_FEATURES); do \
		cargo +nightly public-api --manifest-path sdk/rust/Cargo.toml --features $$f -ss > sdk/rust/public-api/$$f.txt || exit 1; \
	done

semver:
	for f in $(SDK_FEATURES); do \
		cargo semver-checks --manifest-path sdk/rust/Cargo.toml --baseline-rev $(BASELINE) --only-explicit-features --features $$f || exit 1; \
	done

size: plexigraf
	@bytes=$$(wc -c < target/release/plexigraf); budget=$$(( $(SIZE_BUDGET_MB) * 1000000 )); \
	echo "target/release/plexigraf: $$bytes bytes (budget $$budget)"; \
	test $$bytes -le $$budget || { echo "over the $(SIZE_BUDGET_MB) MB size budget" >&2; exit 1; }

health: size
	@echo "direct dependencies: $$(cargo tree -p plexigraf --depth 1 -e normal --prefix none | tail -n +2 | wc -l)"
	@echo "all dependencies:    $$(cargo tree -p plexigraf -e normal --prefix none | sort -u | wc -l)"
	cargo deny check advisories
	@echo "For compile time, run: cargo build --release -p plexigraf --timings"

update:
	for m in $(MANIFESTS); do cargo update --manifest-path $$m/Cargo.toml || exit 1; done

changelog:
	git-cliff -o CHANGELOG.md

setup:
	rustup show active-toolchain
	rustup toolchain install nightly --profile minimal
	cargo install --locked $(LINT_TOOLS)
	git config core.hooksPath .githooks

clean:
	cargo clean
	rm -rf $(PLUGINS) examples/random-rust/target examples/random-go/random_go.wasm examples/random-python/random_py.wasm
