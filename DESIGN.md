# Plexigraf: A Pluggable, Standards-Based Metrics Agent

Nov 1, 2026 · @Joshua Powers

## Summary

Recommendation: build a small Rust agent (**Plexigraf**) whose core carries only a few built-in components and loads every other receiver, processor, and exporter as a sandboxed WebAssembly component. Each plugin declares the capabilities it needs in a manifest. The operator grants them in config. Anything that truly needs root runs in a separate, narrow helper process, not in the agent.

Telegraf today has four structural problems this design targets:

| Problem in Go Telegraf | Root cause | How the Rust agent resolves it |
| --- | --- | --- |
| One binary pulls in about 270 direct and 360 indirect Go modules | All 250+ inputs and 70+ outputs compile into the binary through `plugins/*/all` imports | The core binary carries only the runtime, config, scheduler, and metric pipeline. A plugin's dependencies live inside its own `.wasm` file |
| The whole process runs with the privileges of its most demanding plugin | Plugins share one address space and one OS identity | Plugins get no ambient authority. Each gets only the files, sockets, hosts, and env vars granted to it. Privileged reads go through a separate helper |
| Plugins are not pluggable | Plugins register through Go `init()` and need a rebuild; `execd` is the only escape hatch and it has no sandbox or typed API | A stable, versioned plugin interface (WIT). Anyone builds a plugin in any language that targets the WebAssembly Component Model and drops it in |
| Routing specific inputs to specific outputs is hard with dozens of inputs | Every input flows to every processor and every output; routes are carved out with `namepass`, `tagpass`, and `order` scattered across plugins | No implicit flow. The user declares named pipelines (from, through, to), and nothing moves unless a pipeline says so |

Goals for the first version:

1. The core binary has no plugin code and a dependency tree small enough to audit by hand.
2. A third party can write, sign, and ship a plugin without touching the agent repo.
3. The agent runs as an unprivileged user by default, and each plugin's access is explicit, reviewable, and denied by default.
4. Plugin authors get a typed SDK so a simple receiver is about 50 lines of Rust.

**Standards first.** Where an open standard exists, Plexigraf uses it instead of inventing its own format: the OpenTelemetry data model and semantic conventions for metrics, OTLP as the native wire format, the WebAssembly Component Model and WIT for plugins, OCI and Sigstore for distribution, and TOML for config. Telegraf formats such as line protocol are supported at the edges for compatibility, never as the internal model.

## Background

Telegraf's plugin contract is good and worth keeping; its packaging and trust model are what need to change.

**Telegraf today.** Plugins implement small Go interfaces: `Input.Gather(Accumulator)`, `ServiceInput.Start/Stop`, `Output.Connect/Write/Close`, plus optional `Init`, `SampleConfig`, `GetState/SetState`, and `Probe`. Each plugin registers itself in a global map from an `init()` function, and `plugins/inputs/all` imports every one. The tree has about 250 inputs, 70 outputs, 40 processors, 10 aggregators, 25 parsers, 17 serializers, and 9 secret stores. Of the inputs, about 30 READMEs say they need root, sudo, or extra capabilities (for example `smart`, `iptables`, `ipmi_sensor`, `nftables`, `turbostat`, `wireguard`).

Telegraf has already tried to soften each problem:

- `tools/custom_builder` builds a binary with only the plugins a config uses. It shrinks the binary but still needs a Go toolchain and a rebuild per config.
- `inputs.execd`, `processors.execd`, and `outputs.execd`, with `plugins/common/shim`, run an external process that speaks line protocol over stdin and stdout. This is the only true out-of-tree path. It has no typed config, no sandbox, and no capability model, and the child inherits the agent's privileges.
- Docs suggest `setcap` or sudoers entries per plugin, but these grant the capability to the whole Telegraf binary.

**Spawn.** The `../spawn` repo shows the shape we want for the core. It has one agent loop (`agent.Run`: a ticker that calls each serializer, which calls generators and fans out to outputs), a TOML config with `[[generator.X]]`, `[[serializer.X]]`, and `[[output.X]]` tables, and per-kind registries that decode config into a plugin struct with `mapstructure`. Its direct dependencies are four modules. The lesson: the agent itself is small. Plugins are what make the dependency tree big.

What to carry over:

1. Small, single-purpose plugin kinds, under the OpenTelemetry Collector's names (receivers, processors, exporters), but not Telegraf's metric model, which gives way to OpenTelemetry's.
2. Telegraf's per-plugin TOML options, so existing plugin settings translate with a script. Telegraf's all-to-all routing is deliberately not carried over.
3. Spawn's per-kind registry, but filled at runtime from plugin files instead of at compile time.
4. Line protocol as the interchange format for `execd`-style compatibility.

**Terminology.** Plexigraf uses OpenTelemetry names throughout, not Telegraf's. For readers coming from Telegraf:

| Telegraf | Plexigraf (OpenTelemetry) |
| --- | --- |
| Input | Receiver; a receiver that polls on an interval is a scraper |
| Output | Exporter |
| Aggregator | Processor that implements `tick` |
| Parser, serializer, `data_format` | Encoding |
| Measurement with fields | One metric per value, named `<namespace>.<value>` |
| Tags | Data point attributes and resource attributes |
| `interval` | `collection_interval` |
| `Gather` | `scrape` |
| `Connect`, `Write`, `Close` | `start`, `export`, `shutdown` |
| `namepass`, `tagpass`, `fieldpass` | `include` and `exclude` with `metric_names` and `attributes` |
| Output buffer | Sending queue |
| `[agent]` | `[service]` |
| `internal` input | Service telemetry |

## Plugin runtime options

Use WebAssembly components on Wasmtime as the default runtime, and keep a subprocess runtime for plugins that cannot run in a sandbox yet.

| Option | Isolation | Language reach | Typed API | Dependency cost to core | Main drawback |
| --- | --- | --- | --- | --- | --- |
| **Wasm components (Wasmtime + WASI 0.3)** | Strong: memory sandbox, no ambient authority, per-plugin CPU and memory limits | Rust, Go (TinyGo and Go `wasip2`), C, Python, JS | Yes, WIT generates bindings both sides | One runtime crate tree (`wasmtime`, `wasmtime-wasi`) | Some syscalls (raw sockets, ioctl, netlink) are not in WASI |
| **Subprocess + gRPC or stdio** (like `execd`, HashiCorp go-plugin) | OS process boundary; sandbox only if we add seccomp, Landlock, or sandbox-exec | Any language | Yes with protobuf | `tonic` and `prost`, or nothing for stdio | Heavier per plugin; we must build the sandbox ourselves per OS |
| **Native dylib** (`libloading`, `abi_stable`) | None: shares the agent's address space and privileges | Rust, C | Partly; Rust has no stable ABI | Small | Brings back problem 2 and adds crash risk; ruled out |
| **Embedded scripting** (Lua, Rhai, Starlark) | Interpreter sandbox | One language | Loose | Small | Fine for processors, too slow and too narrow for receivers and exporters |

Why Wasm components:

- [WASI 0.3](https://bytecodealliance.org/articles/WASI-0.3) was ratified on June 11, 2026. It adds native `async func`, `stream<T>`, and `future<T>` to the component model, so a plugin can be long-running (a listener, a consumer) without the 0.2 poll-and-subscribe workarounds. Wasmtime 46 enables it by default.
- Capabilities are explicit by construction. A component can only open files under preopened directories and reach hosts the host lets through. That maps directly to a permission manifest.
- One plugin binary runs on Linux, macOS, Windows, x86 and ARM. No per-platform plugin builds.
- Each plugin carries its own dependencies inside its `.wasm`. The core never links them.

Prior art and its lessons:

- [Fluent Bit's WASM filter](https://docs.fluentbit.io/manual/pipeline/filters/wasm) supports Wasm filters and inputs with `Accessible_Paths` per plugin. It proves the model works for telemetry. It uses raw core Wasm with a C ABI, not components, so the API is untyped.
- [Vector removed its wasm transform](https://vector.dev/highlights/2021-08-23-removing-wasm/) in 2021, citing weak user experience and poor performance. That predates the component model, WIT, and WASI 0.2. Our design has to avoid its mistakes: a typed SDK and batch-oriented calls so the host-guest boundary is crossed once per batch, not once per metric.

Native subprocess plugins are designed alongside Wasm and use the same WIT, but ship after 1.0; v1 keeps execd compatibility for existing programs. They cover vendor C SDKs, native Python and Go drivers, and existing `execd` programs. Both runtimes speak the same WIT interface, so plugin authors see one API; see Plugin runtimes and capability providers.

## Core agent architecture

The core is a single async Rust binary with four parts and only a few built-in components; every other receiver, processor, exporter, and extension is loaded at runtime into its own sandbox.

&#91;embedded content: Plexigraf architecture · core agent, plugin sandboxes, privileged helper\]

Plugins never talk to each other or to the OS directly. Metrics cross the sandbox boundary in batches; every file, socket, secret, or privileged read goes through the capability broker.

| Component | Responsibility | Built on |
| --- | --- | --- |
| Config + registry | Parse TOML, resolve each plugin's `source` against `plexigraf.lock`, check approved capabilities, build component instances | `serde`, `toml` |
| Scheduler | Call `scrape` on each scraper every `collection_interval` with jitter and a deadline, call `flush` on processors that declare it, and restart crashed instances with backoff | `tokio` |
| Metric pipeline | Run each pipeline and connector: bounded channels from receivers through processors to exporters and connectors, one sending queue per exporter, batching, retry, and delivery tracking | `tokio::sync::mpsc` |
| Plugin host | One Wasmtime `Store` per component instance; implements the host imports; enforces each component's approved capabilities on files, sockets, secrets, and provider calls; enforces memory and CPU limits | `wasmtime`, `wasmtime-wasi` |

Built into core, because nearly every deployment needs them and they add no heavy dependencies: the `otlp` receiver; the `otlp`, `debug` (console), and `file` exporters; the `forward` and `routing` connectors; the `filter` and `transform` processors, which take OTTL conditions and statements; the `health_check` and `file_storage` extensions; the `otlp`, `influx` (line protocol), `prometheus`, and `json` encodings; and the `env` and `file` secret stores.

**Extensions.** As in the OpenTelemetry Collector, extensions are components outside the data path, declared under `[extensions.X]` and enabled in `[service]`. `health_check` serves liveness and readiness endpoints for Kubernetes probes. `file_storage` backs disk sending queues and plugin state. Auth extensions (basic, bearer token, OAuth2 client) are referenced by exporters instead of reimplemented in each, and secret stores are extensions too.

**Service telemetry.** The agent observes itself the way the Collector does. `[service.telemetry]` configures its own logs and metrics, exported over OTLP or a Prometheus endpoint rather than through a special receiver. The metrics mirror the Collector's internal ones, for example `plexigraf.receiver.accepted_metric_points`, `plexigraf.receiver.refused_metric_points`, `plexigraf.exporter.sent_metric_points`, `plexigraf.exporter.send_failed_metric_points`, `plexigraf.exporter.queue_size`, and `plexigraf.exporter.queue_capacity`, with `otel.component.type` and `otel.component.name` attributes from the semantic conventions.

**Operations.**

- **Reload.** On SIGHUP or a config file change, the core diffs the pipeline graph by component ID. Unchanged components keep running with their queues and connections, changed ones restart, and removed ones drain before they stop.
- **Failure isolation.** A component that fails to start (a missing approval, an empty selector, a crash loop) is isolated and reported through health and telemetry while everything else keeps running. `--strict` makes any failure fatal, for CI and first deploys.
- **Discovery.** A `receiver_creator` receiver with observers (Kubernetes pods, Docker containers, host ports) starts and stops scrapers for discovered targets, as in the Collector. It ships after v1, and the component kind is reserved now.
- **Kubernetes.** Plugins resolve from `plexigraf.lock` into a cache volume, so a read-only root filesystem works and startup needs no network. `plexigraf-privd` runs as a sidecar or DaemonSet container with only the capabilities it needs.

Metric model: the OpenTelemetry metrics data model, unchanged; see Data model and delivery. Line protocol and other Telegraf formats are supported by encodings at the edges.

## Pipelines and routing

Nothing flows unless a pipeline says so. Unlike Telegraf, where every input goes to every processor and every output unless filtered out, Plexigraf sends a metric only along pipelines the user declares. A receiver or exporter in no pipeline is never started.

The config has four parts:

1. **Plugins.** `[plugins.X]` says where a plugin's code comes from. `plexigraf plugin add` pins it in `plexigraf.lock` with the operator's approval of its capabilities. Short names such as `cpu` resolve from the default registry without a table.
2. **Components.** Each receiver, processor, connector, exporter, and extension is defined once, under an ID, with its own settings.
3. **Pipelines.** Keyed by signal, as in the OpenTelemetry Collector: `[pipelines.metrics.<name>]` lists its `receivers`, its `processors` in order, and its `exporters`.
4. **Service.** `[service]` enables extensions and holds defaults and the agent's own telemetry.

```toml
[service]
extensions = ["health_check", "file_storage"]

[service.defaults]
collection_interval = "10s"          # default for every scraper

[service.telemetry]
metrics = { level = "normal", endpoint = "localhost:8888" }   # the agent's own metrics
logs = { level = "info" }

[extensions.health_check]
endpoint = "localhost:13133"

[extensions.file_storage]
directory = "/var/lib/plexigraf/storage"

# Plugins: where code comes from; pinned and approved in plexigraf.lock
[plugins.nginx]
source = "oci://ghcr.io/example/plexigraf-nginx:1.2"

# Components: defined once per kind, each with an ID
[receivers.cpu]                      # short names resolve from the default registry
[receivers.memory]
[receivers.disk]
[receivers.mysql]                    # defined, but in no pipeline: never started

[receivers.web1]
plugin = "nginx"                     # plugin defaults to the ID when omitted
endpoint = "http://web1/status"
routing_labels = { team = "web" }

[receivers.web2]
plugin = "nginx"
endpoint = "http://web2/status"
routing_labels = { team = "web" }

[processors.add_region]
plugin = "resource"                  # same schema as the Collector's resource processor
attributes = [{ key = "cloud.region", value = "us-east-1", action = "upsert" }]

[processors.drop_debug]
plugin = "filter"                    # OTTL conditions, as in the Collector's filter processor
metrics = { metric = ['IsMatch(name, ".*\.debug$")'] }

[processors.downsample]
plugin = "interval"
interval = "5m"

[connectors.archive]
plugin = "forward"                   # carries metrics from one pipeline into another

[exporters.influx_prod]
plugin = "influxdb_v2"
endpoint = "https://influx.example.com"

[exporters.s3_archive]
plugin = "s3"
bucket = "metrics-archive"
sending_queue = { storage = "file_storage", queue_size = 10000, when_full = "drop_newest" }

# Pipelines: keyed by signal, the whole flow in one place
[pipelines.metrics.system]
receivers  = ["cpu", "memory", "disk"]
processors = ["add_region"]
exporters  = ["influx_prod"]

[pipelines.metrics.web]
receivers  = [{ routing_labels = { team = "web" } }]   # web1, web2, and future web receivers
processors = ["add_region", "drop_debug"]
exporters  = ["influx_prod", "archive"]               # a connector is an exporter here...

[pipelines.metrics.archive]
receivers  = ["archive"]                               # ...and a receiver here
processors = ["downsample"]
exporters  = ["s3_archive"]
```

&#91;embedded content: The example config as a graph · 3 pipelines, 1 unused input\]

The `mysql` receiver is defined but in no pipeline, so it never runs. `add_region` appears twice because each pipeline gets its own instance. The `archive` connector is how the `web` pipeline feeds the `archive` pipeline.

**Rules**

| Topic | Rule |
| --- | --- |
| Pipeline keys | `[pipelines.<signal>.<name>]`. The signal is `metrics` in v1; `logs` and `traces` are reserved so adding them later is not a config break. Each pipeline has `receivers`, `processors`, and `exporters`, as in the Collector |
| IDs | Scoped per kind, so a receiver and an exporter can both be named `otlp`. Connectors appear in both receiver and exporter lists, so a connector ID must not match any receiver or exporter ID. IDs use only `a-z`, `0-9`, `_`, and `-` |
| Plugin resolution | A component's `plugin` defaults to its ID and resolves through `[plugins.X]` or the default registry. `plexigraf plugin add` pins the digest and records the approved capabilities in `plexigraf.lock`; `plexigraf config check` works offline from it |
| `receivers` entries | A receiver or connector ID, a glob such as `"web*"`, or `{ routing_labels = { team = "web" } }`. A glob or label entry that matches nothing is an error unless it sets `allow_no_match = true`. Startup logs and `plexigraf config check` print what every pipeline resolved to, and `plexigraf config diff a.toml b.toml` compares two configs for review |
| Connectors | All edges between pipelines go through connectors. `forward` passes everything; `routing` sends each metric to named pipelines by OTTL condition; an exporter's `dead_letter` names a connector. Cycle checks therefore see every edge |
| Fan-out | A batch shared by fan-out is copy-on-write: a processor that changes it gets its own copy. Each exporter has one sending queue, shared by the pipelines that feed it; `sending_queue = { per_pipeline = true }` isolates them. `plexigraf config check` warns when a receiver reaches the same exporter by two paths, because that writes the data twice |
| Processor state | One instance per pipeline. `share_instance = true` is allowed only when the plugin's manifest declares it stateless |
| Filtering | Only in the `filter` processor and the `routing` connector, both using OTTL conditions. Pipeline lists select components, never metrics |
| Reserved keys | `plugin`, `routing_labels`, `share_instance`, `grant`, `collection_interval`, `timeout`, `sending_queue`, `retry_on_failure`, `acknowledge`, `dead_letter`, `max_undelivered_batches`, and `allow_no_match` are reserved in every component. `plexigraf plugin pack` rejects a plugin schema that uses one |
| Env vars and secrets | `${env:VAR}` (also `${VAR}` and `${VAR:-default}`) is substituted after TOML parsing and only inside string values, so a variable can never inject config. A quoted reference is converted to the type the schema expects, so `port = "${env:PORT}"` becomes an integer. Secrets use `@{store:key}`, where `store` is a secret-store extension; the plugin resolves them through `host.secret`, and only for names its approval lists |
| Multiple files | All `*.toml` files in `--config-dir` merge into one namespace, and the result does not depend on load order. Exactly one `[service]` table is allowed. A duplicate ID is an error that names both files. `plexigraf config print --resolved` shows every entry with its source file |
| Validation | The core validates each component's settings against the JSON Schema in its manifest first, so errors carry file and line, then calls the plugin's optional `validate-config`. A Telegraf-style `[[inputs.cpu]]` gets an error that names the Plexigraf equivalent. `plexigraf config graph` prints the flow as Mermaid or DOT |
| Failure isolation | A component that fails is isolated and reported while the rest run. `--strict` makes any failure fatal |
| No default flow | There is no implicit pipeline. A config with components but no pipelines fails to start with an error that says so. `plexigraf init` writes a starter config with one pipeline |

**Migrating.** Because there is no implicit flow, `plexigraf config migrate telegraf.conf` writes explicit pipelines and translates names: inputs become receivers, outputs become exporters, `interval` becomes `collection_interval`, and `data_format` becomes `encoding`. For a Telegraf config without filters, that is one pipeline from every receiver through every processor, in `order`, to every exporter. Each `namepass`, `tagpass`, or `tagdrop` becomes an OTTL condition in a `filter` processor. The output is meant to be read and split into smaller pipelines by hand. Telegraf's `labels` become `routing_labels`, `alias` becomes the component ID, and field names map to `<measurement>.<field>` unless the receiver emits a semantic-convention name. `plexigraf config import otelcol.yaml` converts an OpenTelemetry Collector config, which maps almost one to one.

**In the core.** At startup the config compiles into a directed graph (`petgraph`): one node per component instance, one bounded `tokio` channel per edge, and connectors as nodes that end one pipeline and start another. Each pipeline's processors run as their own task, and batches cross into Wasm plugins once per hop. Each exporter's sending queue absorbs back-pressure according to its `when_full` policy, so a slow exporter set to `drop_newest` never stalls the pipelines that also feed a healthy one.

## Data model and delivery

The core owns one fixed, versioned metric model; users and plugin authors do not define it. Every metric is tracked from the receiver that produced it to every exporter it reaches, and the receiver learns the outcome.

### Who defines the model

The core does, not the user. Three reasons:

1. **Plugins only interoperate if they agree on the shape.** If each user defined a model, every processor and exporter would have to handle arbitrary schemas, and a plugin written for one deployment would not work in another.
2. **One model keeps converters linear.** Each format needs one decoder into the model and one encoder out of it. With N formats that is 2N converters, not N squared.
3. **The model is the plugin contract.** Changing it is a breaking change for every plugin, so it must change on the project's release schedule, not per config.

What users do control: metric names, attributes, and units within the model, following OTel semantic conventions where one exists, plus an optional `schema` processor that enforces expected names and types on a pipeline. That is schema-on-pipeline, not a custom wire model.

### The OpenTelemetry data model is the internal model

Plexigraf uses the OpenTelemetry (OTel) metrics data model as its internal model, unchanged: resource, instrumentation scope, metric, and data points, with OTel's metric types, temporality, and exemplars. It adds no custom grouping or value types. Metric and attribute names follow [OTel semantic conventions](https://opentelemetry.io/docs/specs/semconv/) wherever a convention exists.

Why OTel and not Telegraf's model: it is the vendor-neutral standard, OTLP in and out becomes lossless, semantic conventions give names that backends and dashboards already understand, and Prometheus 3 ingests OTLP natively. Users learn no Plexigraf-specific schema.

| Level | Holds | Example |
| --- | --- | --- |
| Resource | Attributes describing what produced the data | `host.name=web1`, `service.name`, `k8s.pod.name` |
| Instrumentation scope | Name and version of what produced the data; kept as received on pass-through, and filled in by the host only for metrics a plugin creates | `plexigraf/receivers/cpu` 1.0.0 |
| Metric | Name, description, unit, and exactly one data type | `system.cpu.time`, unit `s`, a monotonic sum |
| Data point | Attributes, start time, time, value, exemplars, flags | `cpu.mode=user`, 51234.5 |

| OTel type | Use for | Prometheus equivalent |
| --- | --- | --- |
| Gauge | A sampled value, such as temperature or utilization | gauge |
| Sum, monotonic | Counters, such as bytes sent or CPU time | counter |
| Sum, non-monotonic | Up-down values, such as memory in use or queue depth | gauge |
| Histogram | Distributions with explicit buckets | histogram |
| Exponential histogram | High-resolution distributions | native histogram |
| Summary | Pre-computed quantiles, only for Prometheus and StatsD compatibility | summary |

**What changes for Telegraf users.** These are the real costs of choosing the standard, and how Plexigraf handles each:

- **One value per data point.** A Telegraf metric with N fields becomes N OTel metrics that share attributes and time. The host-owned batch interns resources, scopes, and attribute sets, so repeated attributes are stored once. The `influx` encoding can regroup on output (`multi_field_lines = true`), so line protocol stays compact.
- **No string or bool metric values.** OTel metrics are numeric. Decoders for other formats apply a rule: bools become 0 or 1 gauges; strings are dropped and counted by default, or become an attribute (`string_values = "attribute"`), or an info-style gauge of 1 (`string_values = "info"`), the Prometheus convention.
- **Untyped values become gauges.** Line protocol carries no type, so decoded values are gauges unless an encoding option marks them as counters (`counter_fields = ["*_total", "accepts"]`).
- **Attribute values are typed** (string, bool, int, double, bytes, and nested arrays and maps), as in OTel. Formats that need strings, such as Influx tags and Prometheus labels, convert on output; a nested value becomes its JSON text there, and only there.

**Nested attribute values stay native.** OTel attribute values can nest arrays and maps, and Plexigraf keeps them exactly. WIT does not allow recursive types, so `plexigraf:metrics` stores a nested value as a flat list of nodes that point to each other by index, with children always after their parent so the tree can never cycle. The host checks that rule when a batch crosses the boundary. Plain scalar values, which are nearly all attributes, skip the list entirely. The SDKs present both as ordinary nested values (`serde_json::Value`-like in Rust, `dict` and `list` in Python, `any` maps and slices in Go), and OTLP round-trips them byte for byte.

**Versioning.** The model is its own WIT package, `plexigraf:metrics`, separate from `plexigraf:plugin`, and it tracks the OTLP metrics data model. New OTel fields arrive additively within a major version. If OTel ever makes a breaking change, the host converts between versions at the plugin boundary, so existing plugins keep running.

### What metrics look like

Each example shows the metric as OTLP JSON (the internal model, exactly), then how the built-in encodings render it. Timestamps are nanoseconds; OTLP JSON writes 64-bit integers as strings.

**1. A counter: CPU time.** A monotonic, cumulative sum named by the `system.cpu.time` semantic convention, with one data point per CPU mode. The full OTLP envelope is shown once here; later examples show only the metric.

```json
{
  "resourceMetrics": [{
    "resource": { "attributes": [
      { "key": "host.name", "value": { "stringValue": "web1" } },
      { "key": "os.type",   "value": { "stringValue": "linux" } }
    ]},
    "scopeMetrics": [{
      "scope": { "name": "plexigraf/receivers/cpu", "version": "1.0.0" },
      "metrics": [{
        "name": "system.cpu.time",
        "description": "Seconds each logical CPU spent in each mode",
        "unit": "s",
        "sum": {
          "aggregationTemporality": 2,
          "isMonotonic": true,
          "dataPoints": [
            {
              "attributes": [
                { "key": "cpu.logical_number", "value": { "intValue": "0" } },
                { "key": "cpu.mode", "value": { "stringValue": "user" } }
              ],
              "startTimeUnixNano": "1761868800000000000",
              "timeUnixNano": "1761955200000000000",
              "asDouble": 51234.5
            },
            {
              "attributes": [
                { "key": "cpu.logical_number", "value": { "intValue": "0" } },
                { "key": "cpu.mode", "value": { "stringValue": "system" } }
              ],
              "startTimeUnixNano": "1761868800000000000",
              "timeUnixNano": "1761955200000000000",
              "asDouble": 8120.25
            }
          ]
        }
      }]
    }]
  }]
}
```

`aggregationTemporality: 2` is cumulative (1 is delta). As Prometheus, through the standard OTel translation:

```text
# TYPE system_cpu_time_seconds counter
# UNIT system_cpu_time_seconds seconds
system_cpu_time_seconds_total{cpu_logical_number="0",cpu_mode="user"} 51234.5
system_cpu_time_seconds_total{cpu_logical_number="0",cpu_mode="system"} 8120.25
target_info{host_name="web1",os_type="linux"} 1
```

As line protocol, with resource attributes written as tags:

```text
system.cpu.time,cpu.logical_number=0,cpu.mode=user,host.name=web1,os.type=linux value=51234.5 1761955200000000000
system.cpu.time,cpu.logical_number=0,cpu.mode=system,host.name=web1,os.type=linux value=8120.25 1761955200000000000
```

**2. An up-down value: memory in use.** A non-monotonic sum (`system.memory.usage`, unit `By`), because memory goes up and down but its parts still add up to a total.

```json
{
  "name": "system.memory.usage",
  "unit": "By",
  "sum": {
    "aggregationTemporality": 2,
    "isMonotonic": false,
    "dataPoints": [
      { "attributes": [{ "key": "system.memory.state", "value": { "stringValue": "used" } }],
        "timeUnixNano": "1761955200000000000", "asInt": "6442450944" },
      { "attributes": [{ "key": "system.memory.state", "value": { "stringValue": "free" } }],
        "timeUnixNano": "1761955200000000000", "asInt": "2147483648" }
    ]
  }
}
```

Prometheus has no up-down type, so this becomes a gauge: `system_memory_usage_bytes{system_memory_state="used"} 6442450944`.

**3. A distribution: HTTP request duration.** An explicit-bucket histogram (`http.server.request.duration`, unit `s`). OTLP stores the count per bucket; `bucketCounts` has one more entry than `explicitBounds`, for values above the last bound.

```json
{
  "name": "http.server.request.duration",
  "unit": "s",
  "histogram": {
    "aggregationTemporality": 2,
    "dataPoints": [{
      "attributes": [
        { "key": "http.request.method", "value": { "stringValue": "GET" } },
        { "key": "http.response.status_code", "value": { "intValue": "200" } }
      ],
      "startTimeUnixNano": "1761868800000000000",
      "timeUnixNano": "1761955200000000000",
      "count": "1500",
      "sum": 37.2,
      "explicitBounds": [0.01, 0.05, 0.1, 0.5, 1],
      "bucketCounts": ["400", "700", "250", "120", "25", "5"]
    }]
  }
}
```

Prometheus buckets are cumulative, so the encoding adds them up:

```text
http_server_request_duration_seconds_bucket{http_request_method="GET",http_response_status_code="200",le="0.01"} 400
http_server_request_duration_seconds_bucket{http_request_method="GET",http_response_status_code="200",le="0.05"} 1100
http_server_request_duration_seconds_bucket{http_request_method="GET",http_response_status_code="200",le="0.1"} 1350
http_server_request_duration_seconds_bucket{http_request_method="GET",http_response_status_code="200",le="0.5"} 1470
http_server_request_duration_seconds_bucket{http_request_method="GET",http_response_status_code="200",le="1"} 1495
http_server_request_duration_seconds_bucket{http_request_method="GET",http_response_status_code="200",le="+Inf"} 1500
http_server_request_duration_seconds_sum{http_request_method="GET",http_response_status_code="200"} 37.2
http_server_request_duration_seconds_count{http_request_method="GET",http_response_status_code="200"} 1500
```

**4. A Telegraf metric converted on the way in.** A legacy line arrives from an `execd` program or an Influx client:

```text
nginx,server=web1 active=12i,accepts=4501i,status="ok" 1761955200000000000
```

With the `influx` encoding set to `counter_fields = ["accepts"]` and `string_values = "info"`, it becomes three OTel metrics that share the `server` attribute and the timestamp:

| Metric | Type | Value | Attributes |
| --- | --- | --- | --- |
| `nginx.active` | Gauge | 12 | `server=web1` |
| `nginx.accepts` | Sum, monotonic, cumulative | 4501 | `server=web1` |
| `nginx.status` | Gauge (info) | 1 | `server=web1`, `value=ok` |

With the defaults, `accepts` would be a gauge and `status` would be dropped and counted in `plexigraf.conversion.lossy`. Written back out with `multi_field_lines = true`, the first two regroup into one line: `nginx,server=web1 active=12i,accepts=4501i 1761955200000000000`.

**Signals.** v1 carries metrics only. Logs and traces, if added, arrive as separate WIT interfaces (`plexigraf:logs`, `plexigraf:traces`) next to the metric worlds, so adding them never changes the metric contract.

### Round-tripping between formats

A conversion is lossless when both formats support a concept. When the target cannot express something, the encoding degrades it by a documented rule and counts it in service telemetry as `plexigraf.conversion.lossy`, with attributes `from`, `to`, and `feature`.

| Format | Preserved | Degraded or lost |
| --- | --- | --- |
| OTLP metrics | Everything; it is the internal model | Nothing |
| Prometheus exposition and remote write | Names through the standard OTel-to-Prometheus translation (dots to underscores, unit and `_total` suffixes), labels, types, histograms, exponential histograms as native histograms, exemplars | Delta sums and histograms must be converted to cumulative (a `cumulative` processor) or the exporter rejects them; resource attributes go to `target_info` unless promoted to labels |
| Influx line protocol | Name, attributes as tags, numeric values, timestamp; `multi_field_lines` regroups metrics into multi-field lines | Type, unit, temporality, and scope are lost on output; histograms are written as `count`, `sum`, and per-bucket fields; parsed lines become gauges unless marked as counters |
| JSON | The full model, as OTLP JSON | Custom layouts depend on the template |

CI enforces this: every built-in encoding runs golden fixtures through format A, then the internal model, then A, and the result must match.

### Delivery and acknowledgement

The default guarantee is at-least-once from a tracked receiver to a sending queue or exporter. Exactly-once is not a goal.

1. A push receiver hands a batch to `metrics-consumer.consume-metrics` and can await a delivery result without blocking its other work. Scrapers return their batch from `scrape` and do not need to track it.
2. The core gives the batch a tracking ID, which every metric keeps through fan-out, processors, connectors, and sending queues.
3. A metric derived from another (split, renamed, enriched) inherits its parent's tracking. A metric dropped by a filter counts as delivered. A processor that aggregates acknowledges its inputs once they are absorbed into a window.
4. An exporter's `export` returns one outcome for the batch, shaped like OTLP partial success: `ok`, `retry` (with an optional retry-after), `reject` (permanent, with a reason), or `partial` with the number of rejected data points and, when the backend says which, their indices.
5. The batch resolves when every copy on every branch is exported, rejected, or dropped by policy. The receiver sees `delivered`, `partial` (with the rejected count), or `failed`.
6. Kafka, MQTT, and AMQP receivers commit offsets or acknowledge messages only on `delivered`. `max_undelivered_batches` on the receiver caps unresolved batches.

Rejected data points go to the connector named by the exporter's `dead_letter` option when the exporter can say which ones failed; otherwise they are counted in service telemetry. Dead-letter routing ships after the core delivery path is proven.

### Sending queues and overflow

Each exporter has one sending queue, shared by every pipeline that feeds it. Teams that need isolation, so one team's flood cannot evict another's data, set `sending_queue = { per_pipeline = true }`. The settings use the Collector's exporter names: `timeout`, `sending_queue` (with `queue_size` and `sizer`), and `retry_on_failure`. Disk queues live in the `file_storage` extension.

```toml
[exporters.influx_prod]
plugin = "influxdb_v2"
endpoint = "https://influx.example.com"
timeout = "10s"
sending_queue = { storage = "file_storage", queue_size = 50000, sizer = "items", when_full = "block" }
retry_on_failure = { initial_interval = "5s", max_interval = "30s", max_elapsed_time = "5m" }
acknowledge = "enqueued"           # tell receivers once queued, or "exported"
dead_letter = "dlq"                # a connector that feeds a dead-letter pipeline
```

| `when_full` | Behavior | Use for |
| --- | --- | --- |
| `block` (default) | Back-pressure upstream; push receivers slow down, and scrapers skip a scrape and count `plexigraf.scrape.skipped` | Primary exporters where loss is worse than delay |
| `drop_oldest` | Evict the oldest queued batch | Dashboards where fresh data matters most |
| `drop_newest` | Refuse the incoming batch | Archives and secondary exporters that must never stall the primary |

The disk queue is a write-ahead log in the `file_storage` extension and survives restarts. `acknowledge = "enqueued"` with a memory queue would let a Kafka receiver commit offsets before anything is durable, so `plexigraf config check` warns about that combination. UDP receivers such as `statsd` cannot block, so they drop and count when their downstream queue is full.

Mapping to the Collector: `when_full = "block"` is the Collector's `block_on_overflow = true`. Plexigraf blocks by default because silently dropping metrics is worse than delaying them; the Collector does not block by default.

### Batch representation and time

- **Batches are a host-owned resource.** In WIT, `batch` is a resource that plugins read and change through methods; the SDKs present it as an iterator. Resources and scopes are stored once per batch in tables and referenced by index, so a processor that edits one attribute copies neither. A batch shared by fan-out is copy-on-write. Phase 0 benchmarks this against a plain OTLP-shaped list across three hops before the interface reaches 1.0.
- **The core owns time.** `scrape` receives `now`, rounded to the configured precision, and scrapers use it unless the source supplies timestamps. Processors that declare `flush` in their manifest get `flush(now)`, so an aggregation window closes on time even when no data arrives, plus a `grace` period for late metrics.

## Plugin interface

The contract is two WIT packages: `plexigraf:metrics`, which holds the data model and the `batch` resource, and `plexigraf:plugin`, which holds host services and one world per component kind. Receivers have two worlds, `scraper` for polling and `receiver` for push, as in the OpenTelemetry Collector. A plugin targets exactly one world.

```wit
// Mirrors the OTLP metrics data model (opentelemetry/proto/metrics/v1),
// using OTLP's own names.
package plexigraf:metrics@0.1.0 {
  interface metrics {
    // OTel AnyValue, including nested arrays and maps. WIT has no recursive
    // types, so a nested value is a flat list of nodes linked by index.
    // Children always come after their parent, so a tree can never cycle.
    variant leaf { empty, str(string), %bool(bool), int(s64), double(f64), bytes(list<u8>) }
    variant node {
      leaf(leaf),
      array(list<u32>),                   // indices of the elements
      kvlist(list<tuple<string, u32>>),   // key and index of each value
    }
    variant any-value {
      leaf(leaf),                         // the common case: no list at all
      tree(list<node>),                   // nodes[0] is the root
    }
    record key-value { key: string, value: any-value }

    record resource {
      attributes: list<key-value>,
      dropped-attributes-count: u32,
      schema-url: string,                 // ResourceMetrics.schema_url
    }
    record instrumentation-scope {
      name: string,
      version: string,
      attributes: list<key-value>,
      dropped-attributes-count: u32,
      schema-url: string,                 // ScopeMetrics.schema_url
    }

    enum aggregation-temporality { unspecified, delta, cumulative }
    flags data-point-flags { no-recorded-value }
    variant number { int(s64), double(f64) }

    record exemplar {
      filtered-attributes: list<key-value>,
      time-unix-nano: u64,
      value: number,
      trace-id: option<list<u8>>,         // host checks 16 bytes
      span-id: option<list<u8>>,          // host checks 8 bytes
    }

    record number-data-point {
      attributes: list<key-value>,
      start-time-unix-nano: u64,
      time-unix-nano: u64,
      value: number,
      exemplars: list<exemplar>,
      %flags: data-point-flags,
    }

    record histogram-data-point {
      attributes: list<key-value>,
      start-time-unix-nano: u64,
      time-unix-nano: u64,
      count: u64,
      sum: option<f64>,
      bucket-counts: list<u64>,
      explicit-bounds: list<f64>,
      exemplars: list<exemplar>,
      %flags: data-point-flags,
      min: option<f64>,
      max: option<f64>,
    }

    record buckets { offset: s32, bucket-counts: list<u64> }
    record exponential-histogram-data-point {
      attributes: list<key-value>,
      start-time-unix-nano: u64,
      time-unix-nano: u64,
      count: u64,
      sum: option<f64>,
      scale: s32,
      zero-count: u64,
      positive: buckets,
      negative: buckets,
      %flags: data-point-flags,
      exemplars: list<exemplar>,
      min: option<f64>,
      max: option<f64>,
      zero-threshold: f64,
    }

    record value-at-quantile { quantile: f64, value: f64 }
    record summary-data-point {
      attributes: list<key-value>,
      start-time-unix-nano: u64,
      time-unix-nano: u64,
      count: u64,
      sum: f64,
      quantile-values: list<value-at-quantile>,
      %flags: data-point-flags,
    }

    record gauge { data-points: list<number-data-point> }
    record sum {
      data-points: list<number-data-point>,
      aggregation-temporality: aggregation-temporality,
      is-monotonic: bool,
    }
    record histogram {
      data-points: list<histogram-data-point>,
      aggregation-temporality: aggregation-temporality,
    }
    record exponential-histogram {
      data-points: list<exponential-histogram-data-point>,
      aggregation-temporality: aggregation-temporality,
    }
    record summary { data-points: list<summary-data-point> }

    variant data {
      gauge(gauge),
      sum(sum),
      histogram(histogram),
      exponential-histogram(exponential-histogram),
      summary(summary),
    }

    record metric {
      resource: u32,                // index into the batch's resource table
      scope: u32,                   // index into the batch's scope table
      name: string,
      description: string,
      unit: string,                 // UCUM, as in OTel
      data: data,
      metadata: list<key-value>,
    }

    // Host-owned batch. Resources and scopes are stored once and referenced by
    // index, so editing a metric never copies them. SDKs wrap this as an iterator.
    resource batch {
      constructor();
      add-resource: func(r: resource) -> u32;
      add-scope: func(s: instrumentation-scope) -> u32;
      resource-at: func(index: u32) -> resource;
      scope-at: func(index: u32) -> instrumentation-scope;
      len: func() -> u32;
      get: func(index: u32) -> metric;
      set: func(index: u32, m: metric);
      push: func(m: metric);
      extend: func(ms: list<metric>);
      retain: func(keep: list<bool>);     // remove many in one pass, no shifting mid-loop
      take-all: func() -> list<metric>;
    }
  }
}

package plexigraf:plugin@0.1.0 {
  interface types {
    enum delivery-status { delivered, partial, failed }
    record delivery { status: delivery-status, rejected-data-points: u64 }

    // Shaped like OTLP ExportPartialSuccess: a count always, indices when known.
    record partial-failure {
      rejected-data-points: u64,
      reason: string,
      indices: option<list<u32>>,
    }
    variant export-outcome { ok, retry(option<u32>), reject(string), partial(partial-failure) }

    variant plugin-error { config(string), transient(string), fatal(string) }
    enum status { starting, ok, recoverable-error, permanent-error, stopping }
  }

  // Services the agent provides to every plugin.
  interface host {
    use types.{status};
    enum level { trace, debug, info, warn, error }
    record kv { key: string, value: string }
    log: func(level: level, msg: string, fields: list<kv>);
    secret: func(name: string) -> option<string>;          // only names in the approval
    state-load: func() -> option<list<u8>>;                // persisted plugin state
    state-save: func(blob: list<u8>);
    report-status: func(s: status, detail: option<string>); // like the Collector's component status
  }

  // The core's encodings, so plugins never bundle their own.
  interface encoding {
    use plexigraf:metrics/metrics@0.1.0.{batch};
    decode: func(format: string, options-toml: string, data: list<u8>) -> result<batch, string>;
    encode: func(format: string, options-toml: string, b: borrow<batch>) -> result<list<u8>, string>;
  }

  // Push receivers hand batches to the pipeline and learn their fate.
  interface metrics-consumer {
    use plexigraf:metrics/metrics@0.1.0.{batch};
    use types.{delivery};
    consume-metrics: async func(b: batch) -> delivery;
  }

  // Every world: an optional validate-config, then start(config) and shutdown.
  // The SDKs supply defaults for validate-config, shutdown, and flush.
  world scraper {
    use plexigraf:metrics/metrics@0.1.0.{batch};
    use types.{plugin-error};
    import host;
    import encoding;
    export validate-config: func(config-toml: string) -> result<_, plugin-error>;
    export start: async func(config-toml: string) -> result<_, plugin-error>;
    export scrape: async func(now-unix-nano: u64) -> result<batch, plugin-error>;
    export shutdown: async func();
  }

  world receiver {
    use types.{plugin-error};
    import host;
    import encoding;
    import metrics-consumer;
    export validate-config: func(config-toml: string) -> result<_, plugin-error>;
    export start: async func(config-toml: string) -> result<_, plugin-error>;   // returns once listening
    export shutdown: async func();
  }

  world processor {
    use plexigraf:metrics/metrics@0.1.0.{batch};
    use types.{plugin-error};
    import host;
    import encoding;
    export validate-config: func(config-toml: string) -> result<_, plugin-error>;
    export start: async func(config-toml: string) -> result<_, plugin-error>;
    export process: async func(input: batch) -> result<batch, plugin-error>;
    export flush: async func(now-unix-nano: u64) -> result<batch, plugin-error>;   // called only if the manifest declares it
    export shutdown: async func();
  }

  world exporter {
    use plexigraf:metrics/metrics@0.1.0.{batch};
    use types.{export-outcome, plugin-error};
    import host;
    import encoding;
    export validate-config: func(config-toml: string) -> result<_, plugin-error>;
    export start: async func(config-toml: string) -> result<_, plugin-error>;
    export export: async func(b: batch) -> result<export-outcome, plugin-error>;
    export shutdown: async func();
  }
}
```

Design choices:

- **OTel names throughout.** `plexigraf:metrics` uses OTLP's own type and field names (`any-value`, `key-value`, `number-data-point`, `aggregation-temporality`, `schema-url`). Worlds are `scraper`, `receiver`, `processor`, and `exporter`, with the Collector's `start` and `shutdown` lifecycle. Anyone who knows OTLP or the Collector can read a Plexigraf plugin.
- **Scope is preserved.** Metrics that pass through a plugin keep their resource and scope. The host fills in the scope (plugin name and version) only for metrics a plugin creates without one.
- **Small surface.** There is no `info` or `init`: name, version, and sample config come from the manifest, and config arrives in `start`. `validate-config` lets `plexigraf config check` ask a plugin about its settings without starting it.
- **Config arrives as a TOML string.** The core validates it against the manifest's JSON Schema first, so errors carry file and line, and the SDK then deserializes it into the plugin's own type. Secret references such as `@{vault:db_password}` stay as references; the plugin resolves them through `host.secret`.
- **Batches are a host-owned resource** with resource and scope tables. See Data model and delivery.
- **Delivery is explicit.** `consume-metrics` resolves with a delivery status, and `export` returns an outcome shaped like OTLP partial success. Together they give Kafka-style receivers at-least-once delivery.
- **Errors and status are typed.** `plugin-error` is `transient` (retry with backoff), `fatal` (stop the component), or `config` (fail startup). `host.report-status` mirrors the Collector's component status and feeds the health endpoint.
- **Two receiver worlds.** A scraper implements `scrape` and is called every `collection_interval`; a push receiver implements `start` and `shutdown` and calls `consume-metrics`. `metrics-consumer` leaves room for `logs-consumer` and `traces-consumer` later.
- **Aggregation is a processor.** A processor that declares `flush` in its manifest is called on a timer so windows close on time. Others never see it; the SDK default covers the export.
- **`encoding` reuses core encoders.** `decode` and `encode` take the same options as config (`counter_fields`, `string_values`, `multi_field_lines`), so a Kafka receiver or a line-protocol exporter bundles no codec of its own.
- **Sync twins only if needed.** If a WASI 0.3 component can implement an `async func` export with a sync lift, Go and Python plugins use the same worlds. If not, sync twins are generated mechanically. Phase 0 settles it.
- **Hardware and OS access come from providers.** See Plugin runtimes and capability providers.
- **Versioning.** Component-model versioning treats each 0.x minor release as incompatible, so 0.x is for the spike only. The target is 1.0 by the end of phase 2. After that, new features arrive as new interfaces marked `@since`, never as changes to existing ones, and experimental ones sit behind `@unstable` gates.

## Plugin runtimes and capability providers

v1 runs plugins as Wasm components, plus existing `execd` programs for compatibility. Native subprocess plugins, built from the same SDK code, are designed now and ship after 1.0. Anything the Wasm sandbox cannot do directly comes from a capability provider: a typed interface that the host implements in-process or, when it needs OS privileges or OS-specific APIs, in a small separate process.

### Which runtime to use

| Plugin needs | Runtime | Examples |
| --- | --- | --- |
| HTTP and REST APIs, OAuth, cloud APIs with Rust SDKs | Wasm | `nginx`, `github`, `cloudwatch` |
| Processing and aggregation | Wasm | `resource`, `filter`, `interval` |
| Files and `/proc` | Wasm with file capabilities | `memory`, `disk`, `filelog` |
| TCP clients with pure-Rust drivers | Wasm, with `plexigraf:tls` for client certificates | Brokers and databases with Rust drivers |
| Unix sockets | Wasm with `plexigraf:unix` (in-process) | `docker`, `systemd` |
| Serial, Modbus RTU, GPIO, I2C, SPI, SocketCAN | Wasm with `plexigraf:serial` or `plexigraf:device` | `modbus`, `sensors` |
| Windows WMI, perf counters, event log | Wasm with `plexigraf:windows` | `windowsperfcounters`, `wmi`, `windowseventlog` |
| Root-only reads | Wasm with `plexigraf:privileged` | `smart`, `iptables`, `ipmi` |
| Existing line-protocol programs | `execd` compatibility | Any Telegraf `execd` program |
| Vendor C SDKs, native Python wheels, Go broker or database drivers, GPU libraries | Native subprocess (after 1.0) | `nvidia_smi` via NVML, Oracle, Go Kafka clients |

### Native subprocess plugins (after 1.0)

- **Same SDK, same WIT, a build flag.** `plexigraf plugin build --target wasm` (default) or `--target native`. A Rust plugin compiles for the host target, a Go plugin with `go build`, and a Python plugin runs under the system interpreter with `plexigraf_sdk`. Plugin code does not change.
- **Wire protocol.** The agent starts the process and connects over a Unix socket (a named pipe on Windows). Calls are the WIT worlds encoded with [wRPC](https://github.com/bytecodealliance/wrpc), the Bytecode Alliance's protocol for WIT over a transport. A batch is serialized in full per call; the SDK implements the `batch` resource locally, so the plugin API is identical.
- **Sandbox, generated from the same manifest and grant.** A separate UID and an empty environment. On Linux, Landlock for files and TCP ports and seccomp for syscalls. On macOS, a `sandbox-exec` profile. On Windows, a restricted token. This is weaker than Wasm: Linux can restrict TCP by port but not by host, for example.
- **Explicit trust.** The manifest says `runtime = "native"`, and the operator must set `allow_native = true` in that component's grant. Startup logs and `plexigraf plugin inspect` show every native plugin. Native artifacts are per platform, published as an OCI index with one image per OS and architecture, and must be signed.
- **Supervision.** The agent restarts a crashed process with backoff, kills it past its deadline, and caps memory with cgroups or rlimits.
- **`execd` compatibility.** `plugin = "execd"` runs an existing line-protocol program unchanged, once its binary is approved by hash and sandboxed as described in Threat model and hardening (S2). It gets no typed config and no delivery tracking beyond write success.

### Capability providers

A provider is a typed WIT interface that a plugin imports; the host links it only when the plugin's approval allows it. `plexigraf:unix` and `plexigraf:tls` need nothing beyond the agent's own permissions, so the host implements them in-process; `plexigraf:tls` follows the `wasi:tls` proposal and retires once that is ready. Providers that need OS privileges or OS-specific APIs run as small native processes that the agent starts, or that `plexigraf-privd` hosts, connected over wRPC. Every operation is a typed function, never a string command.

| Provider | What it offers | Runs as |
| --- | --- | --- |
| `plexigraf:unix/socket` | Connect to an approved socket path and get a stream | In-process |
| `plexigraf:tls/client` | Connect by host and port with an operator-supplied CA, client certificate, and SNI | In-process |
| `plexigraf:serial/port` | Open a port with baud, parity, and stop bits; read; write; flush | Process in the `dialout` group |
| `plexigraf:device/{gpio,i2c,spi,can}` | Allow-listed character-device operations | Process in a device group, or `plexigraf-privd` |
| `plexigraf:windows/{wmi,pdh,eventlog}` | WMI queries, PDH counters, event log reads | Process under the agent service account |
| `plexigraf:privileged` | Typed read-only functions: `smart-read`, `nft-counters`, `ipmi-sensors`, `msr-read` | `plexigraf-privd`, with only the Linux capabilities those need |

A grant names the provider resource, not a raw string:

```toml
[plugins.modbus]
source = "oci://ghcr.io/example/plexigraf-modbus:1.0.0"   # approved for serial access in plexigraf.lock

[receivers.plc1]
plugin = "modbus"
port = "/dev/ttyUSB0"
baud = 19200
grant = { serial = ["/dev/ttyUSB0"] }               # narrow to this one port
```

Providers are maintained by the core project, kept small, and audited. Each ships as an optional package (for example `plexigraf-provider-windows`), so the core binary stays free of OS-specific code. Third parties can write providers, but they are native code and need `allow_native = true`.

## Security model

A plugin starts with no access to anything. It gets a capability only if its manifest requests it and the operator approves it; the agent itself never runs as root.

**1. Manifest (written by the plugin author).** Each plugin ships a `plexigraf-plugin.toml` next to its `.wasm`, and the same data is embedded in a custom section of the component so the two cannot drift.

```toml
# plexigraf-plugin.toml. `plexigraf plugin build` adds name and version from the package
# metadata and kind from the component's world; authors write only this part.
[capabilities]
network.connect = ["$config.endpoint"]  # hosts come from the user's config
filesystem.read = []
env = []
secrets = []
serial = []                           # provider capabilities use the same keys as approvals
privileged = []
```

**2. Approval (by the operator).** `plexigraf plugin add` shows what the manifest requests and records the operator's approval, with the plugin's digest, in `plexigraf.lock`. `plexigraf plugin upgrade` shows a diff when requested capabilities grow and needs a new approval. A component's optional `grant` can only narrow what was approved, for example to one host. Nothing unapproved is ever available, and a mismatch fails that component at startup, never at runtime.

```toml
[plugins.nginx]
source = "oci://ghcr.io/example/plexigraf-nginx:1.2.0"   # approval lives in plexigraf.lock

[receivers.web1]
plugin = "nginx"
endpoint = "http://localhost/status"
grant = { network.connect = ["localhost:80"] }   # optional: narrow further
```

**Matching approvals to config.** A capability that references config must be approved with a concrete scope, for example `network.connect = ["$config.endpoint within *.corp.example:443"]`, so editing config can never widen access. See Threat model and hardening for how the host enforces network destinations. When a component needs more than was approved, `plexigraf config check` prints the requested and approved sets side by side, and `plexigraf plugin inspect <id>` shows each component's effective capabilities.

**3. Enforcement in the host.** Each plugin instance gets its own Wasmtime `Store`, so state is never shared.

| Capability | How the host enforces it |
| --- | --- |
| Files | WASI preopens only the granted paths, read-only unless `filesystem.write` is granted |
| Outbound network | `wasi:sockets` and `wasi:http` calls go through a host check against the granted host:port list |
| Listening sockets | Granted per address; the host binds and hands the plugin a stream |
| Env vars and secrets | Only granted names are visible; secrets come through `host.secret`, never the environment |
| CPU | Epoch interruption gives each call a deadline; a plugin that overruns is cancelled and marked unhealthy |
| Memory | `ResourceLimiter` caps linear memory per instance (default 64 MiB for Rust and Go, 256 MiB for Python, settable per plugin) |
| Clock and randomness | Allowed by default; low risk |

**4. Privileged helper.** Some metrics need root no matter what: SMART data, `iptables` counters, IPMI, MSRs for `turbostat`. Instead of raising the whole agent, a separate small binary, `plexigraf-privd`, runs with only the Linux capabilities it needs (for example `CAP_NET_ADMIN`, `CAP_SYS_RAWIO`) or as a systemd unit with `AmbientCapabilities`. It implements the `plexigraf:privileged` provider as typed read-only functions such as `smart-read` and `nft-counters`, reached over a Unix socket. A plugin imports that interface, and the agent links it only if the plugin's approval lists the function. `plexigraf-privd` has no plugin code and no config parser beyond an allow-list, so it is small enough to audit. On macOS and Windows the same role is filled by a launchd daemon or a Windows service.

**5. Supply chain.** Plugins are distributed as OCI artifacts and signed with Sigstore (cosign). The agent verifies the signature and digest before loading, and config can pin `trusted_publishers`. A lockfile, `plexigraf.lock`, records each plugin's digest so upgrades are explicit. Private registries (Harbor, Artifactory, ECR) and air-gapped hosts use key-based cosign signatures or a private Sigstore instance, because keyless signing needs the public Sigstore servers; plexigraf plugin bundle and plexigraf plugin import move plugins as OCI image layouts.

**6. Native plugins (after 1.0).** Native subprocess plugins and providers need `allow_native = true` and run under an OS sandbox generated from the same manifest, which is weaker than Wasm. See Plugin runtimes and capability providers.

## Threat model and hardening

The sandbox only protects the host if every path around it is closed. A security review found twelve such paths; this section records the fixes, which take precedence over anything earlier in the doc that conflicts.

**Trust boundaries.** One config directory is one trust domain: anyone who can write config can route any receiver's data and reference any secret, so config-write access is limited to people trusted with everything the agent can read. Plexigraf does not isolate teams from each other within one agent. Plugin authors, registries, network peers, and other local users are untrusted.

| # | Severity | Threat | Hardening |
| --- | --- | --- | --- |
| S1 | Critical | A config edit, env var, or reload points an approved plugin at any host, including cloud metadata (`169.254.169.254`) or loopback, or uses DNS rebinding | Approvals record a concrete scope (`$config.endpoint within [...]`). The host resolves names itself and allows `connect` only to the IPs those lookups returned, checking `wasi:http` by authority and `plexigraf:tls` the same way. Loopback, link-local, metadata, and private ranges are denied unless listed. A reload that changes the destination set needs re-approval |
| S2 | Critical | `execd` runs arbitrary native code as the agent user | Each command is approved in `plexigraf.lock` by absolute path, binary hash, and arguments; a changed binary needs re-approval. It starts with an empty environment, gets secrets only through granted env vars, and runs under a separate user started by `plexigraf-privd`, with `no_new_privs`, Landlock where available, and no write access to state, cache, lock, or config. `plexigraf config migrate` writes `execd` entries disabled |
| S3 | Critical | Tampering with the compiled-module cache (`.cwasm`) bypasses signature checks, since Wasmtime trusts deserialized code | The cache is root-owned and read-only to the agent. A root-owned index records each `.cwasm` hash with the engine version and is checked before loading; on mismatch the host recompiles from the verified `.wasm`. Precompiled modules from a registry are never loaded unless signed |
| S4 | High | A directory grant such as `/proc` exposes `/proc/self/mem` and other processes' environments | Grants list files or patterns, enforced in the host. Built-in denies cover `/proc/self`, `/proc/thread-self`, the agent's PIDs, `*/mem`, `*/environ`, and the agent's config, state, and cache. A `plexigraf:procfs` provider serves common procfs reads |
| S5 | High | Built-ins run with full agent authority: the `file` exporter and secret store, `file_storage`, and `${env:VAR}` can read or overwrite anything the agent can; host-side decoders parse untrusted bytes | Built-ins pass the same capability checks as plugins. `${env:}` reads only operator-allow-listed variables. Encodings are fuzzed, and decode work and allocation are charged to the calling component |
| S6 | High | Denial of service outside Wasm limits: unbounded host-owned batches and handles, busy loops in receiver tasks, exponential expansion of shared nodes in `any-value`, oversized or compressed payloads, cardinality floods | Per-component host quotas (batch bytes, metrics, handles, subtasks), a CPU budget per interval with async epoch yields, and a stack cap. Each `any-value` node has exactly one parent, with caps on node count and depth. Receivers default to 4 MiB requests, a 20x decompression ratio, and per-processor series caps |
| S7 | High | One bad file breaks loading for everyone; a stray `[service]` table changes global settings | A bad file or a duplicate ID disables only the components involved and is reported. `[service]` is accepted only from a root-owned file |
| S8 | High | `plexigraf-privd` and providers act as confused deputies: any agent code can call any function, `smart-read` on any device is a raw disk read, a Docker socket grant is root-equivalent | `plexigraf-privd` checks the caller with `SO_PEERCRED`, keeps a root-owned allow-list of the functions and devices enabled on this host, validates arguments (real paths, device types, MSR allow-lists), and rate-limits. Approval prompts label root-equivalent grants such as the Docker socket; a read-only typed Docker provider is preferred. Windows pipes get explicit access lists and reject remote clients |
| S9 | High | Any valid keyless signature passes; short names invite typosquatting; `https://` and `file://` skip signatures; old versions can be rolled back | A signer identity policy (issuer and identity pattern) is required per registry. Short names resolve only to the core namespace signed by the project. Sigstore bundles with transparency-log proofs are stored for offline checks. Downgrades are refused unless allowed. Every source must be signed |
| S10 | Medium | Whoever can write `plexigraf.lock` is the approver, with no record | `plexigraf.lock` is root-owned, and the agent refuses to start if it can write it. Each approval records approver, time, and manifest digest, and approvals are logged |
| S11 | Medium | Endpoints exposed without TLS or auth; secrets leak through logs, status text, errors, debug output, or `print --resolved` | Every endpoint binds to loopback by default, as the Collector does since v0.104; any other address requires TLS, plus auth for OTLP and push receivers. Secret values are redacted everywhere and zeroed after use, as Telegraf does with memguard |
| S12 | Medium | Native runtime gaps and Wasm engine bugs (for example CVE-2023-26489) | `plexigraf-privd` starts native plugins under a separate user; they fail closed when Landlock or seccomp is unavailable, and use AppContainer on Windows. Plexigraf pins a long-term-support Wasmtime release with a patch commitment, and can run untrusted plugins in a separate process |

**Secure defaults.**

- Every endpoint binds to loopback; any other address requires TLS and auth.
- Plugins get only clocks and randomness by default. Sockets, HTTP, name lookup, files, env vars, and stdio are denied until approved, and a test checks every interface the host can link.
- Loopback, link-local, metadata, and private destinations are denied unless listed.
- Per-component limits apply to memory, stack, batch size, handles, request size, decompression, and series count.
- Config, `plexigraf.lock`, and the compiled cache are root-owned and mode `0644` or stricter; state and queues are `0700` and owned by the agent user. The agent refuses to start if it can write config, the lock, or the cache.
- Signatures with an identity policy are required for every source, and short names resolve only to the core namespace.
- `execd` and native plugins are off until approved by hash, and start with an empty environment.
- On Kubernetes: `runAsNonRoot`, a read-only root filesystem, and all capabilities dropped. The `plexigraf-privd` container gets only the listed capabilities and devices, never `privileged: true`.

## Example plugin: `random`

The first plugin ports spawn's random generators into a Plexigraf scraper. It needs no capabilities, so it tests the full load, config, scrape, and export path without touching security.

### Rust

**Layout**

```text
plexigraf-scraper-random/
  Cargo.toml
  plexigraf-plugin.toml
  src/lib.rs
```

**Cargo.toml**

```toml
[package]
name = "plexigraf-scraper-random"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
plexigraf-sdk = "0.1"          # wraps wit-bindgen output for plexigraf:plugin
serde = { version = "1", features = ["derive"] }
toml = "0.8"
fastrand = "2"
```

**src/lib.rs**

```rust
use serde::Deserialize;
use plexigraf_sdk::{export_scraper, Batch, Metric, PluginError, Scraper, Timestamp};

// The SDK deserializes the component's TOML settings into this struct.
#[derive(Deserialize)]
struct Random {
    #[serde(default = "default_prefix")]
    prefix: String,   // metric name prefix
    #[serde(default)]
    min: i64,
    #[serde(default = "default_max")]
    max: i64,
}
fn default_prefix() -> String { "random".into() }
fn default_max() -> i64 { 100 }

impl Scraper for Random {
    async fn start(cfg: Self) -> Result<Self, PluginError> {
        if cfg.min >= cfg.max {
            return Err(PluginError::Config("min must be less than max".into()));
        }
        Ok(cfg)
    }

    // `now` comes from the core, already rounded to the configured precision.
    // Each value is its own OTel gauge; "1" is the UCUM unit for a plain number.
    async fn scrape(&mut self, now: Timestamp) -> Result<Batch, PluginError> {
        let attrs = [("source", "plexigraf")];
        let mut batch = Batch::new();
        batch.push(
            Metric::gauge(format!("{}.int", self.prefix), "1")
                .point(now, fastrand::i64(self.min..self.max), &attrs),
        );
        batch.push(
            Metric::gauge(format!("{}.float", self.prefix), "1")
                .point(now, fastrand::f64(), &attrs),
        );
        Ok(batch)
    }
}

// Name, version, and sample config come from the manifest; shutdown uses the default.
export_scraper!(Random);
```

**Build and run**

```sh
plexigraf plugin new --lang rust random             # scaffold (done once)
plexigraf plugin build                              # cargo build for wasm32-wasip2, embeds the manifest
plexigraf plugin test                               # runs the plugin in an in-process host
plexigraf plugin run --once --config random.toml    # one scrape, printed
plexigraf plugin pack && plexigraf plugin sign
plexigraf plugin publish oci://ghcr.io/me/plexigraf-random:0.1.0
```

**random.toml**

```toml
[service.defaults]
collection_interval = "10s"

[plugins.random]
source = "file://./plugins/plexigraf_scraper_random.wasm"

[receivers.random]
min = 0
max = 50

[exporters.console]
plugin = "debug"                  # built into core; prints to stdout
encoding = { format = "influx" }

[pipelines.metrics.demo]          # required: nothing flows without a pipeline
receivers = ["random"]
exporters = ["console"]
```

Expected output each interval: two OTel gauges, `random.int` and `random.float`. The `debug` exporter with the `influx` encoding writes one line per metric by default:

```text
random.int,source=plexigraf value=17i 1761955200000000000
random.float,source=plexigraf value=0.4213 1761955200000000000
```

With `multi_field_lines = true` on the encoding, metrics that share a name prefix, attributes, and time regroup into one Telegraf-style line: `random,source=plexigraf int=17i,float=0.4213 1761955200000000000`.

### Rust: a push receiver and an exporter

These two sketches show the delivery contract from both ends. The consumer client and HTTP helpers are illustrative.

**A queue receiver** acknowledges a message only after its metrics are delivered, which gives at-least-once delivery:

```rust
use serde::Deserialize;
use plexigraf_sdk::{encoding, export_receiver, metrics_consumer, spawn, DeliveryStatus, PluginError, Receiver};

#[derive(Deserialize)]
struct Queue {
    endpoint: String,
    topic: String,
}

impl Receiver for Queue {
    // Returns once subscribed; the loop keeps running as a background task.
    async fn start(cfg: Self) -> Result<Self, PluginError> {
        let mut sub = subscribe(&cfg.endpoint, &cfg.topic).await?;   // pure-Rust client, TLS via plexigraf:tls
        spawn(async move {
            while let Some(msg) = sub.next().await {
                // Reuse the core's OTLP decoder instead of bundling one.
                let Ok(batch) = encoding::decode("otlp", "", &msg.payload) else {
                    msg.nack().await;
                    continue;
                };
                match metrics_consumer::consume_metrics(batch).await.status {
                    DeliveryStatus::Delivered => msg.ack().await,
                    _ => msg.nack().await,   // the broker redelivers later
                }
            }
        });
        Ok(cfg)
    }
    // shutdown uses the default: the host cancels spawned tasks.
}

export_receiver!(Queue);
```

This version waits for each delivery before reading the next message. A real receiver keeps up to `max_undelivered_batches` batches unresolved at once.

**An HTTP exporter** reports OTLP-style partial success, so a few bad data points do not fail or retry the whole batch:

```rust
use serde::Deserialize;
use plexigraf_sdk::{encoding, export_exporter, http, Batch, ExportOutcome, Exporter, PartialFailure, PluginError};

#[derive(Deserialize)]
struct HttpExporter {
    endpoint: String,
}

impl Exporter for HttpExporter {
    async fn export(&mut self, batch: Batch) -> Result<ExportOutcome, PluginError> {
        let body = encoding::encode("otlp", "", &batch).map_err(PluginError::Fatal)?;
        let resp = http::post(&self.endpoint, body)
            .await
            .map_err(|e| PluginError::Transient(e.to_string()))?;
        Ok(match resp.status {
            200..=299 => match resp.partial_success() {   // OTLP ExportPartialSuccess, if present
                None => ExportOutcome::Ok,
                Some(p) => ExportOutcome::Partial(PartialFailure {
                    rejected_data_points: p.rejected_data_points,
                    reason: p.error_message,
                    indices: None,                       // OTLP reports a count, not which ones
                }),
            },
            429 | 503 => ExportOutcome::Retry(resp.retry_after_ms()),
            400 => ExportOutcome::Reject("bad request".into()),
            s => return Err(PluginError::Transient(format!("HTTP {s}"))),
        })
    }
}

export_exporter!(HttpExporter);   // start and shutdown use the trait defaults
```

A retried batch goes back into the exporter's sending queue. Rejected data points go to its `dead_letter` connector when the exporter knows which ones failed, and the originating receivers learn the outcome through their delivery results.

### Python

The same plugin in Python builds with [componentize-py](https://component-model.bytecodealliance.org/language-support/building-a-simple-component/python.html), which bundles a CPython interpreter and the plugin code into one component. A small `plexigraf-sdk` Python package hides the generated `wit_world` bindings, so authors subclass `Scraper` instead of writing binding code.

**Layout**

```text
plexigraf-scraper-random-py/
  pyproject.toml
  plexigraf-plugin.toml
  random_scraper.py
  wit/                # plexigraf:plugin WIT files, vendored by `plexigraf plugin init`
```

**random\_scraper.py**

```python
import random
from dataclasses import dataclass

from plexigraf_sdk import Batch, ConfigError, Metric, Scraper, register


# The SDK builds an instance from the component's TOML settings.
@dataclass
class Random(Scraper):
    prefix: str = "random"
    min: int = 0
    max: int = 100

    def start(self) -> None:
        if self.min >= self.max:
            raise ConfigError("min must be less than max")

    def scrape(self, now: int) -> Batch:
        attrs = {"source": "plexigraf"}
        batch = Batch()
        batch.push(
            Metric.gauge(f"{self.prefix}.int", unit="1")
            .point(now, random.randint(self.min, self.max - 1), attrs)
        )
        batch.push(
            Metric.gauge(f"{self.prefix}.float", unit="1")
            .point(now, random.random(), attrs)
        )
        return batch


register(Random)
```

**Build**

```sh
plexigraf plugin new --lang python random
plexigraf plugin build      # runs componentize-py, embeds the manifest
plexigraf plugin test
```

Notes:

- Inside `plexigraf-sdk`, a `WitWorld` class implements the world's exports and forwards each call to the registered plugin. `ConfigError` maps to the WIT plugin-`error.config` case.
- componentize-py runs module import at build time and snapshots the result, so `register(...)` at module level costs nothing at agent startup.
- Only pure-Python packages, or packages built for WASI, can be imported. Libraries with C extensions (for example `psutil`) will not work.
- The component embeds CPython, so it is tens of megabytes (approximate; measure in phase 0) against well under 1 MB for the Rust version.

### Go

Go matters most for adoption, because every existing Telegraf plugin author writes Go. The current [component-model guide for Go](https://component-model.bytecodealliance.org/language-support/building-a-simple-component/go.html) uses `componentize-go` with mainline Go 1.25.5 or newer. TinyGo with `wit-bindgen-go` (`-target=wasip2`) is the fallback. A `plexigraf-sdk-go` module wraps the generated bindings, so a plugin is a plain Go struct with Init and Scrape methods.

**Layout**

```text
plexigraf-scraper-random-go/
  go.mod
  plexigraf-plugin.toml
  main.go
  wit/
```

**main.go**

```go
package main

import (
	"errors"
	"math/rand/v2"

	plexigraf "github.com/powersj/plexigraf/sdk/go"
)

// The SDK decodes the component's TOML settings into this struct.
type Random struct {
	Prefix string `toml:"prefix"`
	Min    int64  `toml:"min"`
	Max    int64  `toml:"max"`
}

func (r *Random) Start() error {
	if r.Prefix == "" {
		r.Prefix = "random"
	}
	if r.Max == 0 {
		r.Max = 100
	}
	if r.Min >= r.Max {
		return plexigraf.ConfigError(errors.New("min must be less than max"))
	}
	return nil
}

func (r *Random) Scrape(now plexigraf.Time) (*plexigraf.Batch, error) {
	attrs := plexigraf.Attrs("source", "plexigraf")
	b := plexigraf.NewBatch()
	b.Push(plexigraf.Gauge(r.Prefix+".int", "1").Point(now, r.Min+rand.Int64N(r.Max-r.Min), attrs))
	b.Push(plexigraf.Gauge(r.Prefix+".float", "1").Point(now, rand.Float64(), attrs))
	return b, nil
}

// The kind (scraper) is inferred from the methods Random implements.
func init() { plexigraf.Register(&Random{}) }

func main() {}
```

**Build**

```sh
plexigraf plugin new --lang go random
plexigraf plugin build      # runs componentize-go (or TinyGo), embeds the manifest
plexigraf plugin test
```

Notes:

- `plexigraf-sdk-go` owns the generated export functions and calls the registered plugin. Plugin code never touches binding types, so a change in `componentize-go`'s naming only touches the SDK.
- Migration path: the SDK can offer a `telegraf.Accumulator` adapter, so an existing `Gather(acc telegraf.Accumulator) error` compiles nearly unchanged, with each field becoming an OTel gauge named measurement.field. This only works when the plugin's dependencies build for WASI; anything using cgo, `x/sys/unix` ioctls, or raw sockets does not.
- The component carries the Go runtime and garbage collector, so expect a few megabytes (approximate; measure in phase 0).

### Language support at a glance

| Language | Toolchain | Async (WASI 0.3) exports today | Imports allowed | Best fit |
| --- | --- | --- | --- | --- |
| Rust | `cargo build --target wasm32-wasip2` + `wit-bindgen` | Yes, through `wit-bindgen` | Most pure-Rust crates | Core-maintained plugins, high-volume receivers and exporters |
| Go | `componentize-go` (Go 1.25.5+), or TinyGo | Not yet confirmed | Pure-Go modules | Porting Telegraf plugins, existing Go authors |
| Python | `componentize-py` | Experimental | Pure-Python packages | Processors, custom API scrapers, quick prototypes |

Consequence for the WIT design: Go and Python toolchains may not support async exports yet. Phase 0 checks whether a sync export can implement the async worlds. If it cannot, the host also accepts generated sync twins (`scrape: func(now-unix-nano: u64)` rather than `async func`) and runs each sync call on a blocking worker, so one slow plugin cannot stall the others.

The second plugin should be `mem`ory, reading `/proc/meminfo`. It is just as small but needs `filesystem.read = ["/proc/meminfo"]`, so it exercises the grant path. The third should be `smart`, which proves the `plexigraf-privd` path.

## Packaging and developer experience

The project ships three things: the `plexigraf` binary, the optional `plexigraf-privd` helper, and the `plexigraf-sdk` crate. Plugins ship separately.

**For operators**

| Command | What it does |
| --- | --- |
| `plexigraf run --config plexigraf.toml` | Resolve, verify, and load plugins, then run |
| `plexigraf plugin add oci://ghcr.io/org/plexigraf-nginx:1.2` | Fetch, verify signature, show requested capabilities for approval, record digest and approval in `plexigraf.lock`, cache under `/var/lib/plexigraf/plugins` |
| `plexigraf plugin inspect nginx` | Print manifest, requested capabilities, publisher, and sample config |
| `plexigraf config check` | Validate every plugin section against its schema and every grant against its manifest |
| `plexigraf config migrate telegraf.conf` | Translate a Telegraf config, writing explicit pipelines and flagging plugins with no Plexigraf port |
| `plexigraf once` | Run every scraper once and print, like spawn's `once` |

Plugin sources: `file://`, `https://` (with a required digest), and `oci://`. Air-gapped hosts use `file://` plus `plexigraf.lock`. Wasmtime's compiled-module cache means each plugin is compiled once per host, so startup stays fast after the first run.

**For plugin authors**

- `plexigraf-sdk` (Rust) hides `wit-bindgen`: `Scraper`, `Receiver`, `Processor`, and `Exporter` traits with defaults for optional exports, a `Metric` builder, and `export_scraper!`, `export_receiver!`, `export_processor!`, and `export_exporter!` macros. The Python and Go SDKs infer the kind from the class or struct, so registration is one call.
- One CLI covers every language: `plexigraf plugin new --lang rust|go|python`, then `build`, `test`, `run --once`, `pack`, `sign`, and `publish`. The manifest is generated from package metadata and the component's world, so name, version, and kind are declared once.
- Each SDK ships a test harness that runs the plugin in an in-process host with fake capabilities, so `cargo test`, `pytest`, and `go test` cover it.
- The raw `.wit` files are published so Go (via `wit-bindgen-go`), Python (`componentize-py`), and JS (`jco`) authors can target the same worlds.
- A community index (a Git repo of manifests and OCI references) replaces Telegraf's in-tree plugin list. Inclusion requires a signed artifact and a passing conformance test.

**Core dependency budget.** Measure `Cargo.lock` in phase 0 and set a hard crate budget from it. Most of the tree will come from `wasmtime`; `oci-client` and `sigstore` are also heavy, so put them behind Cargo features for minimal builds. Direct dependencies: `wasmtime`, `wasmtime-wasi`, `wasmtime-wasi-http`, `tokio`, `serde`, `toml`, `clap`, `tracing`, `oci-client`, `sigstore`. Enforce with `cargo deny` in CI.

## Exploration plan

Run it as five phases, each ending in a demo that answers one question. Stop or change course at any gate that fails.

1. **Phase 0: Spike the host (1 to 2 weeks).** A Rust binary embeds Wasmtime, loads one hard-coded component, calls `scrape` on a timer, and prints line protocol. Gate: the `random` plugin runs, and we know the crate count, binary size, and throughput for 1,000-metric batches across three Wasm hops at realistic tag cardinality, comparing the host-owned batch resource with a copied list, and whether a sync export can implement an async WIT function.
2. **Phase 1: Core agent (3 to 4 weeks).** TOML config with plugins, components, pipelines keyed by signal, and the `forward` connector; the per-kind registry modeled on spawn; a metric pipeline with bounded channels, copy-on-write fan-out, memory sending queues, and delivery tracking end to end; the built-in `debug` and `file` exporters and `influx` encoding; service telemetry and the `health_check` extension; reload by graph diff; `plexigraf once`; and graceful shutdown. Gate: `random` reaches the `debug` and `file` exporters from config alone, a receiver learns its delivery status, and a plugin crash or timeout is isolated.
3. **Phase 2: Capabilities (3 weeks).** Manifest parsing, grants, preopens, network allow-list, memory and epoch limits, `plexigraf config check`. Port the `memory` scraper and an HTTP scraper such as `nginx`, and add the filter and transform processors with an OTTL subset. Gate: a plugin that reads a file it was not granted fails at startup with a clear error.
4. **Phase 3: Exporters and privilege (4 weeks).** Port the `influxdb_v2` and `prometheus` exporters with batching, retry, the `file_storage` extension for disk queues, overflow policies, and OTLP-style partial success. Port a Kafka receiver to prove at-least-once delivery. Build `plexigraf-privd` with two typed functions and port `smart`. Gate: the agent runs as a normal user, `smart` metrics still arrive, and a Kafka receiver loses nothing across an exporter outage and an agent restart.
5. **Phase 4: Distribution (3 weeks).** OCI fetch, cosign verification, `plexigraf.lock`, `plexigraf-sdk` on crates.io, plugin template, plexigraf plugin new --lang, plexigraf dev with hot reload, `plexigraf config migrate`. The routing connector, dead-letter edges, and plexigraf config import also land here. Gate: an outside developer builds and runs a plugin using only the docs.

**Open questions**

- WASI 0.3 async support in Go and Python guests is still maturing; the sync twin worlds cover them until it lands.
- Batch resource or copied list: the phase 0 benchmark decides before the interface reaches 1.0.
- Memory at 200+ receivers: the Wasmtime pooling allocator, ahead-of-time `.cwasm`, and shared stateless processors should be enough; phase 0 measures it.
- Native subprocess runtime after 1.0: which vendor SDK and Windows cases justify building it first?
- Logs and traces: in scope after v1, or never?
- OTTL coverage: which OTTL functions do the `filter` and `transform` processors and the `routing` connector need in v1? This replaces the Starlark question.
- Naming, licensing, and whether Telegraf's maintainers want this as a sibling project or a separate effort.

**Risks**

- Plugins that need netlink, raw sockets, or ioctl (`ping`, `wireless`, `ethtool`) cannot run in pure WASI. They need a capability provider or the native runtime.
- The plugin ecosystem starts at zero; adoption depends on the SDK being pleasant and the migration tool covering the top 30 Telegraf plugins.
- Wasm cold-compile time on small devices (Raspberry Pi class) can be seconds per plugin; mitigate with ahead-of-time `.cwasm` artifacts.

## Sources

- [WASI 0.3 launch, Bytecode Alliance](https://bytecodealliance.org/articles/WASI-0.3)
- [Fluent Bit WASM filter docs](https://docs.fluentbit.io/manual/pipeline/filters/wasm)
- [Vector: wasm transform to be removed](https://vector.dev/highlights/2021-08-23-removing-wasm/)
- Local: Telegraf repo at `de2980854` (`input.go`, `output.go`, `plugin.go`, `plugins/common/shim`, `go.mod`) and `../spawn` (`agent/agent.go`, `generators/`, `outputs/`)

