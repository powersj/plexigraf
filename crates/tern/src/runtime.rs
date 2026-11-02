//! The metric pipeline: scrapers on timers, one task per pipeline running its
//! processors, and one sending queue plus worker per exporter.
//!
//! Every batch carries a delivery tracker. Fan-out shares the batch and
//! clones the tracker; when the last copy is exported, rejected, or dropped,
//! the tracker resolves and the receiver learns the outcome. Shutdown drains
//! in graph order: scrapers stop, which closes pipeline inputs, which closes
//! exporter queues once every pipeline feeding them has finished.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinSet;

use crate::bindings::types::{Delivery, DeliveryStatus, ExportOutcome, PluginError, Status};
use crate::config::{Acknowledge, Component, Config, Kind, RetrySettings, WhenFull};
use crate::encoding::EncodingConfig;
use crate::health::{self, Health};
use crate::model::BatchData;
use crate::plugin::{CallError, HostEngine, Instance, InstanceSpec, Plugin, World};
use crate::telemetry;

const PIPELINE_CHANNEL: usize = 16;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

// ---- Delivery tracking ----

type OnDone = Box<dyn FnOnce(Delivery) + Send>;

struct TrackerInner {
    ok: AtomicU64,
    failed: AtomicU64,
    rejected: AtomicU64,
    on_done: Mutex<Option<OnDone>>,
}

/// Shared by every copy of a batch. Resolves when the last copy is dropped.
#[derive(Clone)]
pub struct Tracker(Arc<TrackerInner>);

impl Tracker {
    pub fn new(on_done: impl FnOnce(Delivery) + Send + 'static) -> Tracker {
        Tracker(Arc::new(TrackerInner {
            ok: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            on_done: Mutex::new(Some(Box::new(on_done))),
        }))
    }

    pub fn exported(&self) {
        self.0.ok.fetch_add(1, Ordering::Relaxed);
    }

    pub fn partial(&self, rejected: u64) {
        self.0.ok.fetch_add(1, Ordering::Relaxed);
        self.0.rejected.fetch_add(rejected, Ordering::Relaxed);
    }

    pub fn failed(&self, points: u64) {
        self.0.failed.fetch_add(1, Ordering::Relaxed);
        self.0.rejected.fetch_add(points, Ordering::Relaxed);
    }
}

impl Drop for TrackerInner {
    fn drop(&mut self) {
        let (ok, failed, rejected) = (*self.ok.get_mut(), *self.failed.get_mut(), *self.rejected.get_mut());
        // A copy dropped by a filter counts as delivered, so no outcomes at all
        // also means delivered.
        let status = if failed > 0 && ok == 0 {
            DeliveryStatus::Failed
        } else if failed > 0 || rejected > 0 {
            DeliveryStatus::Partial
        } else {
            DeliveryStatus::Delivered
        };
        if let Some(f) = self.on_done.get_mut().unwrap().take() {
            f(Delivery {
                status,
                rejected_data_points: rejected,
            });
        }
    }
}

#[derive(Clone)]
pub struct Envelope {
    pub batch: Arc<BatchData>,
    pub tracker: Option<Tracker>,
}

impl Envelope {
    /// Take the batch for a component that changes it: free if this is the
    /// only copy, a clone if fan-out still shares it.
    fn take_batch(batch: Arc<BatchData>) -> BatchData {
        Arc::try_unwrap(batch).unwrap_or_else(|shared| (*shared).clone())
    }
}

// ---- Sending queue ----

struct QueueState {
    items: VecDeque<Envelope>,
    closed: bool,
}

/// An exporter's bounded queue, shared by every pipeline that feeds it.
pub struct SendingQueue {
    id: String,
    capacity: usize,
    when_full: WhenFull,
    state: Mutex<QueueState>,
    has_items: Notify,
    has_space: Notify,
}

impl SendingQueue {
    pub fn new(id: &str, capacity: usize, when_full: WhenFull) -> Arc<SendingQueue> {
        Arc::new(SendingQueue {
            id: id.to_string(),
            capacity,
            when_full,
            state: Mutex::new(QueueState {
                items: VecDeque::new(),
                closed: false,
            }),
            has_items: Notify::new(),
            has_space: Notify::new(),
        })
    }

    fn dropped(&self, env: Envelope) {
        let points = env.batch.data_point_count();
        telemetry::component(
            "tern.exporter.enqueue_failed_metric_points",
            "exporter",
            &self.id,
            points,
        );
        if let Some(t) = &env.tracker {
            t.failed(points);
        }
    }

    pub async fn push(&self, env: Envelope) {
        loop {
            let space = self.has_space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            {
                let mut st = self.state.lock().unwrap();
                if st.closed {
                    drop(st);
                    self.dropped(env);
                    return;
                }
                if st.items.len() < self.capacity {
                    st.items.push_back(env);
                    drop(st);
                    self.has_items.notify_one();
                    return;
                }
                match self.when_full {
                    WhenFull::Block => {}
                    WhenFull::DropNewest => {
                        drop(st);
                        self.dropped(env);
                        return;
                    }
                    WhenFull::DropOldest => {
                        let old = st.items.pop_front();
                        st.items.push_back(env);
                        drop(st);
                        if let Some(old) = old {
                            self.dropped(old);
                        }
                        self.has_items.notify_one();
                        return;
                    }
                }
            }
            space.await;
        }
    }

    pub async fn pop(&self) -> Option<Envelope> {
        loop {
            let items = self.has_items.notified();
            tokio::pin!(items);
            items.as_mut().enable();
            {
                let mut st = self.state.lock().unwrap();
                if let Some(env) = st.items.pop_front() {
                    drop(st);
                    self.has_space.notify_one();
                    return Some(env);
                }
                if st.closed {
                    return None;
                }
            }
            items.await;
        }
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.has_items.notify_waiters();
        self.has_space.notify_waiters();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().items.len()
    }
}

/// Held by each pipeline that feeds a queue; the last one closes it.
pub struct QueueProducer(Arc<SendingQueue>);

impl Drop for QueueProducer {
    fn drop(&mut self) {
        self.0.close();
    }
}

// ---- Built-in exporters ----

enum ExporterImpl {
    Wasm(Instance),
    Debug(EncodingConfig),
    File {
        path: PathBuf,
        encoding: EncodingConfig,
        file: std::fs::File,
    },
}

impl ExporterImpl {
    fn builtin(c: &Component) -> Result<ExporterImpl> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct DebugSettings {
            encoding: Option<toml::Value>,
            #[serde(default)]
            verbosity: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FileSettings {
            path: String,
            encoding: Option<toml::Value>,
        }
        let settings = toml::Value::Table(c.settings.clone());
        let label = c.label();
        match c.plugin.as_str() {
            "debug" => {
                let s: DebugSettings = settings.try_into().map_err(|e| anyhow!("[{label}]: {e}"))?;
                let _ = s.verbosity;
                Ok(ExporterImpl::Debug(
                    EncodingConfig::from_value(s.encoding.as_ref(), "influx").map_err(|e| anyhow!("[{label}]: {e}"))?,
                ))
            }
            "file" => {
                let s: FileSettings = settings.try_into().map_err(|e| anyhow!("[{label}]: {e}"))?;
                let encoding =
                    EncodingConfig::from_value(s.encoding.as_ref(), "json").map_err(|e| anyhow!("[{label}]: {e}"))?;
                let mut path = PathBuf::from(&s.path);
                if path.is_relative() {
                    path = c.file.parent().unwrap_or(std::path::Path::new(".")).join(path);
                }
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| anyhow!("[{label}]: opening {}: {e}", path.display()))?;
                Ok(ExporterImpl::File { path, encoding, file })
            }
            other => bail!("[{label}]: unknown built-in exporter {other:?}"),
        }
    }

    async fn export(&mut self, batch: &Arc<BatchData>) -> Result<ExportOutcome, CallError> {
        let write = |out: &mut dyn Write, enc: &EncodingConfig| -> Result<ExportOutcome, CallError> {
            let bytes = enc
                .encode(batch)
                .map_err(|e| CallError::Plugin(PluginError::Fatal(e)))?;
            out.write_all(&bytes)
                .and_then(|_| out.flush())
                .map(|_| ExportOutcome::Ok)
                .map_err(|e| CallError::Plugin(PluginError::Transient(e.to_string())))
        };
        match self {
            ExporterImpl::Wasm(i) => i.export((**batch).clone()).await,
            ExporterImpl::Debug(enc) => write(&mut std::io::stdout().lock(), enc),
            ExporterImpl::File { encoding, file, path } => {
                let _ = path;
                write(file, encoding)
            }
        }
    }

    async fn shutdown(&self) {
        if let ExporterImpl::Wasm(i) = self {
            i.shutdown().await;
        }
    }
}

enum Outcome {
    Exported,
    Partial(u64),
    Failed,
}

async fn export_with_retry(id: &str, imp: &mut ExporterImpl, batch: &Arc<BatchData>, retry: &RetrySettings) -> Outcome {
    let started = Instant::now();
    let mut delay = retry.initial_interval;
    loop {
        let wait = match imp.export(batch).await {
            Ok(ExportOutcome::Ok) => return Outcome::Exported,
            Ok(ExportOutcome::Partial(p)) => {
                tracing::warn!(
                    exporter = id,
                    rejected = p.rejected_data_points,
                    "partial export: {}",
                    p.reason
                );
                return Outcome::Partial(p.rejected_data_points);
            }
            Ok(ExportOutcome::Reject(reason)) => {
                tracing::warn!(exporter = id, "batch rejected: {reason}");
                return Outcome::Failed;
            }
            Ok(ExportOutcome::Retry(after_ms)) => after_ms.map(|ms| Duration::from_millis(ms as u64)),
            Err(CallError::Plugin(PluginError::Transient(e))) => {
                tracing::warn!(exporter = id, "export failed, will retry: {e}");
                None
            }
            Err(CallError::Trap(_)) => None,
            Err(e @ CallError::Unavailable(_)) if e.to_string().contains("restarting") => None,
            Err(e) => {
                tracing::error!(exporter = id, "export failed: {e}");
                return Outcome::Failed;
            }
        };
        let wait = wait.unwrap_or(delay).max(delay);
        if !retry.enabled || started.elapsed() + wait > retry.max_elapsed_time {
            tracing::error!(exporter = id, elapsed = ?started.elapsed(), "giving up on batch after retries");
            return Outcome::Failed;
        }
        tokio::time::sleep(wait).await;
        delay = (delay * 2).min(retry.max_interval);
    }
}

async fn exporter_worker(id: String, queue: Arc<SendingQueue>, mut imp: ExporterImpl, retry: RetrySettings) {
    while let Some(env) = queue.pop().await {
        let points = env.batch.data_point_count();
        match export_with_retry(&id, &mut imp, &env.batch, &retry).await {
            Outcome::Exported => {
                telemetry::component("tern.exporter.sent_metric_points", "exporter", &id, points);
                env.tracker.inspect(Tracker::exported);
            }
            Outcome::Partial(rejected) => {
                telemetry::component(
                    "tern.exporter.sent_metric_points",
                    "exporter",
                    &id,
                    points.saturating_sub(rejected),
                );
                telemetry::component("tern.exporter.send_failed_metric_points", "exporter", &id, rejected);
                env.tracker.inspect(|t| t.partial(rejected));
            }
            Outcome::Failed => {
                telemetry::component("tern.exporter.send_failed_metric_points", "exporter", &id, points);
                env.tracker.inspect(|t| t.failed(points));
            }
        }
    }
    imp.shutdown().await;
}

// ---- Pipelines ----

enum Output {
    Exporter {
        producer: Arc<QueueProducer>,
        ack: Acknowledge,
    },
    Connector {
        downstream: Vec<mpsc::Sender<Envelope>>,
    },
}

struct PipelineTask {
    name: String,
    processors: Vec<Instance>,
    outputs: Vec<Output>,
    flush_every: Option<Duration>,
}

impl PipelineTask {
    async fn run(self, mut rx: mpsc::Receiver<Envelope>) {
        let mut ticker = self.flush_every.map(|d| {
            let mut t = tokio::time::interval_at(tokio::time::Instant::now() + d, d);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            t
        });
        loop {
            let tick = async {
                match ticker.as_mut() {
                    Some(t) => {
                        t.tick().await;
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                env = rx.recv() => match env {
                    Some(env) => self.handle(env, 0).await,
                    None => break,
                },
                _ = tick => self.flush_all().await,
            }
        }
        // Close any open aggregation windows before draining.
        self.flush_all().await;
        tracing::debug!(pipeline = %self.name, "pipeline drained");
    }

    async fn handle(&self, env: Envelope, start: usize) {
        let Envelope { mut batch, tracker } = env;
        for p in &self.processors[start..] {
            let points = batch.data_point_count();
            match p.process(Envelope::take_batch(batch)).await {
                Ok(out) if out.is_empty() => return, // dropped by policy counts as delivered
                Ok(out) => batch = Arc::new(out),
                Err(e) => {
                    tracing::warn!(pipeline = %self.name, processor = %p.id, "processor failed, dropping batch: {e}");
                    tracker.inspect(|t| t.failed(points));
                    return;
                }
            }
        }
        self.emit(Envelope { batch, tracker }).await;
    }

    async fn flush_all(&self) {
        for (i, p) in self.processors.iter().enumerate().filter(|(_, p)| p.flush) {
            match p.flush_now(now_unix_nano(Duration::ZERO)).await {
                Ok(b) if b.is_empty() => {}
                Ok(b) => {
                    Box::pin(self.handle(
                        Envelope {
                            batch: Arc::new(b),
                            tracker: None,
                        },
                        i + 1,
                    ))
                    .await
                }
                Err(e) => tracing::warn!(pipeline = %self.name, processor = %p.id, "flush failed: {e}"),
            }
        }
    }

    async fn emit(&self, env: Envelope) {
        for out in &self.outputs {
            match out {
                Output::Exporter { producer, ack } => {
                    let mut copy = env.clone();
                    if *ack == Acknowledge::Enqueued {
                        // Counted as delivered once queued.
                        copy.tracker.take().inspect(Tracker::exported);
                    }
                    producer.0.push(copy).await;
                }
                Output::Connector { downstream } => {
                    for tx in downstream {
                        let _ = tx.send(env.clone()).await;
                    }
                }
            }
        }
    }
}

// ---- Scrapers ----

fn now_unix_nano(precision: Duration) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let p = precision.as_nanos() as u64;
    if p > 1 { now - now % p } else { now }
}

struct ScrapeTask {
    id: String,
    instance: Instance,
    interval: Duration,
    jitter: Duration,
    precision: Duration,
    outputs: Vec<mpsc::Sender<Envelope>>,
}

impl ScrapeTask {
    async fn run(self, mut shutdown: watch::Receiver<bool>, once: bool) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            if !once && !self.jitter.is_zero() {
                let j = Duration::from_nanos(crate::util::random_u64() % self.jitter.as_nanos().max(1) as u64);
                tokio::select! {
                    _ = tokio::time::sleep(j) => {}
                    _ = shutdown.changed() => break,
                }
            }
            if !self.scrape_once(&mut shutdown).await || once {
                break;
            }
        }
    }

    /// Returns false when shutdown interrupted delivery.
    async fn scrape_once(&self, shutdown: &mut watch::Receiver<bool>) -> bool {
        let batch = match self.instance.scrape(now_unix_nano(self.precision)).await {
            Ok(b) => b,
            Err(e) => {
                telemetry::component("tern.scraper.errored_scrapes", "receiver", &self.id, 1);
                if !matches!(e, CallError::Trap(_)) {
                    tracing::warn!(receiver = %self.id, "scrape failed: {e}");
                }
                return true;
            }
        };
        if batch.is_empty() {
            return true;
        }
        let points = batch.data_point_count();
        telemetry::component("tern.receiver.accepted_metric_points", "receiver", &self.id, points);
        let id = self.id.clone();
        let tracker = Tracker::new(move |d| match d.status {
            DeliveryStatus::Delivered => tracing::trace!(receiver = %id, points, "batch delivered"),
            DeliveryStatus::Partial => {
                tracing::debug!(receiver = %id, rejected = d.rejected_data_points, "batch partially delivered")
            }
            DeliveryStatus::Failed => tracing::debug!(receiver = %id, points, "batch failed delivery"),
        });
        let env = Envelope {
            batch: Arc::new(batch),
            tracker: Some(tracker),
        };
        for tx in &self.outputs {
            tokio::select! {
                _ = tx.send(env.clone()) => {}
                _ = shutdown.changed() => return false,
            }
        }
        true
    }
}

// ---- Assembly ----

pub enum Mode {
    Run,
    Once,
}

struct Started {
    instances: Vec<Instance>,
    failures: Vec<String>,
}

impl Started {
    fn fail(&mut self, health: &Health, c: &Component, why: String) {
        tracing::error!(component = %c.label(), "component failed to start and is isolated: {why}");
        health.set(&c.label(), Status::PermanentError, Some(why.clone()));
        self.failures.push(format!("{}: {why}", c.label()));
    }
}

async fn start_wasm(
    plugins: &BTreeMap<String, Arc<Plugin>>,
    plugin_errors: &BTreeMap<String, String>,
    c: &Component,
    label: String,
    want: World,
    timeout: Duration,
    health: &Arc<Health>,
) -> Result<Instance, String> {
    if let Some(e) = plugin_errors.get(&c.plugin) {
        return Err(e.clone());
    }
    let plugin = plugins
        .get(&c.plugin)
        .ok_or_else(|| format!("plugin {} is not loaded", c.plugin))?;
    if plugin.world != want {
        return Err(format!(
            "plugin {} is a {}, but [{}] needs a {}",
            c.plugin,
            plugin.world.as_str(),
            c.label(),
            want.as_str()
        ));
    }
    let config_toml = toml::to_string(&c.settings).map_err(|e| e.to_string())?;
    health.set(&label, Status::Starting, None);
    Instance::spawn(
        plugin.clone(),
        InstanceSpec {
            id: label,
            config_toml,
            timeout,
            health: health.clone(),
        },
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

pub async fn run(cfg: Config, mode: Mode, strict: bool) -> Result<()> {
    let health = Arc::new(Health::default());
    let host = HostEngine::new()?;
    let (plugins, plugin_failures) = tokio::task::block_in_place(|| crate::plugin::load_plugins(&host, &cfg));
    let plugin_errors: BTreeMap<String, String> = plugin_failures.into_iter().collect();
    for (name, e) in &plugin_errors {
        tracing::error!(plugin = %name, "{e}");
    }
    let mut started = Started {
        instances: Vec::new(),
        failures: Vec::new(),
    };

    // Extensions and the agent's own telemetry endpoint.
    if matches!(mode, Mode::Run) {
        for id in &cfg.service.extensions {
            let c = cfg.component(Kind::Extension, id).expect("checked in config");
            let endpoint = c
                .settings
                .get("endpoint")
                .and_then(|v| v.as_str())
                .unwrap_or("localhost:13133");
            let h = health.clone();
            health::serve(health::loopback_addr(endpoint)?, "health_check", move |_| {
                let (ok, body) = h.report();
                (if ok { 200 } else { 503 }, "application/json", body)
            })
            .await?;
        }
        if let Some(endpoint) = &cfg.service.metrics_endpoint {
            health::serve(health::loopback_addr(endpoint)?, "telemetry", |path| match path {
                "/metrics" => (200, "text/plain; version=0.0.4", telemetry::prometheus_text()),
                _ => (404, "text/plain", "not found\n".into()),
            })
            .await?;
        }
    }

    // Exporters: one sending queue and worker each.
    let mut tasks = JoinSet::new();
    let mut exporter_tasks = JoinSet::new();
    let mut queues: BTreeMap<String, Arc<QueueProducer>> = BTreeMap::new();
    for c in cfg.active_components().into_iter().filter(|c| c.kind == Kind::Exporter) {
        let imp = if c.is_builtin(&cfg.plugins) {
            ExporterImpl::builtin(c).map_err(|e| format!("{e:#}"))
        } else {
            let timeout = c.timeout.unwrap_or(DEFAULT_TIMEOUT);
            start_wasm(
                &plugins,
                &plugin_errors,
                c,
                c.label(),
                World::Exporter,
                timeout,
                &health,
            )
            .await
            .map(|i| {
                started.instances.push(i.clone());
                ExporterImpl::Wasm(i)
            })
        };
        match imp {
            Ok(imp) => {
                let q = SendingQueue::new(&c.id, c.sending_queue.queue_size, c.sending_queue.when_full);
                queues.insert(c.id.clone(), Arc::new(QueueProducer(q.clone())));
                exporter_tasks.spawn(exporter_worker(c.id.clone(), q, imp, c.retry.clone()));
                if c.is_builtin(&cfg.plugins) {
                    health.set(&c.label(), Status::Ok, None);
                }
            }
            Err(e) => started.fail(&health, c, e),
        }
    }

    // Pipeline inputs, created up front so connectors can point at them.
    let mut inputs: BTreeMap<String, (mpsc::Sender<Envelope>, mpsc::Receiver<Envelope>)> = BTreeMap::new();
    for p in &cfg.pipelines {
        inputs.insert(p.name.clone(), mpsc::channel(PIPELINE_CHANNEL));
    }
    let sources_for = |id: &str| -> Vec<mpsc::Sender<Envelope>> {
        cfg.pipelines
            .iter()
            .filter(|p| p.receivers.iter().any(|r| r == id))
            .map(|p| inputs[&p.name].0.clone())
            .collect()
    };

    let mut pipeline_tasks = Vec::new();
    for p in &cfg.pipelines {
        let mut processors = Vec::new();
        let mut ok = true;
        for id in &p.processors {
            let c = cfg.component(Kind::Processor, id).expect("checked in config");
            // One instance per pipeline, so state is never shared.
            let label = format!("{}@{}", c.label(), p.name);
            let timeout = c.timeout.unwrap_or(DEFAULT_TIMEOUT);
            match start_wasm(&plugins, &plugin_errors, c, label, World::Processor, timeout, &health).await {
                Ok(i) => {
                    started.instances.push(i.clone());
                    processors.push(i);
                }
                Err(e) => {
                    started.fail(&health, c, e);
                    ok = false;
                }
            }
        }
        if !ok {
            tracing::error!(pipeline = %p.name, "pipeline disabled because a processor failed to start");
            continue;
        }
        let mut outputs = Vec::new();
        for id in &p.exporters {
            if cfg.is_connector(id) {
                outputs.push(Output::Connector {
                    downstream: sources_for(id),
                });
            } else if let Some(producer) = queues.get(id) {
                let ack = cfg
                    .component(Kind::Exporter, id)
                    .map(|c| c.acknowledge)
                    .unwrap_or_default();
                outputs.push(Output::Exporter {
                    producer: producer.clone(),
                    ack,
                });
            } else {
                tracing::warn!(pipeline = %p.name, exporter = %id, "skipping exporter that failed to start");
            }
        }
        let flush_every = processors.iter().any(|i| i.flush).then(|| {
            p.processors
                .iter()
                .filter_map(|id| cfg.component(Kind::Processor, id).and_then(|c| c.collection_interval))
                .min()
                .unwrap_or(cfg.service.collection_interval)
        });
        pipeline_tasks.push((
            p.name.clone(),
            PipelineTask {
                name: p.name.clone(),
                processors,
                outputs,
                flush_every,
            },
        ));
    }

    // Scrapers.
    let mut scrapers = Vec::new();
    for c in cfg.active_components().into_iter().filter(|c| c.kind == Kind::Receiver) {
        let interval = c.collection_interval.unwrap_or(cfg.service.collection_interval);
        let timeout = c.timeout.unwrap_or(interval);
        match start_wasm(&plugins, &plugin_errors, c, c.label(), World::Scraper, timeout, &health).await {
            Ok(instance) => {
                started.instances.push(instance.clone());
                scrapers.push(ScrapeTask {
                    id: c.id.clone(),
                    instance,
                    interval,
                    jitter: cfg.service.collection_jitter,
                    precision: cfg.service.precision,
                    outputs: sources_for(&c.id),
                });
            }
            Err(e) => started.fail(&health, c, e),
        }
    }

    if strict && !started.failures.is_empty() {
        for i in &started.instances {
            i.shutdown().await;
        }
        bail!(
            "--strict: {} component(s) failed to start:\n  {}",
            started.failures.len(),
            started.failures.join("\n  ")
        );
    }

    // Start pipelines; then drop our own senders so the graph owns them all.
    for (name, task) in pipeline_tasks {
        let (_, rx) = inputs.remove(&name).expect("input exists");
        tasks.spawn(task.run(rx));
    }
    drop(inputs);
    drop(queues);
    for c in cfg.unused_components() {
        tracing::info!(component = %c.label(), "defined but in no pipeline; not started");
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let once = matches!(mode, Mode::Once);
    let mut scraper_tasks = JoinSet::new();
    let scraper_count = scrapers.len();
    for s in scrapers {
        scraper_tasks.spawn(s.run(stop_rx.clone(), once));
    }
    tracing::info!(
        scrapers = scraper_count,
        pipelines = cfg.pipelines.len(),
        "tern is running"
    );

    if once {
        while scraper_tasks.join_next().await.is_some() {}
    } else {
        wait_for_signal().await;
        tracing::info!("shutting down; draining pipelines");
        let _ = stop_tx.send(true);
        tokio::select! {
            _ = async { while scraper_tasks.join_next().await.is_some() {} } => {}
            _ = wait_for_signal() => std::process::exit(130),
        }
    }
    drop(stop_tx);

    let drain = async {
        while tasks.join_next().await.is_some() {}
        while exporter_tasks.join_next().await.is_some() {}
    };
    if once {
        drain.await;
    } else {
        tokio::select! {
            _ = drain => {}
            _ = wait_for_signal() => std::process::exit(130),
        }
    }
    for i in &started.instances {
        if i.world != World::Exporter {
            i.shutdown().await;
        }
    }
    if !started.failures.is_empty() && once {
        bail!("{} component(s) failed to start", started.failures.len());
    }
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::metrics::{Data, DataPointFlags, Gauge, Metric, Number, NumberDataPoint};

    fn batch(points: usize) -> Arc<BatchData> {
        let mut b = BatchData::default();
        b.metrics.push(Metric {
            resource: 0,
            scope: 0,
            name: "m".into(),
            description: String::new(),
            unit: String::new(),
            data: Data::Gauge(Gauge {
                data_points: (0..points)
                    .map(|i| NumberDataPoint {
                        attributes: vec![],
                        start_time_unix_nano: 0,
                        time_unix_nano: i as u64,
                        value: Number::Int(1),
                        exemplars: vec![],
                        flags: DataPointFlags::empty(),
                    })
                    .collect(),
            }),
            metadata: vec![],
        });
        b.normalize("test", "");
        Arc::new(b)
    }

    fn tracked() -> (Tracker, Arc<Mutex<Option<Delivery>>>) {
        let out = Arc::new(Mutex::new(None));
        let o = out.clone();
        (Tracker::new(move |d| *o.lock().unwrap() = Some(d)), out)
    }

    #[test]
    fn tracker_resolves_when_every_copy_is_done() {
        let (t, out) = tracked();
        let a = t.clone();
        let b = t.clone();
        drop(t);
        a.exported();
        drop(a);
        assert!(out.lock().unwrap().is_none(), "one copy is still in flight");
        b.failed(3);
        drop(b);
        let d = (*out.lock().unwrap()).unwrap();
        assert_eq!(d.status, DeliveryStatus::Partial);
        assert_eq!(d.rejected_data_points, 3);

        let (t, out) = tracked();
        t.failed(2);
        drop(t);
        assert_eq!(out.lock().unwrap().as_ref().unwrap().status, DeliveryStatus::Failed);

        // Dropped by a filter: delivered.
        let (t, out) = tracked();
        drop(t);
        assert_eq!(out.lock().unwrap().as_ref().unwrap().status, DeliveryStatus::Delivered);
    }

    #[tokio::test]
    async fn queue_overflow_policies() {
        for (policy, expect_first) in [(WhenFull::DropNewest, 0u64), (WhenFull::DropOldest, 1)] {
            let q = SendingQueue::new("t", 2, policy);
            let mut outcomes = Vec::new();
            for i in 0..3u64 {
                let (t, out) = tracked();
                outcomes.push(out);
                let mut b = (*batch(1)).clone();
                if let Data::Gauge(g) = &mut b.metrics[0].data {
                    g.data_points[0].time_unix_nano = i;
                }
                q.push(Envelope {
                    batch: Arc::new(b),
                    tracker: Some(t),
                })
                .await;
            }
            assert_eq!(q.len(), 2);
            let first = q.pop().await.unwrap();
            let Data::Gauge(g) = &first.batch.metrics[0].data else {
                panic!()
            };
            assert_eq!(g.data_points[0].time_unix_nano, expect_first, "{policy:?}");
            let dropped = if policy == WhenFull::DropNewest { 2 } else { 0 };
            assert_eq!(
                outcomes[dropped].lock().unwrap().as_ref().unwrap().status,
                DeliveryStatus::Failed
            );
        }
    }

    #[tokio::test]
    async fn blocking_queue_waits_and_close_drains() {
        let q = SendingQueue::new("t", 1, WhenFull::Block);
        q.push(Envelope {
            batch: batch(1),
            tracker: None,
        })
        .await;
        let q2 = q.clone();
        let pusher = tokio::spawn(async move {
            q2.push(Envelope {
                batch: batch(2),
                tracker: None,
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!pusher.is_finished(), "push should block while full");
        assert!(q.pop().await.is_some());
        pusher.await.unwrap();
        q.close();
        assert_eq!(q.pop().await.unwrap().batch.data_point_count(), 2);
        assert!(q.pop().await.is_none());
    }

    #[test]
    fn copy_on_write_only_copies_shared_batches() {
        let shared = batch(1);
        let other = shared.clone();
        let mut copy = Envelope::take_batch(shared);
        copy.metrics.clear();
        assert_eq!(other.metrics.len(), 1, "the other copy is untouched");
    }
}
