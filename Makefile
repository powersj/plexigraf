# Build the agent and the example plugins.
#
#   make            build tern and all three example plugins
#   make demo       run all three plugins once through one pipeline
#   make test       run the core's unit and integration tests

PLUGINS := examples/plugins
WIT     := $(CURDIR)/wit
COMPONENTIZE_PY := uvx componentize-py==0.25.1

.PHONY: all tern plugins rust go python demo test clean

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
	cargo test -p tern

clean:
	cargo clean
	rm -rf $(PLUGINS) examples/random-rust/target examples/random-go/random_go.wasm examples/random-python/random_py.wasm
