//! The plugin host: loading components, implementing the host imports, and
//! running each component instance in its own sandboxed store.
//!
//! Every instance lives on a dedicated thread that owns its Wasmtime `Store`.
//! The async runtime talks to it over a channel, so a slow plugin never
//! blocks a Tokio worker. Each call gets an epoch deadline, so a plugin that
//! overruns is interrupted, and a trapped instance is rebuilt with backoff.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::sync::oneshot;
use wasmtime::component::{Component, HasSelf, InstancePre, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::exporter::{Exporter, ExporterPre};
use crate::bindings::metrics::{InstrumentationScope, Metric, Resource as OtelResource};
use crate::bindings::processor::{Processor, ProcessorPre};
use crate::bindings::scraper::{Scraper, ScraperPre};
use crate::bindings::types::{ExportOutcome, PluginError, Status};
use crate::bindings::{encoding, host, metrics, types};
use crate::health::Health;
use crate::model::{BatchData, MAX_METRICS_PER_BATCH};

const EPOCH_TICK: Duration = Duration::from_millis(10);
const MAX_LIVE_BATCHES: usize = 1024;
const MAX_STATE_BYTES: usize = 1 << 20;
const STDERR_CAPACITY: usize = 1 << 20;
const DEFAULT_MEMORY_MIB: u64 = 64;

/// The component-model world a plugin targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum World {
    Scraper,
    Processor,
    Exporter,
}

impl World {
    pub fn as_str(self) -> &'static str {
        match self {
            World::Scraper => "scraper",
            World::Processor => "processor",
            World::Exporter => "exporter",
        }
    }
}

/// `plexigraf-plugin.toml`, written by the plugin author.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Optional; inferred from the component's exports when omitted.
    pub kind: Option<String>,
    /// Processors only: call `flush` on a timer.
    #[serde(default)]
    pub flush: bool,
    pub memory_limit_mib: Option<u64>,
    #[serde(default)]
    pub capabilities: toml::Table,
    /// Example settings, shown by `plexigraf plugin inspect`.
    pub sample_config: Option<String>,
}

impl Manifest {
    /// Find the manifest next to a plugin file: `<stem>.plexigraf-plugin.toml`,
    /// then `plexigraf-plugin.toml` in the same directory.
    pub fn find(wasm: &Path) -> Result<(Manifest, Option<PathBuf>)> {
        let dir = wasm.parent().unwrap_or(Path::new("."));
        let stem = wasm.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        for candidate in [
            dir.join(format!("{stem}.plexigraf-plugin.toml")),
            dir.join("plexigraf-plugin.toml"),
        ] {
            if candidate.is_file() {
                let text = std::fs::read_to_string(&candidate)?;
                let m: Manifest = toml::from_str(&text).with_context(|| format!("parsing {}", candidate.display()))?;
                return Ok((m, Some(candidate)));
            }
        }
        Ok((Manifest::default(), None))
    }

    /// Plexigraf 0.1 grants no capabilities, so a plugin that asks for any fails
    /// closed rather than running without what it needs.
    pub fn requested_capabilities(&self) -> Vec<String> {
        fn walk(prefix: &str, t: &toml::Table, out: &mut Vec<String>) {
            for (k, v) in t {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                match v {
                    toml::Value::Table(t) => walk(&key, t, out),
                    toml::Value::Array(a) if a.is_empty() => {}
                    toml::Value::Boolean(false) => {}
                    other => out.push(format!("{key} = {other}")),
                }
            }
        }
        let mut out = Vec::new();
        walk("", &self.capabilities, &mut out);
        out
    }
}

/// The shared engine and linker. One per agent.
pub struct HostEngine {
    engine: Engine,
    linker: Linker<PluginState>,
}

impl HostEngine {
    pub fn new() -> Result<Arc<HostEngine>> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        // All three worlds import the same interfaces, so one linker serves them all.
        Scraper::add_to_linker::<PluginState, HasSelf<PluginState>>(&mut linker, |s| s)?;

        let weak = engine.weak();
        std::thread::Builder::new()
            .name("plexigraf-epoch".into())
            .spawn(move || {
                while let Some(engine) = weak.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(EPOCH_TICK);
                }
            })?;
        Ok(Arc::new(HostEngine { engine, linker }))
    }
}

/// A compiled plugin, ready to instantiate any number of times.
pub struct Plugin {
    pub name: String,
    pub manifest: Manifest,
    pub world: World,
    pub version: String,
    pub memory_limit: usize,
    pre: Pre,
    host: Arc<HostEngine>,
}

#[derive(Clone)]
enum Pre {
    Scraper(ScraperPre<PluginState>),
    Processor(ProcessorPre<PluginState>),
    Exporter(ExporterPre<PluginState>),
}

impl Plugin {
    pub fn load(host: &Arc<HostEngine>, name: &str, path: &Path, memory_limit_mib: Option<u64>) -> Result<Plugin> {
        let (manifest, _) = Manifest::find(path)?;
        let requested = manifest.requested_capabilities();
        if !requested.is_empty() {
            bail!(
                "plugin {name} requests capabilities ({}), but Plexigraf 0.1 cannot grant any yet; it fails closed instead of running without them",
                requested.join(", ")
            );
        }
        let started = Instant::now();
        let component =
            Component::from_file(&host.engine, path).map_err(|e| anyhow!("loading {}: {e:#}", path.display()))?;
        tracing::debug!(plugin = name, elapsed = ?started.elapsed(), "compiled plugin");

        let exports: Vec<String> = component
            .component_type()
            .exports(&host.engine)
            .map(|(n, _)| n.to_string())
            .collect();
        let inferred: Vec<World> = [
            ("scrape", World::Scraper),
            ("process", World::Processor),
            ("export", World::Exporter),
        ]
        .into_iter()
        .filter(|(f, _)| exports.iter().any(|e| e == f))
        .map(|(_, w)| w)
        .collect();
        let world = match manifest.kind.as_deref() {
            Some("scraper") => World::Scraper,
            Some("processor") => World::Processor,
            Some("exporter") => World::Exporter,
            Some(other) => bail!("plugin {name}: manifest kind {other:?} must be scraper, processor, or exporter"),
            None => match inferred.as_slice() {
                [w] => *w,
                [] => bail!(
                    "plugin {name} exports none of scrape, process, or export (exports: {}); is it built for a plexigraf:plugin world?",
                    exports.join(", ")
                ),
                _ => bail!(
                    "plugin {name} exports more than one of scrape, process, and export; set `kind` in its manifest"
                ),
            },
        };

        let instance_pre: InstancePre<PluginState> = host
            .linker
            .instantiate_pre(&component)
            .map_err(|e| anyhow!("plugin {name} imports something the host does not provide: {e:#}"))?;
        let pre = match world {
            World::Scraper => Pre::Scraper(ScraperPre::new(instance_pre)?),
            World::Processor => Pre::Processor(ProcessorPre::new(instance_pre)?),
            World::Exporter => Pre::Exporter(ExporterPre::new(instance_pre)?),
        }
        .clone();
        let mib = memory_limit_mib
            .or(manifest.memory_limit_mib)
            .unwrap_or(DEFAULT_MEMORY_MIB);
        let memory_limit = usize::try_from(mib)
            .ok()
            .and_then(|m| m.checked_mul(1 << 20))
            .ok_or_else(|| anyhow!("plugin {name}: memory limit of {mib} MiB is too large"))?;
        Ok(Plugin {
            name: manifest.name.clone().unwrap_or_else(|| name.to_string()),
            version: manifest.version.clone().unwrap_or_default(),
            manifest,
            world,
            memory_limit,
            pre,
            host: host.clone(),
        })
    }
}

/// Per-instance host state, reachable from every host import.
pub struct PluginState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    id: String,
    live_batches: usize,
    state: Arc<Mutex<Option<Vec<u8>>>>,
    health: Arc<Health>,
    warned_secret: bool,
}

impl WasiView for PluginState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl PluginState {
    fn batch(&self, r: &Resource<BatchData>) -> wasmtime::Result<&BatchData> {
        Ok(self.table.get(r)?)
    }

    fn batch_mut(&mut self, r: &Resource<BatchData>) -> wasmtime::Result<&mut BatchData> {
        Ok(self.table.get_mut(r)?)
    }

    fn push_batch(&mut self, b: BatchData) -> wasmtime::Result<Resource<BatchData>> {
        if self.live_batches >= MAX_LIVE_BATCHES {
            wasmtime::bail!("plugin holds more than {MAX_LIVE_BATCHES} batches");
        }
        self.live_batches += 1;
        Ok(self.table.push(b)?)
    }

    fn take_batch(&mut self, r: Resource<BatchData>) -> wasmtime::Result<BatchData> {
        self.live_batches = self.live_batches.saturating_sub(1);
        Ok(self.table.delete(r)?)
    }
}

fn check_len(len: usize) -> wasmtime::Result<()> {
    if len > MAX_METRICS_PER_BATCH {
        wasmtime::bail!("batch would exceed {MAX_METRICS_PER_BATCH} metrics");
    }
    Ok(())
}

impl metrics::Host for PluginState {}

impl metrics::HostBatch for PluginState {
    fn new(&mut self) -> wasmtime::Result<Resource<BatchData>> {
        self.push_batch(BatchData::default())
    }

    fn add_resource(&mut self, b: Resource<BatchData>, r: OtelResource) -> wasmtime::Result<u32> {
        let b = self.batch_mut(&b)?;
        b.resources.push(r);
        Ok(u32::try_from(b.resources.len() - 1)?)
    }

    fn add_scope(&mut self, b: Resource<BatchData>, s: InstrumentationScope) -> wasmtime::Result<u32> {
        let b = self.batch_mut(&b)?;
        b.scopes.push(s);
        Ok(u32::try_from(b.scopes.len() - 1)?)
    }

    fn resource_at(&mut self, b: Resource<BatchData>, index: u32) -> wasmtime::Result<OtelResource> {
        let b = self.batch(&b)?;
        b.resources
            .get(index as usize)
            .cloned()
            .ok_or_else(|| wasmtime::format_err!("resource index {index} out of range"))
    }

    fn scope_at(&mut self, b: Resource<BatchData>, index: u32) -> wasmtime::Result<InstrumentationScope> {
        let b = self.batch(&b)?;
        b.scopes
            .get(index as usize)
            .cloned()
            .ok_or_else(|| wasmtime::format_err!("scope index {index} out of range"))
    }

    fn len(&mut self, b: Resource<BatchData>) -> wasmtime::Result<u32> {
        Ok(u32::try_from(self.batch(&b)?.metrics.len())?)
    }

    fn get(&mut self, b: Resource<BatchData>, index: u32) -> wasmtime::Result<Metric> {
        let b = self.batch(&b)?;
        b.metrics
            .get(index as usize)
            .cloned()
            .ok_or_else(|| wasmtime::format_err!("metric index {index} out of range"))
    }

    fn set(&mut self, b: Resource<BatchData>, index: u32, m: Metric) -> wasmtime::Result<()> {
        let b = self.batch_mut(&b)?;
        let slot = b
            .metrics
            .get_mut(index as usize)
            .ok_or_else(|| wasmtime::format_err!("metric index {index} out of range"))?;
        *slot = m;
        Ok(())
    }

    fn push(&mut self, b: Resource<BatchData>, m: Metric) -> wasmtime::Result<()> {
        let b = self.batch_mut(&b)?;
        check_len(b.metrics.len() + 1)?;
        b.metrics.push(m);
        Ok(())
    }

    fn extend(&mut self, b: Resource<BatchData>, ms: Vec<Metric>) -> wasmtime::Result<()> {
        let b = self.batch_mut(&b)?;
        check_len(b.metrics.len() + ms.len())?;
        b.metrics.extend(ms);
        Ok(())
    }

    fn retain(&mut self, b: Resource<BatchData>, keep: Vec<bool>) -> wasmtime::Result<()> {
        let b = self.batch_mut(&b)?;
        if keep.len() != b.metrics.len() {
            wasmtime::bail!("retain got {} flags for {} metrics", keep.len(), b.metrics.len());
        }
        let mut keep = keep.into_iter();
        b.metrics.retain(|_| keep.next().unwrap_or(false));
        Ok(())
    }

    fn take_all(&mut self, b: Resource<BatchData>) -> wasmtime::Result<Vec<Metric>> {
        Ok(std::mem::take(&mut self.batch_mut(&b)?.metrics))
    }

    fn drop(&mut self, rep: Resource<BatchData>) -> wasmtime::Result<()> {
        self.take_batch(rep)?;
        Ok(())
    }
}

impl types::Host for PluginState {}

impl host::Host for PluginState {
    fn log(&mut self, level: host::Level, msg: String, fields: Vec<host::Kv>) -> wasmtime::Result<()> {
        let mut text = msg;
        for f in fields {
            let _ = write!(text, " {}={}", f.key, f.value);
        }
        let id = self.id.as_str();
        match level {
            host::Level::Trace => tracing::trace!(component = id, "{text}"),
            host::Level::Debug => tracing::debug!(component = id, "{text}"),
            host::Level::Info => tracing::info!(component = id, "{text}"),
            host::Level::Warn => tracing::warn!(component = id, "{text}"),
            host::Level::Error => tracing::error!(component = id, "{text}"),
        }
        Ok(())
    }

    fn secret(&mut self, name: String) -> wasmtime::Result<Option<String>> {
        if !self.warned_secret {
            tracing::warn!(component = %self.id, secret = %name, "plugin asked for a secret, but no secret stores are approved in Plexigraf 0.1");
            self.warned_secret = true;
        }
        Ok(None)
    }

    fn state_load(&mut self) -> wasmtime::Result<Option<Vec<u8>>> {
        Ok(self.state.lock().unwrap().clone())
    }

    fn state_save(&mut self, blob: Vec<u8>) -> wasmtime::Result<()> {
        if blob.len() > MAX_STATE_BYTES {
            wasmtime::bail!(
                "plugin state is {} bytes, more than the {MAX_STATE_BYTES}-byte limit",
                blob.len()
            );
        }
        *self.state.lock().unwrap() = Some(blob);
        Ok(())
    }

    fn report_status(&mut self, s: Status, detail: Option<String>) -> wasmtime::Result<()> {
        self.health.set(&self.id, s, detail);
        Ok(())
    }
}

impl encoding::Host for PluginState {
    fn decode(
        &mut self,
        format: String,
        options_toml: String,
        data: Vec<u8>,
    ) -> wasmtime::Result<Result<Resource<BatchData>, String>> {
        let decoded = crate::encoding::parse_options(&options_toml)
            .and_then(|opts| crate::encoding::decode(&format, &opts, &data));
        match decoded {
            Ok(b) => Ok(Ok(self.push_batch(b)?)),
            Err(e) => Ok(Err(e)),
        }
    }

    fn encode(
        &mut self,
        format: String,
        options_toml: String,
        b: Resource<BatchData>,
    ) -> wasmtime::Result<Result<Vec<u8>, String>> {
        let batch = self.batch(&b)?;
        Ok(crate::encoding::parse_options(&options_toml)
            .and_then(|opts| crate::encoding::encode(&format, &opts, batch)))
    }
}

// ---- Instances ----

#[derive(Debug)]
pub enum CallError {
    /// The plugin returned an error.
    Plugin(PluginError),
    /// The instance trapped or overran its deadline and is being rebuilt.
    Trap(String),
    /// The instance is stopped or waiting to restart.
    Unavailable(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Plugin(PluginError::Config(m)) => write!(f, "config error: {m}"),
            CallError::Plugin(PluginError::Transient(m)) => write!(f, "transient error: {m}"),
            CallError::Plugin(PluginError::Fatal(m)) => write!(f, "fatal error: {m}"),
            CallError::Trap(m) => write!(f, "plugin crashed: {m}"),
            CallError::Unavailable(m) => write!(f, "unavailable: {m}"),
        }
    }
}

type Reply<T> = oneshot::Sender<Result<T, CallError>>;

enum Request {
    Scrape(u64, Reply<BatchData>),
    Process(BatchData, Reply<BatchData>),
    Flush(u64, Reply<BatchData>),
    Export(BatchData, Reply<ExportOutcome>),
    Shutdown(oneshot::Sender<()>),
}

enum Bound {
    Scraper(Scraper),
    Processor(Processor),
    Exporter(Exporter),
}

struct Live {
    store: Store<PluginState>,
    bound: Bound,
}

/// A handle to one running component instance.
#[derive(Clone)]
pub struct Instance {
    tx: std::sync::mpsc::Sender<Request>,
    pub id: String,
    pub world: World,
    pub flush: bool,
    timeout: Duration,
}

pub struct InstanceSpec {
    /// The component label used in logs and health, e.g. `receivers.web1`.
    pub id: String,
    pub config_toml: String,
    /// Per-call deadline.
    pub timeout: Duration,
    pub health: Arc<Health>,
}

impl Instance {
    /// Start an instance: instantiate, validate its config, and call `start`.
    /// Returns once `start` has returned.
    pub async fn spawn(plugin: Arc<Plugin>, spec: InstanceSpec) -> Result<Instance> {
        let (tx, rx) = std::sync::mpsc::channel::<Request>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        let world = plugin.world;
        let flush = plugin.manifest.flush && world == World::Processor;
        let handle = Instance {
            tx,
            id: spec.id.clone(),
            world,
            flush,
            timeout: spec.timeout,
        };
        std::thread::Builder::new()
            .name(format!("plexigraf-{}", spec.id))
            .spawn(move || Actor::new(plugin, spec).run(&rx, ready_tx))?;
        match ready_rx.await {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("instance thread exited during start")),
        }
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> Result<T, CallError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| CallError::Unavailable("instance stopped".into()))?;
        // The epoch deadline interrupts the plugin itself; this outer timeout
        // only covers time spent waiting behind earlier calls.
        match tokio::time::timeout(self.timeout * 2 + Duration::from_secs(1), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(CallError::Unavailable("instance stopped".into())),
            Err(_) => Err(CallError::Trap(format!("no reply within {:?}", self.timeout * 2))),
        }
    }

    pub async fn scrape(&self, now: u64) -> Result<BatchData, CallError> {
        self.call(|r| Request::Scrape(now, r)).await
    }

    pub async fn process(&self, b: BatchData) -> Result<BatchData, CallError> {
        self.call(|r| Request::Process(b, r)).await
    }

    pub async fn flush_now(&self, now: u64) -> Result<BatchData, CallError> {
        self.call(|r| Request::Flush(now, r)).await
    }

    pub async fn export(&self, b: BatchData) -> Result<ExportOutcome, CallError> {
        self.call(|r| Request::Export(b, r)).await
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Request::Shutdown(tx)).is_ok() {
            let _ = tokio::time::timeout(self.timeout + Duration::from_secs(1), rx).await;
        }
    }
}

struct Actor {
    plugin: Arc<Plugin>,
    spec: InstanceSpec,
    state: Arc<Mutex<Option<Vec<u8>>>>,
    stderr: Option<MemoryOutputPipe>,
    live: Option<Live>,
    stopped: Option<String>,
    restart_at: Instant,
    backoff: Duration,
}

impl Actor {
    fn new(plugin: Arc<Plugin>, spec: InstanceSpec) -> Actor {
        Actor {
            plugin,
            spec,
            state: Arc::default(),
            stderr: None,
            live: None,
            stopped: None,
            restart_at: Instant::now(),
            backoff: Duration::from_secs(1),
        }
    }

    fn deadline_ticks(&self) -> u64 {
        u64::try_from(self.spec.timeout.as_millis() / EPOCH_TICK.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    fn instantiate(&mut self, start: bool) -> Result<Live, String> {
        let stderr = MemoryOutputPipe::new(STDERR_CAPACITY);
        self.stderr = Some(stderr.clone());
        // Clocks and randomness only: no preopens, env, args, network, or stdin.
        let wasi = WasiCtxBuilder::new().stderr(stderr).build();
        let state = PluginState {
            wasi,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(self.plugin.memory_limit)
                .instances(64)
                .build(),
            id: self.spec.id.clone(),
            live_batches: 0,
            state: self.state.clone(),
            health: self.spec.health.clone(),
            warned_secret: false,
        };
        let mut store = Store::new(&self.plugin.host.engine, state);
        store.limiter(|s| &mut s.limits);
        store.epoch_deadline_trap();
        // Instantiation runs guest start-up code, so it gets a deadline too.
        store.set_epoch_deadline(self.deadline_ticks().max(1000));
        let trap = |e: wasmtime::Error| format!("instantiating: {e:#}");
        let bound = match &self.plugin.pre {
            Pre::Scraper(p) => Bound::Scraper(p.instantiate(&mut store).map_err(trap)?),
            Pre::Processor(p) => Bound::Processor(p.instantiate(&mut store).map_err(trap)?),
            Pre::Exporter(p) => Bound::Exporter(p.instantiate(&mut store).map_err(trap)?),
        };
        let mut live = Live { store, bound };
        let cfg = self.spec.config_toml.clone();
        live.store.set_epoch_deadline(self.deadline_ticks());
        let validated = match &live.bound {
            Bound::Scraper(s) => s
                .plexigraf_plugin_lifecycle()
                .call_validate_config(&mut live.store, &cfg),
            Bound::Processor(p) => p
                .plexigraf_plugin_lifecycle()
                .call_validate_config(&mut live.store, &cfg),
            Bound::Exporter(x) => x
                .plexigraf_plugin_lifecycle()
                .call_validate_config(&mut live.store, &cfg),
        };
        self.check_start("validate-config", validated)?;
        if !start {
            return Ok(live);
        }
        live.store.set_epoch_deadline(self.deadline_ticks());
        let started = match &live.bound {
            Bound::Scraper(s) => s.plexigraf_plugin_lifecycle().call_start(&mut live.store, &cfg),
            Bound::Processor(p) => p.plexigraf_plugin_lifecycle().call_start(&mut live.store, &cfg),
            Bound::Exporter(x) => x.plexigraf_plugin_lifecycle().call_start(&mut live.store, &cfg),
        };
        self.check_start("start", started)?;
        Ok(live)
    }

    fn check_start(&self, what: &str, r: wasmtime::Result<Result<(), PluginError>>) -> Result<(), String> {
        match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("{what}: {}", CallError::Plugin(e))),
            Err(e) => Err(format!("{what} trapped: {e:#}{}", self.stderr_tail())),
        }
    }

    fn stderr_tail(&self) -> String {
        let Some(pipe) = &self.stderr else { return String::new() };
        let bytes = pipe.contents();
        if bytes.is_empty() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&bytes);
        let tail: String = text
            .chars()
            .rev()
            .take(2000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("\nplugin stderr:\n{}", tail.trim_end())
    }

    fn run(mut self, rx: &std::sync::mpsc::Receiver<Request>, ready: oneshot::Sender<Result<(), String>>) {
        match self.instantiate(true) {
            Ok(live) => {
                self.live = Some(live);
                self.spec.health.set(&self.spec.id, Status::Ok, None);
                let _ = ready.send(Ok(()));
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        }
        while let Ok(req) = rx.recv() {
            if let Request::Shutdown(done) = req {
                if let Some(mut live) = self.live.take() {
                    live.store.set_epoch_deadline(self.deadline_ticks());
                    let r = match &live.bound {
                        Bound::Scraper(s) => s.plexigraf_plugin_lifecycle().call_shutdown(&mut live.store),
                        Bound::Processor(p) => p.plexigraf_plugin_lifecycle().call_shutdown(&mut live.store),
                        Bound::Exporter(x) => x.plexigraf_plugin_lifecycle().call_shutdown(&mut live.store),
                    };
                    if let Err(e) = r {
                        tracing::warn!(component = %self.spec.id, "shutdown trapped: {e:#}");
                    }
                }
                self.spec.health.set(&self.spec.id, Status::Stopping, None);
                let _ = done.send(());
                return;
            }
            if let Err(e) = self.ensure_live() {
                reply_err(req, e);
                continue;
            }
            self.handle(req);
        }
    }

    /// Rebuild a crashed instance once its backoff has passed.
    fn ensure_live(&mut self) -> Result<(), CallError> {
        if let Some(reason) = &self.stopped {
            return Err(CallError::Unavailable(reason.clone()));
        }
        if self.live.is_some() {
            return Ok(());
        }
        if Instant::now() < self.restart_at {
            return Err(CallError::Unavailable("restarting after a crash".into()));
        }
        match self.instantiate(true) {
            Ok(live) => {
                tracing::info!(component = %self.spec.id, "restarted plugin instance");
                self.live = Some(live);
                self.backoff = Duration::from_secs(1);
                self.spec.health.set(&self.spec.id, Status::Ok, None);
                Ok(())
            }
            Err(e) => {
                self.crashed(&e);
                Err(CallError::Unavailable(e))
            }
        }
    }

    fn crashed(&mut self, why: &str) {
        self.live = None;
        self.restart_at = Instant::now() + self.backoff;
        tracing::error!(component = %self.spec.id, retry_in = ?self.backoff, "{why}");
        self.backoff = (self.backoff * 2).min(Duration::from_secs(60));
        self.spec
            .health
            .set(&self.spec.id, Status::RecoverableError, Some(why.to_string()));
    }

    fn handle(&mut self, req: Request) {
        enum Done {
            Batch(Reply<BatchData>, wasmtime::Result<Result<BatchData, PluginError>>),
            Export(
                Reply<ExportOutcome>,
                wasmtime::Result<Result<ExportOutcome, PluginError>>,
            ),
            Unsupported(Request),
        }
        let ticks = self.deadline_ticks();
        let scope = (self.plugin.scope_name(), self.plugin.version.clone());
        let live = self.live.as_mut().expect("handle runs only after ensure_live succeeds");
        live.store.set_epoch_deadline(ticks);
        let store = &mut live.store;
        let done = match (req, &live.bound) {
            (Request::Scrape(now, reply), Bound::Scraper(s)) => {
                let r = s.call_scrape(&mut *store, now);
                Done::Batch(reply, finish_batch(store, r, &scope))
            }
            (Request::Process(b, reply), Bound::Processor(p)) => {
                let r = store
                    .data_mut()
                    .push_batch(b)
                    .and_then(|res| p.call_process(&mut *store, res));
                Done::Batch(reply, finish_batch(store, r, &scope))
            }
            (Request::Flush(now, reply), Bound::Processor(p)) => {
                let r = p.call_flush(&mut *store, now);
                Done::Batch(reply, finish_batch(store, r, &scope))
            }
            (Request::Export(b, reply), Bound::Exporter(x)) => {
                let r = store
                    .data_mut()
                    .push_batch(b)
                    .and_then(|res| x.call_export(&mut *store, res));
                Done::Export(reply, r)
            }
            (req, _) => Done::Unsupported(req),
        };
        match done {
            Done::Batch(reply, r) => self.reply(reply, r),
            Done::Export(reply, r) => self.reply(reply, r),
            Done::Unsupported(req) => {
                let world = self.plugin.world.as_str();
                reply_err(
                    req,
                    CallError::Unavailable(format!("not supported by a {world} plugin")),
                );
            }
        }
    }

    fn reply<T>(&mut self, reply: Reply<T>, r: wasmtime::Result<Result<T, PluginError>>) {
        let out = match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(PluginError::Fatal(m))) => {
                tracing::error!(component = %self.spec.id, "plugin reported a fatal error; stopping it: {m}");
                self.stopped = Some(format!("stopped after a fatal error: {m}"));
                self.live = None;
                self.spec
                    .health
                    .set(&self.spec.id, Status::PermanentError, Some(m.clone()));
                Err(CallError::Plugin(PluginError::Fatal(m)))
            }
            Ok(Err(e)) => Err(CallError::Plugin(e)),
            Err(trap) => {
                let why = format!("{}{}", describe_trap(&trap, self.spec.timeout), self.stderr_tail());
                self.crashed(&why);
                Err(CallError::Trap(why))
            }
        };
        let _ = reply.send(out);
    }
}

/// Instantiate a plugin and call only `validate-config`, for `plexigraf config check`.
pub fn validate_config(plugin: Arc<Plugin>, id: &str, config_toml: String) -> Result<(), String> {
    let spec = InstanceSpec {
        id: id.to_string(),
        config_toml,
        timeout: Duration::from_secs(5),
        health: Arc::default(),
    };
    Actor::new(plugin, spec).instantiate(false).map(|_| ())
}

impl Plugin {
    /// The scope for metrics this plugin creates, such as `plexigraf/receivers/cpu`.
    fn scope_name(&self) -> String {
        let kind = match self.world {
            World::Scraper => "receivers",
            World::Processor => "processors",
            World::Exporter => "exporters",
        };
        format!("plexigraf/{kind}/{}", self.name)
    }
}

fn describe_trap(e: &wasmtime::Error, timeout: Duration) -> String {
    if let Some(wasmtime::Trap::Interrupt) = e.downcast_ref::<wasmtime::Trap>() {
        return format!("plugin overran its {timeout:?} deadline and was interrupted");
    }
    format!("plugin trapped: {e:#}")
}

/// Take ownership of a batch a plugin returned, then fill defaults and check
/// its invariants before it enters the pipeline.
fn finish_batch(
    store: &mut Store<PluginState>,
    r: wasmtime::Result<Result<Resource<BatchData>, PluginError>>,
    scope: &(String, String),
) -> wasmtime::Result<Result<BatchData, PluginError>> {
    let res = match r? {
        Ok(res) => res,
        Err(e) => return Ok(Err(e)),
    };
    let mut b = store.data_mut().take_batch(res)?;
    b.normalize(&scope.0, &scope.1);
    Ok(b.validate()
        .map(|()| b)
        .map_err(|e| PluginError::Transient(format!("invalid batch: {e}"))))
}

fn reply_err(req: Request, e: CallError) {
    match req {
        Request::Scrape(_, r) | Request::Process(_, r) | Request::Flush(_, r) => {
            let _ = r.send(Err(e));
        }
        Request::Export(_, r) => {
            let _ = r.send(Err(e));
        }
        Request::Shutdown(r) => {
            let _ = r.send(());
        }
    }
}

/// Plugins loaded by name, and the `(name, error)` of each that failed.
type LoadedPlugins = (BTreeMap<String, Arc<Plugin>>, Vec<(String, String)>);

/// Load every plugin the active components use, once each.
pub fn load_plugins(host: &Arc<HostEngine>, cfg: &crate::config::Config) -> LoadedPlugins {
    let mut out = BTreeMap::new();
    let mut failed = Vec::new();
    for c in cfg.active_components() {
        let Some(decl) = cfg.plugins.get(&c.plugin) else {
            continue;
        };
        if out.contains_key(&decl.name) || failed.iter().any(|(n, _)| n == &decl.name) {
            continue;
        }
        match Plugin::load(host, &decl.name, &decl.path, decl.memory_limit_mib) {
            Ok(p) => {
                tracing::info!(plugin = %decl.name, world = p.world.as_str(), path = %decl.path.display(), "loaded plugin");
                out.insert(decl.name.clone(), Arc::new(p));
            }
            Err(e) => failed.push((decl.name.clone(), format!("{e:#}"))),
        }
    }
    (out, failed)
}
