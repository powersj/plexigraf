//! A processor for the integration tests: prefixes metric names and drops some.

use plexigraf_sdk::{Batch, Metric, PluginError, Processor, Timestamp, export_processor};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rename {
    prefix: String,
    /// Metrics whose name ends with this are dropped.
    #[serde(default)]
    drop_suffix: Option<String>,
    #[serde(skip)]
    seen: i64,
}

impl Processor for Rename {
    fn process(&mut self, mut batch: Batch) -> Result<Batch, PluginError> {
        if let Some(suffix) = &self.drop_suffix {
            batch.retain(|m| !m.name.ends_with(suffix.as_str()));
        }
        for i in 0..batch.len() {
            let mut m = batch.get(i).unwrap();
            m.name = format!("{}{}", self.prefix, m.name);
            batch.set(i, m);
        }
        self.seen += i64::try_from(batch.len()).unwrap_or(i64::MAX);
        Ok(batch)
    }

    fn flush(&mut self, now: Timestamp) -> Result<Batch, PluginError> {
        let mut b = Batch::new();
        b.push(Metric::counter("rename.seen", "1").point(now, self.seen, &[]));
        Ok(b)
    }
}

export_processor!(Rename);
