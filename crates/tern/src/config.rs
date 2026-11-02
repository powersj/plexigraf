//! Configuration: plugins, components, pipelines, and service settings.
//!
//! Loading happens in three steps. Each file is parsed and its `${env:VAR}`
//! references are substituted inside string values. All files are merged into
//! one namespace, where a duplicate ID is an error naming both files. Then the
//! merged tables become typed components and resolved pipelines, and the
//! pipeline graph is checked for unknown references and cycles.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::util::{glob_match, is_glob, parse_duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Receiver,
    Processor,
    Exporter,
    Connector,
    Extension,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Receiver,
        Kind::Processor,
        Kind::Exporter,
        Kind::Connector,
        Kind::Extension,
    ];

    fn table(self) -> &'static str {
        match self {
            Kind::Receiver => "receivers",
            Kind::Processor => "processors",
            Kind::Exporter => "exporters",
            Kind::Connector => "connectors",
            Kind::Extension => "extensions",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Receiver => "receiver",
            Kind::Processor => "processor",
            Kind::Exporter => "exporter",
            Kind::Connector => "connector",
            Kind::Extension => "extension",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Components the core provides without a plugin file.
pub fn builtin(kind: Kind, plugin: &str) -> bool {
    matches!(
        (kind, plugin),
        (Kind::Exporter, "debug" | "file") | (Kind::Connector, "forward") | (Kind::Extension, "health_check")
    )
}

#[derive(Debug)]
pub struct Config {
    pub service: Service,
    pub plugins: BTreeMap<String, PluginDecl>,
    pub components: BTreeMap<(Kind, String), Component>,
    pub pipelines: Vec<Pipeline>,
}

#[derive(Debug)]
pub struct Service {
    pub extensions: Vec<String>,
    pub collection_interval: Duration,
    pub collection_jitter: Duration,
    pub precision: Duration,
    pub log_level: Option<String>,
    pub metrics_endpoint: Option<String>,
}

#[derive(Debug)]
pub struct PluginDecl {
    pub name: String,
    pub path: PathBuf,
    pub memory_limit_mib: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Component {
    pub id: String,
    pub kind: Kind,
    pub plugin: String,
    /// The plugin's own settings: the table minus reserved keys.
    pub settings: toml::Table,
    pub file: PathBuf,
    pub routing_labels: BTreeMap<String, String>,
    pub collection_interval: Option<Duration>,
    pub timeout: Option<Duration>,
    pub sending_queue: QueueSettings,
    pub retry: RetrySettings,
    pub acknowledge: Acknowledge,
}

impl Component {
    pub fn label(&self) -> String {
        format!("{}.{}", self.kind.table(), self.id)
    }

    pub fn is_builtin(&self, plugins: &BTreeMap<String, PluginDecl>) -> bool {
        !plugins.contains_key(&self.plugin) && builtin(self.kind, &self.plugin)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenFull {
    Block,
    DropOldest,
    DropNewest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueSettings {
    #[serde(default = "default_queue_size")]
    pub queue_size: usize,
    #[serde(default = "default_when_full")]
    pub when_full: WhenFull,
    #[serde(default)]
    sizer: Option<String>,
    #[serde(default)]
    storage: Option<String>,
    #[serde(default)]
    per_pipeline: bool,
}

fn default_queue_size() -> usize {
    1000
}
fn default_when_full() -> WhenFull {
    WhenFull::Block
}

impl Default for QueueSettings {
    fn default() -> Self {
        QueueSettings {
            queue_size: default_queue_size(),
            when_full: default_when_full(),
            sizer: None,
            storage: None,
            per_pipeline: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetrySettings {
    pub enabled: bool,
    pub initial_interval: Duration,
    pub max_interval: Duration,
    pub max_elapsed_time: Duration,
}

impl Default for RetrySettings {
    fn default() -> Self {
        RetrySettings {
            enabled: true,
            initial_interval: Duration::from_secs(5),
            max_interval: Duration::from_secs(30),
            max_elapsed_time: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Acknowledge {
    Enqueued,
    #[default]
    Exported,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub name: String,
    /// Receiver and connector IDs, resolved from IDs, globs, and labels.
    pub receivers: Vec<String>,
    pub processors: Vec<String>,
    /// Exporter and connector IDs.
    pub exporters: Vec<String>,
}

/// Collect `*.toml` files from `--config` paths and `--config-dir`
/// directories, in a stable order.
pub fn collect_files(files: &[PathBuf], dirs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = files.to_vec();
    for dir in dirs {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("reading config directory {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        found.sort();
        out.extend(found);
    }
    if out.is_empty() {
        bail!(
            "no config given; pass --config <file> or --config-dir <dir> (`tern init` is planned to write a starter config)"
        );
    }
    Ok(out)
}

/// One table and the file it came from.
struct Entry {
    table: toml::Table,
    file: PathBuf,
}

#[derive(Default)]
struct Merged {
    service: Option<Entry>,
    plugins: BTreeMap<String, Entry>,
    components: BTreeMap<(Kind, String), Entry>,
    pipelines: BTreeMap<String, Entry>,
}

pub fn load(paths: &[PathBuf]) -> Result<Config> {
    let mut merged = Merged::default();
    for path in paths {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut table: toml::Table = text.parse().map_err(|e| anyhow!("{}: {e}", path.display()))?;
        substitute_table(&mut table).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        merge_file(&mut merged, table, path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    }
    build(merged)
}

#[cfg(test)]
pub fn load_str(text: &str) -> Result<Config> {
    let mut merged = Merged::default();
    let path = PathBuf::from("test.toml");
    let mut table: toml::Table = text.parse()?;
    substitute_table(&mut table).map_err(|e| anyhow!(e))?;
    merge_file(&mut merged, table, &path).map_err(|e| anyhow!(e))?;
    build(merged)
}

fn merge_file(merged: &mut Merged, table: toml::Table, path: &Path) -> Result<(), String> {
    for (key, value) in table {
        match key.as_str() {
            "service" => {
                let t = expect_table(value, "service")?;
                if let Some(prev) = &merged.service {
                    return Err(format!(
                        "only one [service] table is allowed; another is in {}",
                        prev.file.display()
                    ));
                }
                merged.service = Some(Entry {
                    table: t,
                    file: path.to_path_buf(),
                });
            }
            "plugins" => {
                for (id, v) in expect_table(value, "plugins")? {
                    insert_unique(&mut merged.plugins, id, expect_table(v, "plugins.*")?, path, "plugin")?;
                }
            }
            "pipelines" => {
                for (signal, v) in expect_table(value, "pipelines")? {
                    match signal.as_str() {
                        "metrics" => {}
                        "logs" | "traces" => {
                            return Err(format!(
                                "[pipelines.{signal}] is reserved for a future release; Tern 0.1 carries metrics only"
                            ));
                        }
                        _ => {
                            return Err(format!(
                                "unknown signal [pipelines.{signal}]; use [pipelines.metrics.<name>]"
                            ));
                        }
                    }
                    for (name, p) in expect_table(v, "pipelines.metrics")? {
                        insert_unique(
                            &mut merged.pipelines,
                            name,
                            expect_table(p, "pipelines.metrics.*")?,
                            path,
                            "pipeline",
                        )?;
                    }
                }
            }
            "inputs" | "outputs" | "aggregators" | "agent" | "global_tags" | "secretstores" => {
                return Err(telegraf_hint(&key));
            }
            other => {
                let Some(kind) = Kind::ALL.iter().copied().find(|k| k.table() == other) else {
                    return Err(format!(
                        "unknown top-level table [{other}]; expected service, plugins, receivers, processors, exporters, connectors, extensions, or pipelines"
                    ));
                };
                if kind == Kind::Processor && value.is_array() {
                    return Err(telegraf_hint("processors"));
                }
                for (id, v) in expect_table(value, other)? {
                    let t = expect_table(v, &format!("{other}.{id}"))?;
                    let key = (kind, id);
                    if let Some(prev) = merged.components.get(&key) {
                        return Err(format!(
                            "duplicate {} ID {:?}; it is also defined in {}",
                            kind,
                            key.1,
                            prev.file.display()
                        ));
                    }
                    merged.components.insert(
                        key,
                        Entry {
                            table: t,
                            file: path.to_path_buf(),
                        },
                    );
                }
            }
        }
    }
    Ok(())
}

fn telegraf_hint(key: &str) -> String {
    let tern = match key {
        "inputs" => "[receivers.<id>] plus a [pipelines.metrics.<name>] that lists it",
        "outputs" => "[exporters.<id>] plus a [pipelines.metrics.<name>] that lists it",
        "processors" => "[processors.<id>] listed in a pipeline's `processors`",
        "aggregators" => "a processor that implements flush, listed in a pipeline",
        "agent" => "[service] and [service.defaults]",
        "global_tags" => "a resource processor (planned) or resource attributes set by receivers",
        _ => "secret-store extensions (planned)",
    };
    format!(
        "this looks like a Telegraf config ([{key}] / [[{key}.*]]); the Tern equivalent is {tern}. `tern config migrate` is planned to convert Telegraf configs"
    )
}

fn insert_unique(
    map: &mut BTreeMap<String, Entry>,
    id: String,
    table: toml::Table,
    path: &Path,
    what: &str,
) -> Result<(), String> {
    if let Some(prev) = map.get(&id) {
        return Err(format!(
            "duplicate {what} {id:?}; it is also defined in {}",
            prev.file.display()
        ));
    }
    map.insert(
        id,
        Entry {
            table,
            file: path.to_path_buf(),
        },
    );
    Ok(())
}

fn expect_table(v: toml::Value, what: &str) -> Result<toml::Table, String> {
    match v {
        toml::Value::Table(t) => Ok(t),
        other => Err(format!("[{what}] must be a table, found {}", other.type_str())),
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn duration_value(v: &toml::Value, key: &str) -> Result<Duration, String> {
    match v {
        toml::Value::String(s) => parse_duration(s).map_err(|e| format!("`{key}`: {e}")),
        _ => Err(format!("`{key}` must be a duration string such as \"10s\"")),
    }
}

fn build(merged: Merged) -> Result<Config> {
    let service = build_service(merged.service.as_ref())?;

    let mut plugins = BTreeMap::new();
    for (name, entry) in merged.plugins {
        if !valid_id(&name) {
            bail!(
                "{}: plugin name {name:?} may use only a-z, 0-9, _ and -",
                entry.file.display()
            );
        }
        let decl =
            build_plugin(&name, &entry).map_err(|e| anyhow!("{}: [plugins.{name}]: {e}", entry.file.display()))?;
        plugins.insert(name, decl);
    }

    let mut components = BTreeMap::new();
    for ((kind, id), entry) in merged.components {
        let c = build_component(kind, &id, entry)?;
        components.insert((kind, id), c);
    }

    let connectors: BTreeSet<&String> = components
        .keys()
        .filter(|(k, _)| *k == Kind::Connector)
        .map(|(_, id)| id)
        .collect();
    for id in &connectors {
        for other in [Kind::Receiver, Kind::Exporter] {
            if components.contains_key(&(other, (*id).clone())) {
                bail!(
                    "connector ID {id:?} is also used by a {other}; connector IDs must be unique across receivers and exporters"
                );
            }
        }
    }

    let mut pipelines = Vec::new();
    for (name, entry) in merged.pipelines {
        let p = build_pipeline(&name, entry, &components)?;
        pipelines.push(p);
    }
    if pipelines.is_empty() {
        bail!(
            "no pipelines are defined, so nothing would flow; add a [pipelines.metrics.<name>] with receivers and exporters"
        );
    }

    let cfg = Config {
        service,
        plugins,
        components,
        pipelines,
    };
    cfg.check_graph()?;
    cfg.check_plugins()?;
    Ok(cfg)
}

fn build_service(entry: Option<&Entry>) -> Result<Service> {
    let mut svc = Service {
        extensions: Vec::new(),
        collection_interval: Duration::from_secs(10),
        collection_jitter: Duration::ZERO,
        precision: Duration::ZERO,
        log_level: None,
        metrics_endpoint: None,
    };
    let Some(entry) = entry else { return Ok(svc) };
    let ctx = |e: String| anyhow!("{}: [service]: {e}", entry.file.display());

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        #[serde(default)]
        extensions: Vec<String>,
        #[serde(default)]
        defaults: Defaults,
        #[serde(default)]
        telemetry: Telemetry,
    }
    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct Defaults {
        collection_interval: Option<String>,
        collection_jitter: Option<String>,
        precision: Option<String>,
    }
    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct Telemetry {
        metrics: Option<TelemetryMetrics>,
        logs: Option<TelemetryLogs>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TelemetryMetrics {
        #[serde(default)]
        level: Option<String>,
        endpoint: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TelemetryLogs {
        level: Option<String>,
    }

    let raw: Raw = toml::Value::Table(entry.table.clone())
        .try_into()
        .map_err(|e: toml::de::Error| ctx(e.to_string()))?;
    svc.extensions = raw.extensions;
    let d = |v: Option<String>, key: &str, default: Duration| -> Result<Duration> {
        v.map_or(Ok(default), |s| {
            parse_duration(&s).map_err(|e| ctx(format!("defaults.{key}: {e}")))
        })
    };
    svc.collection_interval = d(
        raw.defaults.collection_interval,
        "collection_interval",
        svc.collection_interval,
    )?;
    svc.collection_jitter = d(
        raw.defaults.collection_jitter,
        "collection_jitter",
        svc.collection_jitter,
    )?;
    svc.precision = d(raw.defaults.precision, "precision", svc.precision)?;
    if svc.collection_interval.is_zero() {
        return Err(ctx("defaults.collection_interval must be greater than zero".into()));
    }
    svc.log_level = raw.telemetry.logs.and_then(|l| l.level);
    if let Some(m) = raw.telemetry.metrics {
        let _ = m.level;
        svc.metrics_endpoint = m.endpoint;
    }
    Ok(svc)
}

fn build_plugin(name: &str, entry: &Entry) -> Result<PluginDecl, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        source: String,
        memory_limit_mib: Option<u64>,
    }
    let raw: Raw = toml::Value::Table(entry.table.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.to_string())?;
    let path = if let Some(p) = raw.source.strip_prefix("file://") {
        let p = PathBuf::from(p);
        if p.is_absolute() {
            p
        } else {
            entry.file.parent().unwrap_or(Path::new(".")).join(p)
        }
    } else if raw.source.starts_with("oci://") || raw.source.starts_with("https://") {
        return Err(format!(
            "source {:?}: OCI and HTTPS sources need signature verification, which is not implemented yet; use file://",
            raw.source
        ));
    } else {
        return Err(format!(
            "source {:?} must start with file://, https://, or oci://",
            raw.source
        ));
    };
    Ok(PluginDecl {
        name: name.to_string(),
        path,
        memory_limit_mib: raw.memory_limit_mib,
    })
}

fn build_component(kind: Kind, id: &str, entry: Entry) -> Result<Component> {
    let file = entry.file;
    let label = format!("{}: [{}.{id}]", file.display(), kind.table());
    if !valid_id(id) {
        bail!("{label}: IDs may use only a-z, 0-9, _ and -");
    }
    let mut settings = entry.table;
    let mut take = |k: &str| settings.remove(k);
    let err = |e: String| anyhow!("{label}: {e}");

    let plugin = match take("plugin") {
        None => id.to_string(),
        Some(toml::Value::String(s)) => s,
        Some(_) => return Err(err("`plugin` must be a string".into())),
    };
    let routing_labels = match take("routing_labels") {
        None => BTreeMap::new(),
        Some(v) => v
            .try_into()
            .map_err(|_| err("`routing_labels` must be a table of strings".into()))?,
    };
    let collection_interval = take("collection_interval")
        .map(|v| duration_value(&v, "collection_interval"))
        .transpose()
        .map_err(err)?;
    let timeout = take("timeout")
        .map(|v| duration_value(&v, "timeout"))
        .transpose()
        .map_err(err)?;
    let sending_queue: QueueSettings = match take("sending_queue") {
        None => QueueSettings::default(),
        Some(v) => v
            .try_into()
            .map_err(|e: toml::de::Error| err(format!("`sending_queue`: {e}")))?,
    };
    if sending_queue.storage.is_some() {
        return Err(err(
            "`sending_queue.storage` needs the file_storage extension, which is not implemented yet".into(),
        ));
    }
    if sending_queue.per_pipeline {
        return Err(err("`sending_queue.per_pipeline` is not implemented yet".into()));
    }
    if sending_queue.sizer.as_deref().is_some_and(|s| s != "requests") {
        return Err(err(
            "`sending_queue.sizer` supports only \"requests\" (batches) for now".into(),
        ));
    }
    if sending_queue.queue_size == 0 {
        return Err(err("`sending_queue.queue_size` must be at least 1".into()));
    }
    let retry = match take("retry_on_failure") {
        None => RetrySettings::default(),
        Some(v) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Raw {
                enabled: Option<bool>,
                initial_interval: Option<String>,
                max_interval: Option<String>,
                max_elapsed_time: Option<String>,
            }
            let raw: Raw = v
                .try_into()
                .map_err(|e: toml::de::Error| err(format!("`retry_on_failure`: {e}")))?;
            let mut r = RetrySettings::default();
            let d = |s: Option<String>, def: Duration| -> Result<Duration> {
                s.map_or(Ok(def), |s| {
                    parse_duration(&s).map_err(|e| err(format!("`retry_on_failure`: {e}")))
                })
            };
            r.enabled = raw.enabled.unwrap_or(true);
            r.initial_interval = d(raw.initial_interval, r.initial_interval)?;
            r.max_interval = d(raw.max_interval, r.max_interval)?;
            r.max_elapsed_time = d(raw.max_elapsed_time, r.max_elapsed_time)?;
            r
        }
    };
    let acknowledge = match take("acknowledge") {
        None => Acknowledge::default(),
        Some(v) => v
            .try_into()
            .map_err(|_| err("`acknowledge` must be \"enqueued\" or \"exported\"".into()))?,
    };
    if take("grant").is_some() {
        return Err(err(
            "`grant` is not supported yet: Tern 0.1 approves no capabilities, so there is nothing to narrow".into(),
        ));
    }
    if take("dead_letter").is_some() {
        return Err(err("`dead_letter` routing is not implemented yet".into()));
    }
    if matches!(take("share_instance"), Some(toml::Value::Boolean(true))) {
        return Err(err("`share_instance` is not implemented yet".into()));
    }
    take("max_undelivered_batches");
    if take("allow_no_match").is_some() {
        return Err(err(
            "`allow_no_match` belongs on a pipeline's receiver entry, not on a component".into(),
        ));
    }

    Ok(Component {
        id: id.to_string(),
        kind,
        plugin,
        settings,
        file,
        routing_labels,
        collection_interval,
        timeout,
        sending_queue,
        retry,
        acknowledge,
    })
}

fn build_pipeline(name: &str, entry: Entry, components: &BTreeMap<(Kind, String), Component>) -> Result<Pipeline> {
    let label = format!("{}: [pipelines.metrics.{name}]", entry.file.display());
    let err = |e: String| anyhow!("{label}: {e}");
    if !valid_id(name) {
        return Err(err("pipeline names may use only a-z, 0-9, _ and -".into()));
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ReceiverEntry {
        Id(String),
        Select {
            #[serde(default)]
            routing_labels: BTreeMap<String, String>,
            #[serde(default, rename = "match")]
            pattern: Option<String>,
            #[serde(default)]
            allow_no_match: bool,
        },
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        #[serde(default)]
        receivers: Vec<ReceiverEntry>,
        #[serde(default)]
        processors: Vec<String>,
        #[serde(default)]
        exporters: Vec<String>,
    }
    let raw: Raw = toml::Value::Table(entry.table)
        .try_into()
        .map_err(|e: toml::de::Error| err(e.to_string()))?;

    // Receivers and connectors can both feed a pipeline.
    let sources: Vec<&Component> = components
        .values()
        .filter(|c| matches!(c.kind, Kind::Receiver | Kind::Connector))
        .collect();
    let mut receivers: Vec<String> = Vec::new();
    let mut add = |id: &str| {
        if !receivers.iter().any(|r| r == id) {
            receivers.push(id.to_string());
        }
    };
    for r in raw.receivers {
        match r {
            ReceiverEntry::Id(s) if is_glob(&s) => {
                let found: Vec<_> = sources.iter().filter(|c| glob_match(&s, &c.id)).collect();
                if found.is_empty() {
                    return Err(err(format!(
                        "receiver pattern {s:?} matches nothing; use {{ match = {s:?}, allow_no_match = true }} if that is expected"
                    )));
                }
                for c in &found {
                    add(&c.id);
                }
            }
            ReceiverEntry::Id(s) => {
                if !sources.iter().any(|c| c.id == s) {
                    return Err(err(format!("receiver {s:?} is not defined as a receiver or connector")));
                }
                add(&s);
            }
            ReceiverEntry::Select {
                routing_labels,
                pattern,
                allow_no_match,
            } => {
                if routing_labels.is_empty() && pattern.is_none() {
                    return Err(err(
                        "a receiver selector needs `routing_labels`, `match`, or both".into()
                    ));
                }
                let found: Vec<_> = sources
                    .iter()
                    .filter(|c| pattern.as_ref().is_none_or(|p| glob_match(p, &c.id)))
                    .filter(|c| routing_labels.iter().all(|(k, v)| c.routing_labels.get(k) == Some(v)))
                    .collect();
                if found.is_empty() && !allow_no_match {
                    return Err(err(
                        "a receiver selector matches nothing; set allow_no_match = true if that is expected".into(),
                    ));
                }
                for c in &found {
                    add(&c.id);
                }
            }
        }
    }
    for p in &raw.processors {
        if !components.contains_key(&(Kind::Processor, p.clone())) {
            return Err(err(format!("processor {p:?} is not defined")));
        }
    }
    for e in &raw.exporters {
        if !components.contains_key(&(Kind::Exporter, e.clone()))
            && !components.contains_key(&(Kind::Connector, e.clone()))
        {
            return Err(err(format!(
                "exporter {e:?} is not defined as an exporter or connector"
            )));
        }
    }
    if raw.exporters.is_empty() {
        return Err(err("a pipeline needs at least one exporter".into()));
    }
    Ok(Pipeline {
        name: name.to_string(),
        receivers,
        processors: raw.processors,
        exporters: raw.exporters,
    })
}

impl Config {
    pub fn component(&self, kind: Kind, id: &str) -> Option<&Component> {
        self.components.get(&(kind, id.to_string()))
    }

    pub fn is_connector(&self, id: &str) -> bool {
        self.components.contains_key(&(Kind::Connector, id.to_string()))
    }

    /// Check connectors are used on both sides and that no chain of
    /// connectors loops back on itself.
    fn check_graph(&self) -> Result<()> {
        for ((kind, id), _) in self.components.iter().filter(|((k, _), _)| *k == Kind::Connector) {
            let _ = kind;
            let as_exporter = self.pipelines.iter().any(|p| p.exporters.contains(id));
            let as_receiver = self.pipelines.iter().any(|p| p.receivers.contains(id));
            if as_exporter != as_receiver {
                bail!(
                    "connector {id:?} is used only as {}; a connector must end one pipeline and start another",
                    if as_exporter { "an exporter" } else { "a receiver" }
                );
            }
        }
        for p in &self.pipelines {
            if p.receivers.is_empty() {
                tracing::warn!(pipeline = %p.name, "pipeline has no receivers; it will never receive data");
            }
        }
        // Edges: pipeline A -> pipeline B when A exports to a connector B receives from.
        let edges = |a: &Pipeline| -> Vec<usize> {
            self.pipelines
                .iter()
                .enumerate()
                .filter(|(_, b)| {
                    a.exporters
                        .iter()
                        .any(|e| self.is_connector(e) && b.receivers.contains(e))
                })
                .map(|(i, _)| i)
                .collect()
        };
        fn visit(i: usize, state: &mut [u8], adj: &[Vec<usize>], path: &mut Vec<usize>) -> Option<Vec<usize>> {
            state[i] = 1;
            path.push(i);
            for &j in &adj[i] {
                if state[j] == 1 {
                    let start = path.iter().position(|&x| x == j).unwrap();
                    let mut cycle = path[start..].to_vec();
                    cycle.push(j);
                    return Some(cycle);
                }
                if state[j] == 0
                    && let Some(c) = visit(j, state, adj, path)
                {
                    return Some(c);
                }
            }
            path.pop();
            state[i] = 2;
            None
        }
        let adj: Vec<Vec<usize>> = self.pipelines.iter().map(edges).collect();
        let mut state = vec![0u8; self.pipelines.len()];
        for i in 0..self.pipelines.len() {
            if state[i] == 0
                && let Some(cycle) = visit(i, &mut state, &adj, &mut Vec::new())
            {
                let names: Vec<&str> = cycle.iter().map(|&i| self.pipelines[i].name.as_str()).collect();
                bail!("pipelines form a cycle through connectors: {}", names.join(" -> "));
            }
        }
        Ok(())
    }

    /// Every component that will run must resolve to a plugin file or a
    /// built-in of its kind.
    fn check_plugins(&self) -> Result<()> {
        for c in self.active_components() {
            if c.kind == Kind::Connector && c.plugin != "forward" {
                bail!(
                    "{}: [{}]: only the built-in `forward` connector exists today",
                    c.file.display(),
                    c.label()
                );
            }
            if !self.plugins.contains_key(&c.plugin) && !builtin(c.kind, &c.plugin) {
                bail!(
                    "{}: [{}]: plugin {:?} not found; add [plugins.{}] with a source (the default registry is not available yet)",
                    c.file.display(),
                    c.label(),
                    c.plugin,
                    c.plugin
                );
            }
        }
        for id in &self.service.extensions {
            let Some(c) = self.component(Kind::Extension, id) else {
                bail!("[service] enables extension {id:?}, but [extensions.{id}] is not defined");
            };
            if c.plugin != "health_check" {
                bail!(
                    "{}: [{}]: only the built-in health_check extension exists today",
                    c.file.display(),
                    c.label()
                );
            }
        }
        Ok(())
    }

    /// Components that appear in at least one pipeline. Anything else is
    /// defined but never started.
    pub fn active_components(&self) -> Vec<&Component> {
        let mut out: Vec<&Component> = Vec::new();
        let mut seen = BTreeSet::new();
        for p in &self.pipelines {
            let refs = p
                .receivers
                .iter()
                .map(|r| {
                    if self.is_connector(r) {
                        (Kind::Connector, r)
                    } else {
                        (Kind::Receiver, r)
                    }
                })
                .chain(p.processors.iter().map(|x| (Kind::Processor, x)))
                .chain(p.exporters.iter().map(|e| {
                    if self.is_connector(e) {
                        (Kind::Connector, e)
                    } else {
                        (Kind::Exporter, e)
                    }
                }));
            for (kind, id) in refs {
                if seen.insert((kind, id.clone())) {
                    out.push(&self.components[&(kind, id.clone())]);
                }
            }
        }
        out
    }

    pub fn unused_components(&self) -> Vec<&Component> {
        let active: BTreeSet<(Kind, &str)> = self
            .active_components()
            .iter()
            .map(|c| (c.kind, c.id.as_str()))
            .collect();
        self.components
            .values()
            .filter(|c| c.kind != Kind::Extension && !active.contains(&(c.kind, c.id.as_str())))
            .collect()
    }

    /// The flow of every pipeline, for `tern config check` and startup logs.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for p in &self.pipelines {
            let _ = writeln!(
                out,
                "pipeline metrics/{}: [{}] -> [{}] -> [{}]",
                p.name,
                p.receivers.join(", "),
                p.processors.join(", "),
                p.exporters.join(", ")
            );
        }
        for c in self.unused_components() {
            let _ = writeln!(out, "unused: {} is in no pipeline and will not start", c.label());
        }
        out
    }
}

// ---- Environment substitution ----

fn substitute_table(t: &mut toml::Table) -> Result<(), String> {
    for (_, v) in t.iter_mut() {
        substitute_value(v)?;
    }
    Ok(())
}

fn substitute_value(v: &mut toml::Value) -> Result<(), String> {
    match v {
        toml::Value::String(s) => *s = substitute(s)?,
        toml::Value::Array(a) => a.iter_mut().try_for_each(substitute_value)?,
        toml::Value::Table(t) => substitute_table(t)?,
        _ => {}
    }
    Ok(())
}

/// Expand `${env:VAR}`, `${VAR}`, and `${VAR:-default}` inside one string.
/// `$${` writes a literal `${`. This runs after TOML parsing, so a variable can
/// never inject config structure.
pub fn substitute(s: &str) -> Result<String, String> {
    substitute_with(s, |k| std::env::var(k).ok())
}

fn substitute_with(s: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        if let Some(r) = after.strip_prefix("$${") {
            out.push_str("${");
            rest = r;
        } else if let Some(r) = after.strip_prefix("${") {
            let end = r.find('}').ok_or_else(|| format!("unterminated ${{...}} in {s:?}"))?;
            let expr = &r[..end];
            let expr = expr.strip_prefix("env:").unwrap_or(expr);
            let (name, default) = match expr.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (expr, None),
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("invalid variable name {name:?} in {s:?}"));
            }
            match (lookup(name), default) {
                (Some(v), _) => out.push_str(&v),
                (None, Some(d)) => out.push_str(d),
                (None, None) => {
                    return Err(format!(
                        "environment variable {name} is not set (use ${{{name}:-default}} to allow that)"
                    ));
                }
            }
            rest = &r[end + 1..];
        } else {
            out.push('$');
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESIGN_SHAPE: &str = r#"
        [service.defaults]
        collection_interval = "10s"

        [plugins.random]
        source = "file://./random.wasm"

        [receivers.web1]
        plugin = "random"
        routing_labels = { team = "web" }

        [receivers.web2]
        plugin = "random"
        routing_labels = { team = "web" }

        [receivers.mysql]
        plugin = "random"

        [receivers.cpu]
        plugin = "random"

        [connectors.archive]
        plugin = "forward"

        [exporters.console]
        plugin = "debug"

        [exporters.file_out]
        plugin = "file"
        path = "/tmp/out.json"

        [pipelines.metrics.system]
        receivers = ["cpu"]
        exporters = ["console"]

        [pipelines.metrics.web]
        receivers = [{ routing_labels = { team = "web" } }]
        exporters = ["console", "archive"]

        [pipelines.metrics.archive]
        receivers = ["archive"]
        exporters = ["file_out"]
    "#;

    #[test]
    fn resolves_the_design_example() {
        let cfg = load_str(DESIGN_SHAPE).unwrap();
        let web = cfg.pipelines.iter().find(|p| p.name == "web").unwrap();
        assert_eq!(web.receivers, ["web1", "web2"]);
        let unused: Vec<_> = cfg.unused_components().iter().map(|c| c.id.clone()).collect();
        assert_eq!(unused, ["mysql"]);
        let out = cfg.component(Kind::Exporter, "file_out").unwrap();
        assert_eq!(out.settings.get("path").and_then(|v| v.as_str()), Some("/tmp/out.json"));
        assert!(!out.settings.contains_key("plugin"));
    }

    #[test]
    fn rejects_bad_graphs() {
        let base = r#"
            [plugins.random]
            source = "file://r.wasm"
            [receivers.r]
            plugin = "random"
            [exporters.out]
            plugin = "debug"
        "#;
        let err = |extra: &str| load_str(&format!("{base}{extra}")).unwrap_err().to_string();
        assert!(err("").contains("no pipelines"));
        assert!(err("[pipelines.metrics.a]\nreceivers=[\"nope\"]\nexporters=[\"out\"]").contains("not defined"));
        assert!(err("[pipelines.metrics.a]\nreceivers=[\"x*\"]\nexporters=[\"out\"]").contains("matches nothing"));
        assert!(err("[pipelines.logs.a]\n").contains("reserved"));
        assert!(err("[[inputs.cpu]]\n").contains("Telegraf"));
        assert!(err("[receivers.r2]\nplugin=\"random\"\ngrant={}\n[pipelines.metrics.a]\nreceivers=[\"r\"]\nexporters=[\"out\"]").contains("grant"));
        let cycle = r#"
            [connectors.c1]
            plugin = "forward"
            [connectors.c2]
            plugin = "forward"
            [pipelines.metrics.a]
            receivers = ["r", "c2"]
            exporters = ["c1"]
            [pipelines.metrics.b]
            receivers = ["c1"]
            exporters = ["c2", "out"]
        "#;
        assert!(err(cycle).contains("cycle"));
        let one_sided =
            "[connectors.c]\nplugin=\"forward\"\n[pipelines.metrics.a]\nreceivers=[\"r\"]\nexporters=[\"c\"]";
        assert!(err(one_sided).contains("only as an exporter"));
        let missing_plugin = "[receivers.cpu]\n[pipelines.metrics.a]\nreceivers=[\"cpu\"]\nexporters=[\"out\"]";
        assert!(err(missing_plugin).contains("plugin \"cpu\" not found"));
        let bad_id =
            "[receivers.Bad]\nplugin=\"random\"\n[pipelines.metrics.a]\nreceivers=[\"r\"]\nexporters=[\"out\"]";
        assert!(err(bad_id).contains("IDs may use only"));
    }

    #[test]
    fn substitutes_env_in_strings_only() {
        let env = |k: &str| match k {
            "HOST" => Some("db1".to_string()),
            "PORT" => Some("5432".to_string()),
            _ => None,
        };
        assert_eq!(substitute_with("${env:HOST}:${PORT}", env).unwrap(), "db1:5432");
        assert_eq!(substitute_with("${MISSING:-x}", env).unwrap(), "x");
        assert_eq!(substitute_with("$${HOST} $5", env).unwrap(), "${HOST} $5");
        assert!(substitute_with("${MISSING}", env).is_err());
        assert!(substitute_with("${HOST", env).is_err());
    }
}
