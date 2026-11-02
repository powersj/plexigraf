//! The `random` example scraper: emits random integer and float gauges.

use plexigraf_sdk::{Batch, Metric, PluginError, Scraper, Timestamp, export_scraper};
use serde::Deserialize;

// The SDK deserializes the component's TOML settings into this struct.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Random {
    #[serde(default = "default_prefix")]
    prefix: String, // metric name prefix
    #[serde(default)]
    min: i64,
    #[serde(default = "default_max")]
    max: i64,
}
fn default_prefix() -> String {
    "random".into()
}
fn default_max() -> i64 {
    100
}

impl Scraper for Random {
    // Called by `plexigraf config check` and before start.
    fn validate(&self) -> Result<(), PluginError> {
        if self.min >= self.max {
            return Err(PluginError::Config("min must be less than max".into()));
        }
        Ok(())
    }

    // `now` comes from the core, already rounded to the configured precision.
    // Each value is its own OTel gauge; "1" is the UCUM unit for a plain number.
    fn scrape(&mut self, now: Timestamp) -> Result<Batch, PluginError> {
        let attrs = [("source", "plexigraf")];
        let mut batch = Batch::new();
        batch.push(Metric::gauge(format!("{}.int", self.prefix), "1").point(
            now,
            fastrand::i64(self.min..self.max),
            &attrs,
        ));
        batch.push(Metric::gauge(format!("{}.float", self.prefix), "1").point(now, fastrand::f64(), &attrs));
        Ok(batch)
    }
}

// Name, version, and sample config come from the manifest; start and
// shutdown use the trait defaults.
export_scraper!(Random);
