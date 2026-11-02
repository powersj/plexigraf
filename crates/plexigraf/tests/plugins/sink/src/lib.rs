//! An exporter for the integration tests that succeeds, rejects, or fails on demand.

use plexigraf_sdk::{Batch, ExportOutcome, Exporter, PartialFailure, PluginError, encoding, export_exporter, log};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sink {
    /// ok, reject, partial, or flaky (retry once, then ok).
    mode: String,
    #[serde(skip)]
    attempts: u32,
}

impl Exporter for Sink {
    fn export(&mut self, batch: Batch) -> Result<ExportOutcome, PluginError> {
        self.attempts += 1;
        match self.mode.as_str() {
            "reject" => return Ok(ExportOutcome::Reject("sink: rejected".into())),
            "partial" => {
                return Ok(ExportOutcome::Partial(PartialFailure {
                    rejected_data_points: 1,
                    reason: "sink: one bad point".into(),
                    indices: None,
                }));
            }
            "flaky" if self.attempts == 1 => return Ok(ExportOutcome::Retry(Some(10))),
            _ => {}
        }
        let text = encoding::encode("influx", "", &batch).map_err(PluginError::Fatal)?;
        let text = String::from_utf8_lossy(&text);
        for line in text.lines() {
            log::warn("sink line", &[("line", line), ("attempt", &self.attempts.to_string())]);
        }
        Ok(ExportOutcome::Ok)
    }
}

export_exporter!(Sink);
