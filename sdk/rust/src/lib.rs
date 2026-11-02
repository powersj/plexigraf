//! Write Tern plugins in Rust.
//!
//! Enable the feature for the world your plugin targets (`scraper`,
//! `processor`, or `exporter`), implement the matching trait on your config
//! struct, and call the export macro:
//!
//! ```ignore
//! use serde::Deserialize;
//! use tern_sdk::{export_scraper, Batch, Metric, PluginError, Scraper, Timestamp};
//!
//! #[derive(Deserialize)]
//! struct Hello {}
//!
//! impl Scraper for Hello {
//!     fn scrape(&mut self, now: Timestamp) -> Result<Batch, PluginError> {
//!         let mut batch = Batch::new();
//!         batch.push(Metric::gauge("hello.value", "1").point(now, 1, &[]));
//!         Ok(batch)
//!     }
//! }
//!
//! export_scraper!(Hello);
//! ```
//!
//! Build with `cargo build --target wasm32-wasip2 --release`.

#[cfg(not(any(feature = "scraper", feature = "processor", feature = "exporter")))]
compile_error!("enable exactly one of the `scraper`, `processor`, or `exporter` features of tern-sdk");

#[cfg(any(
    all(feature = "scraper", feature = "processor"),
    all(feature = "scraper", feature = "exporter"),
    all(feature = "processor", feature = "exporter"),
))]
compile_error!("enable only one of the `scraper`, `processor`, or `exporter` features of tern-sdk");

/// Generated bindings for the selected world. Plugin code should not need
/// these; use the SDK types instead.
#[doc(hidden)]
pub mod bindings {
    #[cfg(feature = "scraper")]
    wit_bindgen::generate!({
        path: "../../wit",
        world: "tern:plugin/scraper",
        pub_export_macro: true,
        export_macro_name: "__export_world",
        default_bindings_module: "tern_sdk::bindings",
        generate_all,
    });
    #[cfg(feature = "processor")]
    wit_bindgen::generate!({
        path: "../../wit",
        world: "tern:plugin/processor",
        pub_export_macro: true,
        export_macro_name: "__export_world",
        default_bindings_module: "tern_sdk::bindings",
        generate_all,
    });
    #[cfg(feature = "exporter")]
    wit_bindgen::generate!({
        path: "../../wit",
        world: "tern:plugin/exporter",
        pub_export_macro: true,
        export_macro_name: "__export_world",
        default_bindings_module: "tern_sdk::bindings",
        generate_all,
    });
}

use std::cell::RefCell;

use serde::de::DeserializeOwned;

/// The raw `tern:metrics` types, which mirror the OTLP data model.
pub use bindings::tern::metrics::metrics as raw;
pub use bindings::tern::plugin::types::PluginError;
#[cfg(feature = "exporter")]
pub use bindings::tern::plugin::types::{ExportOutcome, PartialFailure};

use raw::{
    AggregationTemporality, AnyValue, Data, DataPointFlags, Gauge, KeyValue, Leaf, Number, NumberDataPoint, Sum,
};

/// Nanoseconds since the Unix epoch, as handed out by the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn unix_nano(self) -> u64 {
        self.0
    }
}

/// A batch of metrics owned by the host. Plugins add to it and read from it
/// without copying resources or scopes.
pub struct Batch(raw::Batch);

impl Batch {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Batch {
        Batch(raw::Batch::new())
    }

    pub fn push(&mut self, m: impl Into<raw::Metric>) {
        self.0.push(&m.into());
    }

    pub fn extend(&mut self, ms: impl IntoIterator<Item = raw::Metric>) {
        self.0.extend(&ms.into_iter().collect::<Vec<_>>());
    }

    pub fn len(&self) -> usize {
        self.0.len() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<raw::Metric> {
        (index < self.len()).then(|| self.0.get(index as u32))
    }

    pub fn set(&mut self, index: usize, m: raw::Metric) {
        self.0.set(index as u32, &m);
    }

    /// Copy every metric out, in one boundary crossing per metric.
    pub fn iter(&self) -> impl Iterator<Item = raw::Metric> + '_ {
        (0..self.0.len()).map(|i| self.0.get(i))
    }

    /// Keep only the metrics for which `keep` returns true, in one pass.
    pub fn retain(&mut self, mut keep: impl FnMut(&raw::Metric) -> bool) {
        let flags: Vec<bool> = self.iter().map(|m| keep(&m)).collect();
        self.0.retain(&flags);
    }

    /// Move every metric out, leaving the batch empty.
    pub fn take_all(&mut self) -> Vec<raw::Metric> {
        self.0.take_all()
    }

    pub fn resource(&self, index: u32) -> raw::Resource {
        self.0.resource_at(index)
    }

    pub fn scope(&self, index: u32) -> raw::InstrumentationScope {
        self.0.scope_at(index)
    }

    pub fn add_resource(&mut self, r: raw::Resource) -> u32 {
        self.0.add_resource(&r)
    }

    pub fn add_scope(&mut self, s: raw::InstrumentationScope) -> u32 {
        self.0.add_scope(&s)
    }

    #[doc(hidden)]
    pub fn from_raw(b: raw::Batch) -> Batch {
        Batch(b)
    }

    #[doc(hidden)]
    pub fn into_raw(self) -> raw::Batch {
        self.0
    }

    pub fn as_raw(&self) -> &raw::Batch {
        &self.0
    }
}

impl From<i64> for Number {
    fn from(v: i64) -> Number {
        Number::Int(v)
    }
}
impl From<i32> for Number {
    fn from(v: i32) -> Number {
        Number::Int(v as i64)
    }
}
impl From<u32> for Number {
    fn from(v: u32) -> Number {
        Number::Int(v as i64)
    }
}
impl From<f64> for Number {
    fn from(v: f64) -> Number {
        Number::Double(v)
    }
}
impl From<f32> for Number {
    fn from(v: f32) -> Number {
        Number::Double(v as f64)
    }
}

impl From<&str> for AnyValue {
    fn from(v: &str) -> AnyValue {
        AnyValue::Leaf(Leaf::Str(v.to_string()))
    }
}
impl From<String> for AnyValue {
    fn from(v: String) -> AnyValue {
        AnyValue::Leaf(Leaf::Str(v))
    }
}
impl From<i64> for AnyValue {
    fn from(v: i64) -> AnyValue {
        AnyValue::Leaf(Leaf::Int(v))
    }
}
impl From<f64> for AnyValue {
    fn from(v: f64) -> AnyValue {
        AnyValue::Leaf(Leaf::Double(v))
    }
}
impl From<bool> for AnyValue {
    fn from(v: bool) -> AnyValue {
        AnyValue::Leaf(Leaf::Bool(v))
    }
}

/// Build an attribute from a key and any scalar value.
pub fn attr(key: impl Into<String>, value: impl Into<AnyValue>) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: value.into(),
    }
}

/// A metric under construction. Each value is its own metric in the OTel
/// model, so a builder holds one name, unit, and type, plus data points.
#[derive(Clone)]
pub struct Metric {
    raw: raw::Metric,
    start: u64,
}

impl Metric {
    fn new(name: impl Into<String>, unit: impl Into<String>, data: Data) -> Metric {
        Metric {
            raw: raw::Metric {
                resource: 0,
                scope: 0,
                name: name.into(),
                description: String::new(),
                unit: unit.into(),
                data,
                metadata: Vec::new(),
            },
            start: 0,
        }
    }

    /// A sampled value, such as a temperature or utilization.
    pub fn gauge(name: impl Into<String>, unit: impl Into<String>) -> Metric {
        Metric::new(
            name,
            unit,
            Data::Gauge(Gauge {
                data_points: Vec::new(),
            }),
        )
    }

    /// A monotonic, cumulative sum, such as bytes sent.
    pub fn counter(name: impl Into<String>, unit: impl Into<String>) -> Metric {
        Metric::new(
            name,
            unit,
            Data::Sum(Sum {
                data_points: Vec::new(),
                aggregation_temporality: AggregationTemporality::Cumulative,
                is_monotonic: true,
            }),
        )
    }

    /// A cumulative sum that can go down, such as memory in use.
    pub fn up_down(name: impl Into<String>, unit: impl Into<String>) -> Metric {
        Metric::new(
            name,
            unit,
            Data::Sum(Sum {
                data_points: Vec::new(),
                aggregation_temporality: AggregationTemporality::Cumulative,
                is_monotonic: false,
            }),
        )
    }

    pub fn description(mut self, d: impl Into<String>) -> Metric {
        self.raw.description = d.into();
        self
    }

    /// Start time for the points that follow, for cumulative sums.
    pub fn start_time(mut self, t: Timestamp) -> Metric {
        self.start = t.0;
        self
    }

    /// Add a data point with string attributes.
    pub fn point(self, now: Timestamp, value: impl Into<Number>, attrs: &[(&str, &str)]) -> Metric {
        let attrs = attrs.iter().map(|(k, v)| attr(*k, *v)).collect();
        self.point_with(now, value, attrs)
    }

    /// Add a data point with attributes of any type.
    pub fn point_with(mut self, now: Timestamp, value: impl Into<Number>, attributes: Vec<KeyValue>) -> Metric {
        let p = NumberDataPoint {
            attributes,
            start_time_unix_nano: self.start,
            time_unix_nano: now.0,
            value: value.into(),
            exemplars: Vec::new(),
            flags: DataPointFlags::empty(),
        };
        match &mut self.raw.data {
            Data::Gauge(g) => g.data_points.push(p),
            Data::Sum(s) => s.data_points.push(p),
            _ => unreachable!("builders only make gauges and sums"),
        }
        self
    }
}

impl From<Metric> for raw::Metric {
    fn from(m: Metric) -> raw::Metric {
        m.raw
    }
}

/// Logging through the agent, tagged with the component ID.
pub mod log {
    use crate::bindings::tern::plugin::host::{self, Kv, Level};

    fn emit(level: Level, msg: &str, fields: &[(&str, &str)]) {
        let fields: Vec<Kv> = fields
            .iter()
            .map(|(k, v)| Kv {
                key: k.to_string(),
                value: v.to_string(),
            })
            .collect();
        host::log(level, msg, &fields);
    }
    pub fn debug(msg: &str, fields: &[(&str, &str)]) {
        emit(Level::Debug, msg, fields)
    }
    pub fn info(msg: &str, fields: &[(&str, &str)]) {
        emit(Level::Info, msg, fields)
    }
    pub fn warn(msg: &str, fields: &[(&str, &str)]) {
        emit(Level::Warn, msg, fields)
    }
    pub fn error(msg: &str, fields: &[(&str, &str)]) {
        emit(Level::Error, msg, fields)
    }
}

/// Persisted plugin state and secrets.
pub mod host {
    use crate::bindings::tern::plugin::host;

    pub fn secret(name: &str) -> Option<String> {
        host::secret(name)
    }
    pub fn state_load() -> Option<Vec<u8>> {
        host::state_load()
    }
    pub fn state_save(blob: &[u8]) {
        host::state_save(blob)
    }
}

/// The core's encodings, so plugins never bundle their own codecs.
pub mod encoding {
    use crate::Batch;
    use crate::bindings::tern::plugin::encoding;

    pub fn decode(format: &str, options_toml: &str, data: &[u8]) -> Result<Batch, String> {
        encoding::decode(format, options_toml, data).map(Batch::from_raw)
    }
    pub fn encode(format: &str, options_toml: &str, batch: &Batch) -> Result<Vec<u8>, String> {
        encoding::encode(format, options_toml, batch.as_raw())
    }
}

/// A plugin that polls on an interval.
pub trait Scraper: DeserializeOwned + Sized + 'static {
    /// Check settings without side effects; `tern config check` calls this.
    fn validate(&self) -> Result<(), PluginError> {
        Ok(())
    }
    fn start(cfg: Self) -> Result<Self, PluginError> {
        Ok(cfg)
    }
    fn scrape(&mut self, now: Timestamp) -> Result<Batch, PluginError>;
    fn shutdown(&mut self) {}
}

/// A plugin that transforms batches, and optionally emits on a timer.
pub trait Processor: DeserializeOwned + Sized + 'static {
    fn validate(&self) -> Result<(), PluginError> {
        Ok(())
    }
    fn start(cfg: Self) -> Result<Self, PluginError> {
        Ok(cfg)
    }
    fn process(&mut self, batch: Batch) -> Result<Batch, PluginError>;
    /// Called on a timer only when the manifest sets `flush = true`.
    fn flush(&mut self, _now: Timestamp) -> Result<Batch, PluginError> {
        Ok(Batch::new())
    }
    fn shutdown(&mut self) {}
}

/// A plugin that sends batches somewhere.
#[cfg(feature = "exporter")]
pub trait Exporter: DeserializeOwned + Sized + 'static {
    fn validate(&self) -> Result<(), PluginError> {
        Ok(())
    }
    fn start(cfg: Self) -> Result<Self, PluginError> {
        Ok(cfg)
    }
    fn export(&mut self, batch: Batch) -> Result<ExportOutcome, PluginError>;
    fn shutdown(&mut self) {}
}

/// Glue used by the export macros.
#[doc(hidden)]
pub mod __rt {
    use super::*;
    use std::any::Any;

    thread_local! {
        static INSTANCE: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
    }

    fn parse<T: DeserializeOwned>(toml: &str) -> Result<T, PluginError> {
        toml::from_str(toml).map_err(|e| PluginError::Config(e.message().to_string()))
    }

    pub fn validate<T: DeserializeOwned>(
        toml: &str,
        check: fn(&T) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        check(&parse::<T>(toml)?)
    }

    pub fn start<T: DeserializeOwned + 'static>(
        toml: &str,
        start: fn(T) -> Result<T, PluginError>,
    ) -> Result<(), PluginError> {
        let inst = start(parse::<T>(toml)?)?;
        INSTANCE.with(|i| *i.borrow_mut() = Some(Box::new(inst)));
        Ok(())
    }

    pub fn with<T: 'static, R>(f: impl FnOnce(&mut T) -> Result<R, PluginError>) -> Result<R, PluginError> {
        INSTANCE.with(|i| match i.borrow_mut().as_mut().and_then(|b| b.downcast_mut::<T>()) {
            Some(inst) => f(inst),
            None => Err(PluginError::Fatal("plugin called before start".into())),
        })
    }

    pub fn stop<T: 'static>(f: impl FnOnce(&mut T)) {
        INSTANCE.with(|i| {
            if let Some(mut b) = i.borrow_mut().take() {
                if let Some(inst) = b.downcast_mut::<T>() {
                    f(inst);
                }
            }
        });
    }
}

/// Export a [`Scraper`] as the plugin's component.
#[cfg(feature = "scraper")]
#[macro_export]
macro_rules! export_scraper {
    ($t:ty) => {
        struct __TernGuest;
        impl $crate::bindings::Guest for __TernGuest {
            fn scrape(now: u64) -> Result<$crate::raw::Batch, $crate::PluginError> {
                $crate::__rt::with::<$t, _>(|s| <$t as $crate::Scraper>::scrape(s, $crate::Timestamp(now)))
                    .map($crate::Batch::into_raw)
            }
        }
        impl $crate::bindings::exports::tern::plugin::lifecycle::Guest for __TernGuest {
            fn validate_config(c: String) -> Result<(), $crate::PluginError> {
                $crate::__rt::validate::<$t>(&c, <$t as $crate::Scraper>::validate)
            }
            fn start(c: String) -> Result<(), $crate::PluginError> {
                $crate::__rt::start::<$t>(&c, <$t as $crate::Scraper>::start)
            }
            fn shutdown() {
                $crate::__rt::stop::<$t>(<$t as $crate::Scraper>::shutdown)
            }
        }
        $crate::bindings::__export_world!(__TernGuest);
    };
}

/// Export a [`Processor`] as the plugin's component.
#[cfg(feature = "processor")]
#[macro_export]
macro_rules! export_processor {
    ($t:ty) => {
        struct __TernGuest;
        impl $crate::bindings::Guest for __TernGuest {
            fn process(input: $crate::raw::Batch) -> Result<$crate::raw::Batch, $crate::PluginError> {
                $crate::__rt::with::<$t, _>(|s| <$t as $crate::Processor>::process(s, $crate::Batch::from_raw(input)))
                    .map($crate::Batch::into_raw)
            }
            fn flush(now: u64) -> Result<$crate::raw::Batch, $crate::PluginError> {
                $crate::__rt::with::<$t, _>(|s| <$t as $crate::Processor>::flush(s, $crate::Timestamp(now)))
                    .map($crate::Batch::into_raw)
            }
        }
        impl $crate::bindings::exports::tern::plugin::lifecycle::Guest for __TernGuest {
            fn validate_config(c: String) -> Result<(), $crate::PluginError> {
                $crate::__rt::validate::<$t>(&c, <$t as $crate::Processor>::validate)
            }
            fn start(c: String) -> Result<(), $crate::PluginError> {
                $crate::__rt::start::<$t>(&c, <$t as $crate::Processor>::start)
            }
            fn shutdown() {
                $crate::__rt::stop::<$t>(<$t as $crate::Processor>::shutdown)
            }
        }
        $crate::bindings::__export_world!(__TernGuest);
    };
}

/// Export an [`Exporter`] as the plugin's component.
#[cfg(feature = "exporter")]
#[macro_export]
macro_rules! export_exporter {
    ($t:ty) => {
        struct __TernGuest;
        impl $crate::bindings::Guest for __TernGuest {
            fn export(b: $crate::raw::Batch) -> Result<$crate::ExportOutcome, $crate::PluginError> {
                $crate::__rt::with::<$t, _>(|s| <$t as $crate::Exporter>::export(s, $crate::Batch::from_raw(b)))
            }
        }
        impl $crate::bindings::exports::tern::plugin::lifecycle::Guest for __TernGuest {
            fn validate_config(c: String) -> Result<(), $crate::PluginError> {
                $crate::__rt::validate::<$t>(&c, <$t as $crate::Exporter>::validate)
            }
            fn start(c: String) -> Result<(), $crate::PluginError> {
                $crate::__rt::start::<$t>(&c, <$t as $crate::Exporter>::start)
            }
            fn shutdown() {
                $crate::__rt::stop::<$t>(<$t as $crate::Exporter>::shutdown)
            }
        }
        $crate::bindings::__export_world!(__TernGuest);
    };
}
