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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::metrics::Data;

    const LINE: &[u8] = b"cpu,host=a usage=0.5 1761955200000000000\n";

    #[test]
    fn dispatches_by_format() {
        let batch = decode("influx", &toml::Table::new(), LINE).unwrap();
        assert_eq!(batch.metrics.len(), 1);
        assert_eq!(batch.metrics[0].name, "cpu.usage");

        let influx = encode("influx", &toml::Table::new(), &batch).unwrap();
        assert!(
            String::from_utf8(influx)
                .unwrap()
                .starts_with("cpu.usage,host=a value=0.5 ")
        );
        let json: serde_json::Value =
            serde_json::from_slice(&encode("json", &toml::Table::new(), &batch).unwrap()).unwrap();
        assert!(json.get("resourceMetrics").is_some(), "{json}");
    }

    #[test]
    fn rejects_unknown_and_unimplemented_formats() {
        let empty = BatchData::default();
        for format in ["otlp", "prometheus"] {
            assert!(
                encode(format, &toml::Table::new(), &empty)
                    .unwrap_err()
                    .contains("not implemented")
            );
        }
        for format in ["json", "otlp", "prometheus"] {
            assert!(
                decode(format, &toml::Table::new(), LINE)
                    .unwrap_err()
                    .contains("not implemented")
            );
        }
        assert!(
            encode("csv", &toml::Table::new(), &empty)
                .unwrap_err()
                .contains("unknown encoding")
        );
        assert!(
            decode("csv", &toml::Table::new(), LINE)
                .unwrap_err()
                .contains("unknown encoding")
        );
    }

    #[test]
    fn passes_options_through() {
        let options = parse_options("multi_field_lines = true").unwrap();
        let batch = decode("influx", &toml::Table::new(), b"cpu usage=0.5,idle=99i 1\n").unwrap();
        let out = String::from_utf8(encode("influx", &options, &batch).unwrap()).unwrap();
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(parse_options("not toml").is_err());
        assert!(encode("influx", &parse_options("bogus = 1").unwrap(), &batch).is_err());
    }

    #[test]
    fn missing_timestamps_default_to_now() {
        let batch = decode("influx", &toml::Table::new(), b"cpu usage=0.5\n").unwrap();
        let Data::Gauge(g) = &batch.metrics[0].data else {
            panic!("expected a gauge")
        };
        assert!(g.data_points[0].time_unix_nano > 1_700_000_000_000_000_000);
    }
}
