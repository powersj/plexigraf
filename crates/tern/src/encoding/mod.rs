//! Built-in encodings. Each format has one decoder into the internal model
//! and one encoder out of it, shared by built-in components and by plugins
//! through the `tern:plugin/encoding` interface.

mod influx;
mod json;

use crate::model::BatchData;

/// Encode a batch. `options` holds the format's settings, the same keys a
/// component's `encoding` table takes, without `format`.
pub fn encode(format: &str, options: &toml::Table, batch: &BatchData) -> Result<Vec<u8>, String> {
    match format {
        "influx" => influx::encode(&options_into(options)?, batch),
        "json" => json::encode(&options_into(options)?, batch),
        "otlp" | "prometheus" => Err(format!("encoding {format:?} is not implemented yet")),
        _ => Err(format!("unknown encoding {format:?} (known: influx, json)")),
    }
}

pub fn decode(format: &str, options: &toml::Table, data: &[u8]) -> Result<BatchData, String> {
    match format {
        "influx" => influx::decode(&options_into(options)?, data, now_unix_nano()),
        "json" | "otlp" | "prometheus" => Err(format!("decoding {format:?} is not implemented yet")),
        _ => Err(format!("unknown encoding {format:?} (known: influx)")),
    }
}

/// Parse the `options-toml` string that crosses the plugin boundary.
pub fn parse_options(options_toml: &str) -> Result<toml::Table, String> {
    options_toml
        .parse::<toml::Table>()
        .map_err(|e| format!("invalid encoding options: {e}"))
}

fn options_into<T: serde::de::DeserializeOwned>(options: &toml::Table) -> Result<T, String> {
    toml::Value::Table(options.clone())
        .try_into()
        .map_err(|e| format!("invalid encoding options: {e}"))
}

fn now_unix_nano() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// An `encoding = { format = "...", ... }` setting on a built-in component.
#[derive(Clone, Debug)]
pub struct EncodingConfig {
    pub format: String,
    pub options: toml::Table,
}

impl EncodingConfig {
    pub fn from_value(value: Option<&toml::Value>, default_format: &str) -> Result<Self, String> {
        let mut options = match value {
            None => toml::Table::new(),
            Some(toml::Value::String(s)) => {
                let mut t = toml::Table::new();
                t.insert("format".into(), toml::Value::String(s.clone()));
                t
            }
            Some(toml::Value::Table(t)) => t.clone(),
            Some(_) => return Err("`encoding` must be a format name or a table".into()),
        };
        let format = match options.remove("format") {
            None => default_format.to_string(),
            Some(toml::Value::String(s)) => s,
            Some(_) => return Err("`encoding.format` must be a string".into()),
        };
        let cfg = EncodingConfig { format, options };
        // Fail at startup, not on the first batch.
        encode(&cfg.format, &cfg.options, &BatchData::default())?;
        Ok(cfg)
    }

    pub fn encode(&self, batch: &BatchData) -> Result<Vec<u8>, String> {
        encode(&self.format, &self.options, batch)
    }
}
