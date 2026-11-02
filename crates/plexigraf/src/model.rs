//! The internal metric model: the OTel data model exactly as `plexigraf:metrics`
//! defines it, plus the host-owned batch that holds it.

use crate::bindings::metrics::{AnyValue, Data, InstrumentationScope, KeyValue, Leaf, Metric, Node, Resource};

/// Caps that keep one plugin from exhausting the host (threat S6).
pub const MAX_METRICS_PER_BATCH: usize = 100_000;
pub const MAX_ANY_VALUE_NODES: usize = 1_024;
pub const MAX_ANY_VALUE_DEPTH: usize = 32;

/// The host side of the `batch` resource. Resources and scopes are stored once
/// and referenced by index from each metric.
#[derive(Clone, Debug, Default)]
pub struct BatchData {
    pub resources: Vec<Resource>,
    pub scopes: Vec<InstrumentationScope>,
    pub metrics: Vec<Metric>,
}

impl BatchData {
    pub fn is_empty(&self) -> bool {
        self.metrics.is_empty()
    }

    pub fn data_point_count(&self) -> u64 {
        self.metrics.iter().map(|m| data_point_count(&m.data)).sum()
    }

    /// Fill in an empty resource and the plugin's scope for metrics a plugin
    /// created without them. Tables that already have entries are kept as
    /// received, so pass-through metrics keep their resource and scope.
    pub fn normalize(&mut self, scope_name: &str, scope_version: &str) {
        if self.metrics.is_empty() {
            return;
        }
        if self.resources.is_empty() {
            self.resources.push(empty_resource());
        }
        if self.scopes.is_empty() {
            self.scopes.push(InstrumentationScope {
                name: scope_name.to_string(),
                version: scope_version.to_string(),
                attributes: Vec::new(),
                dropped_attributes_count: 0,
                schema_url: String::new(),
            });
        }
    }

    /// Check every invariant the WIT types cannot express. Called whenever a
    /// batch crosses from a plugin back into the host.
    pub fn validate(&self) -> Result<(), String> {
        if self.metrics.len() > MAX_METRICS_PER_BATCH {
            return Err(format!(
                "batch has {} metrics, more than the limit of {MAX_METRICS_PER_BATCH}",
                self.metrics.len()
            ));
        }
        for r in &self.resources {
            validate_attrs(&r.attributes)?;
        }
        for s in &self.scopes {
            validate_attrs(&s.attributes)?;
        }
        for (i, m) in self.metrics.iter().enumerate() {
            if m.resource as usize >= self.resources.len() {
                return Err(format!(
                    "metric {i} ({}) references resource {} but the batch has {}",
                    m.name,
                    m.resource,
                    self.resources.len()
                ));
            }
            if m.scope as usize >= self.scopes.len() {
                return Err(format!(
                    "metric {i} ({}) references scope {} but the batch has {}",
                    m.name,
                    m.scope,
                    self.scopes.len()
                ));
            }
            if m.name.is_empty() {
                return Err(format!("metric {i} has an empty name"));
            }
            validate_attrs(&m.metadata)?;
            validate_data(&m.data).map_err(|e| format!("metric {}: {e}", m.name))?;
        }
        Ok(())
    }
}

pub fn empty_resource() -> Resource {
    Resource {
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        schema_url: String::new(),
    }
}

pub fn data_point_count(data: &Data) -> u64 {
    (match data {
        Data::Gauge(g) => g.data_points.len(),
        Data::Sum(s) => s.data_points.len(),
        Data::Histogram(h) => h.data_points.len(),
        Data::ExponentialHistogram(h) => h.data_points.len(),
        Data::Summary(s) => s.data_points.len(),
    }) as u64
}

fn validate_data(data: &Data) -> Result<(), String> {
    let check_ex = |ex: &[crate::bindings::metrics::Exemplar]| -> Result<(), String> {
        for e in ex {
            validate_attrs(&e.filtered_attributes)?;
            if e.trace_id.as_ref().is_some_and(|t| t.len() != 16) {
                return Err("exemplar trace-id must be 16 bytes".into());
            }
            if e.span_id.as_ref().is_some_and(|s| s.len() != 8) {
                return Err("exemplar span-id must be 8 bytes".into());
            }
        }
        Ok(())
    };
    match data {
        Data::Gauge(g) => {
            for p in &g.data_points {
                validate_attrs(&p.attributes)?;
                check_ex(&p.exemplars)?;
            }
        }
        Data::Sum(s) => {
            for p in &s.data_points {
                validate_attrs(&p.attributes)?;
                check_ex(&p.exemplars)?;
            }
        }
        Data::Histogram(h) => {
            for p in &h.data_points {
                validate_attrs(&p.attributes)?;
                check_ex(&p.exemplars)?;
                if !p.bucket_counts.is_empty() && p.bucket_counts.len() != p.explicit_bounds.len() + 1 {
                    return Err("histogram bucket-counts must have one more entry than explicit-bounds".into());
                }
            }
        }
        Data::ExponentialHistogram(h) => {
            for p in &h.data_points {
                validate_attrs(&p.attributes)?;
                check_ex(&p.exemplars)?;
            }
        }
        Data::Summary(s) => {
            for p in &s.data_points {
                validate_attrs(&p.attributes)?;
            }
        }
    }
    Ok(())
}

pub fn validate_attrs(attrs: &[KeyValue]) -> Result<(), String> {
    for kv in attrs {
        if let AnyValue::Tree(nodes) = &kv.value {
            validate_tree(nodes).map_err(|e| format!("attribute {}: {e}", kv.key))?;
        }
    }
    Ok(())
}

/// A tree is valid when every node except the root has exactly one parent that
/// comes before it, which rules out cycles and shared subtrees.
pub fn validate_tree(nodes: &[Node]) -> Result<(), String> {
    if nodes.is_empty() {
        return Err("tree has no nodes".into());
    }
    if nodes.len() > MAX_ANY_VALUE_NODES {
        return Err(format!("tree has more than {MAX_ANY_VALUE_NODES} nodes"));
    }
    let mut depth = vec![usize::MAX; nodes.len()];
    depth[0] = 0;
    for (i, node) in nodes.iter().enumerate() {
        if depth[i] == usize::MAX {
            return Err(format!("node {i} has no parent"));
        }
        let children: Vec<u32> = match node {
            Node::Leaf(_) => continue,
            Node::Array(c) => c.clone(),
            Node::Kvlist(kv) => kv.iter().map(|(_, c)| *c).collect(),
        };
        for c in children {
            let c = c as usize;
            if c <= i || c >= nodes.len() {
                return Err(format!("node {i} points to node {c}, which is not after it"));
            }
            if depth[c] != usize::MAX {
                return Err(format!("node {c} has more than one parent"));
            }
            depth[c] = depth[i] + 1;
            if depth[c] > MAX_ANY_VALUE_DEPTH {
                return Err(format!("tree is deeper than {MAX_ANY_VALUE_DEPTH}"));
            }
        }
    }
    Ok(())
}

/// Render an attribute value as text, for formats such as Influx tags and
/// Prometheus labels that only carry strings. Nested values become JSON here,
/// and only here.
pub fn any_value_to_string(v: &AnyValue) -> String {
    match v {
        AnyValue::Leaf(l) => leaf_to_string(l),
        AnyValue::Tree(nodes) => node_to_json(nodes, 0).to_string(),
    }
}

fn leaf_to_string(l: &Leaf) -> String {
    match l {
        Leaf::Empty => String::new(),
        Leaf::Str(s) => s.clone(),
        Leaf::Bool(b) => b.to_string(),
        Leaf::Int(i) => i.to_string(),
        Leaf::Double(d) => d.to_string(),
        Leaf::Bytes(b) => base64(b),
    }
}

fn node_to_json(nodes: &[Node], i: usize) -> serde_json::Value {
    use serde_json::Value;
    match &nodes[i] {
        Node::Leaf(l) => match l {
            Leaf::Empty => Value::Null,
            Leaf::Str(s) => Value::String(s.clone()),
            Leaf::Bool(b) => Value::Bool(*b),
            Leaf::Int(n) => Value::from(*n),
            Leaf::Double(d) => serde_json::Number::from_f64(*d).map_or(Value::Null, Value::Number),
            Leaf::Bytes(b) => Value::String(base64(b)),
        },
        Node::Array(c) => Value::Array(c.iter().map(|c| node_to_json(nodes, *c as usize)).collect()),
        Node::Kvlist(kv) => Value::Object(
            kv.iter()
                .map(|(k, c)| (k.clone(), node_to_json(nodes, *c as usize)))
                .collect(),
        ),
    }
}

pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Shorthand constructors used by the decoders and tests.
pub fn kv_str(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: AnyValue::Leaf(Leaf::Str(value.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn tree_rules() {
        let leaf = || Node::Leaf(Leaf::Int(1));
        assert!(validate_tree(&[Node::Array(vec![1, 2]), leaf(), leaf()]).is_ok());
        // Child before parent.
        assert!(validate_tree(&[Node::Array(vec![0])]).is_err());
        // Shared child.
        assert!(validate_tree(&[Node::Array(vec![1, 1]), leaf()]).is_err());
        // Orphan.
        assert!(validate_tree(&[leaf(), leaf()]).is_err());
        let json = node_to_json(
            &[
                Node::Kvlist(vec![("a".into(), 1), ("b".into(), 2)]),
                leaf(),
                Node::Array(vec![3]),
                Node::Leaf(Leaf::Str("x".into())),
            ],
            0,
        );
        assert_eq!(json.to_string(), r#"{"a":1,"b":["x"]}"#);
    }
}
