# Plexigraf

A pluggable, standards-based metrics agent. The core is a small Rust binary;
every receiver, processor, and exporter beyond a few built-ins is a sandboxed
WebAssembly component that targets a versioned WIT interface. See
[DESIGN.md](DESIGN.md) for the full design.

This tree implements phase 0 and most of phase 1 of the design's exploration
plan: the host, the config and pipeline model, delivery tracking, and SDKs
for Rust, Go, and Python, with the `random` example plugin in each language.

## Quick start

```sh
make          # build plexigraf and the three example plugins into examples/plugins/
make demo     # run the Rust, Go, and Python plugins once through one pipeline
make test     # unit tests plus end-to-end tests with real plugins
```

`make demo` prints one line per plugin and writes OTLP JSON to
`examples/random-all.jsonl`:

```text
rust,source=plexigraf int=21i,float=0.5853531656354315 1793570962649000000
go,source=plexigraf int=5i,float=0.37693630835985714 1793570962649012000
python,source=plexigraf int=1i,float=0.9517904533023209 1793570962649016000
```

Other commands:

```sh
plexigraf run --config examples/random-rust.toml       # run until Ctrl-C, then drain
plexigraf once --config examples/random-rust.toml      # one scrape per scraper, then exit
plexigraf config check --config examples/random-all.toml
plexigraf plugin inspect examples/plugins/random_py.wasm
```

## Requirements

| For | Needs |
| --- | --- |
| The agent and Rust plugins | Rust stable with `rustup target add wasm32-wasip2` |
| Go plugins | Go 1.27.1 or later. `componentize-go` is a `go tool` dependency of each module and downloads its own binary on first use |
| Python plugins | [`uv`](https://docs.astral.sh/uv/). The Makefile runs a pinned `componentize-py` through `uvx`, which bundles its own CPython |

## Development

Run `make setup` once after cloning. It installs the lint and test tools and
enables a git pre-commit hook that runs `make lint`.

```sh
make lint       # formatting, Clippy, docs, cargo-deny, unused deps, lockfiles, workflows
make fmt        # fix formatting
make test       # unit, property, snapshot, integration, and doc tests
make coverage   # line coverage report in your browser
make bench      # encoder benchmarks
make fuzz       # fuzz the line protocol decoder
```

Run `make lint` before every commit; the hook and CI both enforce it. The
Makefile header lists the other targets. [CONTRIBUTING.md](CONTRIBUTING.md)
covers versioning, dependencies, and releases, and [AGENTS.md](AGENTS.md) has
the lint and test conventions. Report vulnerabilities as described in
[SECURITY.md](SECURITY.md).

## Telemetry

Plexigraf sends nothing anywhere you did not configure. Its own metrics (see
"Service telemetry" below) are served only on the endpoint you set, and
panics are written to its log. There is no crash reporting service and no
usage tracking.

## Layout

| Path | What it is |
| --- | --- |
| `wit/` | The plugin contract: `plexigraf:metrics` (the OTel data model and the `batch` resource) and `plexigraf:plugin` (host services and one world per plugin kind) |
| `crates/plexigraf/` | The agent: config, plugin host, pipeline runtime, encodings, CLI |
| `crates/plexigraf/tests/plugins/` | Test plugins: `chaos` (misbehaves on purpose), `rename` (processor), `sink` (exporter) |
| `sdk/rust/` | `plexigraf-sdk` crate: `Scraper`, `Processor`, and `Exporter` traits, a `Metric` builder, and export macros |
| `sdk/go/` | `plexigraf-sdk-go` module: plugin types plus one package per world (`scraper`, `processor`, `exporter`). `generate.sh` regenerates the bindings |
| `sdk/python/` | `plexigraf_sdk` package: `Scraper`, `Processor`, and `Exporter` base classes and `register` |
| `examples/random-{rust,go,python}/` | The design's `random` scraper in each language |
| `examples/*.toml` | Example configs |

## What works

- **Config.** `[plugins]`, components by kind, `[pipelines.metrics.<name>]`, and
  `[service]`. Multiple files merge into one namespace (`--config` repeated or
  `--config-dir`), and a duplicate ID is an error that names both files.
  `${env:VAR}`, `${VAR}`, and `${VAR:-default}` are substituted after parsing,
  inside strings only. Pipeline receivers accept IDs, globs, and
  `{ routing_labels = {...}, match = "...", allow_no_match = true }`. The
  loader checks unknown references, one-sided connectors, and connector
  cycles. A config with no pipelines fails with an error that says so, and a
  Telegraf `[[inputs.*]]` table gets an error naming the Plexigraf equivalent.
- **Plugin host.** Wasmtime 50 with the component model. The world is inferred
  from a component's exports, or set in its manifest. Each instance runs on
  its own thread with its own `Store`.
- **Sandbox.** Plugins get clocks and randomness only: no files, env, network,
  or stdio. Limits cover memory (64 MiB by default, settable in the manifest or
  `[plugins.X]`), per-call epoch deadlines (`timeout`), live batch handles,
  metrics per batch, state size, and `any-value` tree size and depth. Every
  batch a plugin returns is validated: indices in range, trees acyclic with
  one parent per node, and trace and span ID lengths. A plugin whose manifest
  requests any capability is refused, because grants are not implemented
  yet. It fails closed.
- **Failure isolation.** A trap or overrun rebuilds the instance with
  exponential backoff. A `fatal` error stops the component. A component that
  fails to start is isolated and reported, unless you pass `--strict`. The
  plugin's stderr is captured and attached to crash reports.
- **Pipelines.** One task per pipeline, with copy-on-write fan-out and one
  processor instance per pipeline. Processors that set `flush = true` are
  flushed on a timer and once more on drain. Each exporter has one shared
  sending queue with `block`, `drop_oldest`, or `drop_newest`, plus retry with
  backoff on `retry` and transient errors. `acknowledge` can be `exported` or
  `enqueued`.
- **Delivery tracking.** Every batch carries a tracker through fan-out,
  processors, connectors, and queues. It resolves to `delivered`, `partial`,
  or `failed` when its last copy finishes.
- **Built-ins.** The `debug` and `file` exporters, the `forward` connector,
  the `health_check` extension, the `influx` encoding (encode and decode,
  including `multi_field_lines`, `counter_fields`, and `string_values`), and
  the `json` encoding (OTLP JSON, encode only). Plugins reach the same
  encodings through `plexigraf:plugin/encoding`.
- **Service telemetry.** `plexigraf.receiver.accepted_metric_points`,
  `plexigraf.exporter.sent_metric_points`, `plexigraf.exporter.send_failed_metric_points`,
  `plexigraf.conversion.lossy`, and others, served as Prometheus text at
  `[service.telemetry] metrics.endpoint`.

## Not yet

Each of these is either rejected with a clear error or simply absent:

- The push `receiver` world, async exports, and `consume-metrics`.
- Capabilities and grants, `plexigraf.lock`, OCI sources, signatures, and the
  default registry. Only `file://` sources work.
- The `otlp` receiver and exporter, protobuf OTLP, and the `prometheus`
  encoding.
- The `filter` and `transform` processors with OTTL, the `routing`
  connector, and dead-letter edges.
- `file_storage` and disk queues, `per_pipeline` queues, and `share_instance`.
- Reload on SIGHUP, the compiled-module cache, secret stores, and `plexigraf-privd`.
- The plugin-author CLI (`plexigraf plugin new`, `build`, `pack`, and `sign`) and
  `plexigraf config migrate`.

## Differences from DESIGN.md

These came out of building against the real toolchains.

1. **WIT fixes.** `resource` and `export` are WIT keywords, so the record,
   the field, and the exporter's function are spelled `%resource` and
   `%export`. The two packages live in separate files (`wit/plugin.wit` and
   `wit/deps/metrics/metrics.wit`), which every binding generator accepts.
2. **Lifecycle moved into an interface.** `validate-config`, `start`, and
   `shutdown` are exported through `plexigraf:plugin/lifecycle` instead of at world
   level. componentize-py cannot link a component whose world exports
   `shutdown` or `close`, because the name collides with libc's symbols.
   Interface exports are namespaced, so this class of collision cannot recur.
3. **Sync worlds for 0.1.** The design's open question about sync twins is
   settled for now: every export is a plain `func`. This lets Rust, Go, and
   Python share one set of worlds today. The host still runs each instance
   off the async runtime, so a slow plugin cannot stall others. The SDK
   traits are therefore synchronous (`fn scrape`, not `async fn scrape`).
4. **Go registration is per world.** Go cannot link wasm exports
   conditionally, so a single `plexigraf.Register` that infers the kind from
   methods is not possible. Plugins call `scraper.Register(&Random{...})`, and
   importing the `scraper` package selects the world. The values passed to
   `Register` are the defaults that config overrides.
5. **A `validate` hook in every SDK.** `plexigraf config check` calls
   `validate-config` without starting the plugin, so a check placed in `start`
   (as in the design's `random` example) would never run there. Each SDK gives
   plugins an optional `validate` that runs on check and before start.
6. **Measured sizes.** For the `random` plugin: Rust 236 KB, Go 3.8 MB, and
   Python 19 MB. The Python build needs a 256 MiB memory limit, which its
   manifest sets. Cold start in a release build is about 1.3 s for all three,
   most of it spent compiling the Python component.
