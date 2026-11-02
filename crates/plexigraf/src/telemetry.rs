//! Service telemetry: the agent's own counters, named after the Collector's
//! internal metrics and served in Prometheus text by the telemetry endpoint.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

type Key = (&'static str, Vec<(&'static str, String)>);

fn counters() -> &'static Mutex<BTreeMap<Key, u64>> {
    static COUNTERS: OnceLock<Mutex<BTreeMap<Key, u64>>> = OnceLock::new();
    COUNTERS.get_or_init(Default::default)
}

pub fn add(name: &'static str, attrs: &[(&'static str, &str)], n: u64) {
    if n == 0 {
        return;
    }
    let key = (name, attrs.iter().map(|(k, v)| (*k, v.to_string())).collect());
    *counters().lock().unwrap().entry(key).or_default() += n;
}

/// Count a conversion that could not be expressed in the target format.
pub fn lossy(from: &str, to: &str, feature: &str, n: u64) {
    add(
        "plexigraf.conversion.lossy",
        &[("from", from), ("to", to), ("feature", feature)],
        n,
    );
}

pub fn component(name: &'static str, kind: &str, id: &str, n: u64) {
    add(name, &[("otel.component.type", kind), ("otel.component.name", id)], n);
}

/// Render every counter in Prometheus text format, translating names the way
/// the OTel-to-Prometheus mapping does.
pub fn prometheus_text() -> String {
    let mut out = String::new();
    let mut last = "";
    for ((name, attrs), value) in counters().lock().unwrap().iter() {
        let prom = format!("{}_total", name.replace('.', "_"));
        if *name != last {
            let _ = writeln!(out, "# TYPE {prom} counter");
            last = name;
        }
        let labels: Vec<String> = attrs
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}=\"{}\"",
                    k.replace('.', "_"),
                    v.replace('\\', "\\\\").replace('"', "\\\"")
                )
            })
            .collect();
        if labels.is_empty() {
            let _ = writeln!(out, "{prom} {value}");
        } else {
            let _ = writeln!(out, "{prom}{{{}}} {value}", labels.join(","));
        }
    }
    out
}
