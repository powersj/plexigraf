//! OTLP JSON: the full internal model as an ExportMetricsServiceRequest, one
//! object per batch. 64-bit integers are strings, as the OTLP JSON mapping
//! requires.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::bindings::metrics::{
    AggregationTemporality, AnyValue, Buckets, Data, DataPointFlags, Exemplar, KeyValue, Leaf, Node, Number,
};
use crate::model::{BatchData, base64};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodeOptions {
    /// Indent the output; otherwise each batch is one line (JSON Lines).
    #[serde(default)]
    pretty: bool,
}

pub fn encode(opts: EncodeOptions, batch: &BatchData) -> Result<Vec<u8>, String> {
    if batch.metrics.is_empty() {
        return Ok(Vec::new());
    }
    // Group metrics by resource, then scope, keeping first-seen order.
    let mut resource_metrics: Vec<(u32, Vec<(u32, Vec<Value>)>)> = Vec::new();
    for m in &batch.metrics {
        let ri = match resource_metrics.iter().position(|(r, _)| *r == m.resource) {
            Some(i) => i,
            None => {
                resource_metrics.push((m.resource, Vec::new()));
                resource_metrics.len() - 1
            }
        };
        let scopes = &mut resource_metrics[ri].1;
        let si = match scopes.iter().position(|(s, _)| *s == m.scope) {
            Some(i) => i,
            None => {
                scopes.push((m.scope, Vec::new()));
                scopes.len() - 1
            }
        };
        let mut obj = Map::new();
        obj.insert("name".into(), json!(m.name));
        if !m.description.is_empty() {
            obj.insert("description".into(), json!(m.description));
        }
        if !m.unit.is_empty() {
            obj.insert("unit".into(), json!(m.unit));
        }
        let (key, data) = data_json(&m.data);
        obj.insert(key.into(), data);
        if !m.metadata.is_empty() {
            obj.insert("metadata".into(), attrs(&m.metadata));
        }
        scopes[si].1.push(Value::Object(obj));
    }

    let resource_metrics: Vec<Value> = resource_metrics
        .into_iter()
        .map(|(ri, scopes)| {
            let r = &batch.resources[ri as usize];
            let mut resource = Map::new();
            resource.insert("attributes".into(), attrs(&r.attributes));
            if r.dropped_attributes_count > 0 {
                resource.insert("droppedAttributesCount".into(), json!(r.dropped_attributes_count));
            }
            let scope_metrics: Vec<Value> = scopes
                .into_iter()
                .map(|(si, metrics)| {
                    let s = &batch.scopes[si as usize];
                    let mut scope = Map::new();
                    scope.insert("name".into(), json!(s.name));
                    if !s.version.is_empty() {
                        scope.insert("version".into(), json!(s.version));
                    }
                    if !s.attributes.is_empty() {
                        scope.insert("attributes".into(), attrs(&s.attributes));
                    }
                    let mut sm = Map::new();
                    sm.insert("scope".into(), Value::Object(scope));
                    sm.insert("metrics".into(), Value::Array(metrics));
                    if !s.schema_url.is_empty() {
                        sm.insert("schemaUrl".into(), json!(s.schema_url));
                    }
                    Value::Object(sm)
                })
                .collect();
            let mut rm = Map::new();
            rm.insert("resource".into(), Value::Object(resource));
            rm.insert("scopeMetrics".into(), Value::Array(scope_metrics));
            if !r.schema_url.is_empty() {
                rm.insert("schemaUrl".into(), json!(r.schema_url));
            }
            Value::Object(rm)
        })
        .collect();

    let doc = json!({ "resourceMetrics": resource_metrics });
    let mut out = if opts.pretty {
        serde_json::to_vec_pretty(&doc)
    } else {
        serde_json::to_vec(&doc)
    }
    .map_err(|e| e.to_string())?;
    out.push(b'\n');
    Ok(out)
}

fn temporality(t: AggregationTemporality) -> u8 {
    match t {
        AggregationTemporality::Unspecified => 0,
        AggregationTemporality::Delta => 1,
        AggregationTemporality::Cumulative => 2,
    }
}

fn data_json(data: &Data) -> (&'static str, Value) {
    match data {
        Data::Gauge(g) => (
            "gauge",
            json!({ "dataPoints": g.data_points.iter().map(number_point).collect::<Vec<_>>() }),
        ),
        Data::Sum(s) => (
            "sum",
            json!({
                "aggregationTemporality": temporality(s.aggregation_temporality),
                "isMonotonic": s.is_monotonic,
                "dataPoints": s.data_points.iter().map(number_point).collect::<Vec<_>>(),
            }),
        ),
        Data::Histogram(h) => (
            "histogram",
            json!({
                "aggregationTemporality": temporality(h.aggregation_temporality),
                "dataPoints": h.data_points.iter().map(|p| {
                    let mut o = point_common(&p.attributes, p.start_time_unix_nano, p.time_unix_nano, p.flags);
                    o.insert("count".into(), json!(p.count.to_string()));
                    p.sum.inspect(|v| { o.insert("sum".into(), json!(v)); });
                    o.insert("bucketCounts".into(), json!(p.bucket_counts.iter().map(u64::to_string).collect::<Vec<_>>()));
                    o.insert("explicitBounds".into(), json!(p.explicit_bounds));
                    p.min.inspect(|v| { o.insert("min".into(), json!(v)); });
                    p.max.inspect(|v| { o.insert("max".into(), json!(v)); });
                    exemplars(&mut o, &p.exemplars);
                    Value::Object(o)
                }).collect::<Vec<_>>(),
            }),
        ),
        Data::ExponentialHistogram(h) => (
            "exponentialHistogram",
            json!({
                "aggregationTemporality": temporality(h.aggregation_temporality),
                "dataPoints": h.data_points.iter().map(|p| {
                    let mut o = point_common(&p.attributes, p.start_time_unix_nano, p.time_unix_nano, p.flags);
                    o.insert("count".into(), json!(p.count.to_string()));
                    p.sum.inspect(|v| { o.insert("sum".into(), json!(v)); });
                    o.insert("scale".into(), json!(p.scale));
                    o.insert("zeroCount".into(), json!(p.zero_count.to_string()));
                    o.insert("positive".into(), buckets(&p.positive));
                    o.insert("negative".into(), buckets(&p.negative));
                    p.min.inspect(|v| { o.insert("min".into(), json!(v)); });
                    p.max.inspect(|v| { o.insert("max".into(), json!(v)); });
                    if p.zero_threshold != 0.0 {
                        o.insert("zeroThreshold".into(), json!(p.zero_threshold));
                    }
                    exemplars(&mut o, &p.exemplars);
                    Value::Object(o)
                }).collect::<Vec<_>>(),
            }),
        ),
        Data::Summary(s) => (
            "summary",
            json!({
                "dataPoints": s.data_points.iter().map(|p| {
                    let mut o = point_common(&p.attributes, p.start_time_unix_nano, p.time_unix_nano, p.flags);
                    o.insert("count".into(), json!(p.count.to_string()));
                    o.insert("sum".into(), json!(p.sum));
                    o.insert("quantileValues".into(), json!(p.quantile_values.iter()
                        .map(|q| json!({ "quantile": q.quantile, "value": q.value })).collect::<Vec<_>>()));
                    Value::Object(o)
                }).collect::<Vec<_>>(),
            }),
        ),
    }
}

fn buckets(b: &Buckets) -> Value {
    json!({ "offset": b.offset, "bucketCounts": b.bucket_counts.iter().map(u64::to_string).collect::<Vec<_>>() })
}

fn point_common(a: &[KeyValue], start: u64, time: u64, flags: DataPointFlags) -> Map<String, Value> {
    let mut o = Map::new();
    if !a.is_empty() {
        o.insert("attributes".into(), attrs(a));
    }
    if start != 0 {
        o.insert("startTimeUnixNano".into(), json!(start.to_string()));
    }
    o.insert("timeUnixNano".into(), json!(time.to_string()));
    if flags.contains(DataPointFlags::NO_RECORDED_VALUE) {
        o.insert("flags".into(), json!(1));
    }
    o
}

fn number_point(p: &crate::bindings::metrics::NumberDataPoint) -> Value {
    let mut o = point_common(&p.attributes, p.start_time_unix_nano, p.time_unix_nano, p.flags);
    let (k, v) = number(&p.value);
    o.insert(k.into(), v);
    exemplars(&mut o, &p.exemplars);
    Value::Object(o)
}

fn number(n: &Number) -> (&'static str, Value) {
    match n {
        Number::Int(i) => ("asInt", json!(i.to_string())),
        Number::Double(d) => ("asDouble", json!(d)),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn exemplars(o: &mut Map<String, Value>, ex: &[Exemplar]) {
    if ex.is_empty() {
        return;
    }
    let list: Vec<Value> = ex
        .iter()
        .map(|e| {
            let mut m = Map::new();
            if !e.filtered_attributes.is_empty() {
                m.insert("filteredAttributes".into(), attrs(&e.filtered_attributes));
            }
            m.insert("timeUnixNano".into(), json!(e.time_unix_nano.to_string()));
            let (k, v) = number(&e.value);
            m.insert(k.into(), v);
            if let Some(t) = &e.trace_id {
                m.insert("traceId".into(), json!(hex(t)));
            }
            if let Some(s) = &e.span_id {
                m.insert("spanId".into(), json!(hex(s)));
            }
            Value::Object(m)
        })
        .collect();
    o.insert("exemplars".into(), Value::Array(list));
}

fn attrs(kvs: &[KeyValue]) -> Value {
    Value::Array(
        kvs.iter()
            .map(|kv| json!({ "key": kv.key, "value": any_value(&kv.value) }))
            .collect(),
    )
}

fn any_value(v: &AnyValue) -> Value {
    match v {
        AnyValue::Leaf(l) => leaf(l),
        AnyValue::Tree(nodes) => node(nodes, 0),
    }
}

fn leaf(l: &Leaf) -> Value {
    match l {
        Leaf::Empty => json!({}),
        Leaf::Str(s) => json!({ "stringValue": s }),
        Leaf::Bool(b) => json!({ "boolValue": b }),
        Leaf::Int(i) => json!({ "intValue": i.to_string() }),
        Leaf::Double(d) => json!({ "doubleValue": d }),
        Leaf::Bytes(b) => json!({ "bytesValue": base64(b) }),
    }
}

fn node(nodes: &[Node], i: usize) -> Value {
    match &nodes[i] {
        Node::Leaf(l) => leaf(l),
        Node::Array(c) => {
            json!({ "arrayValue": { "values": c.iter().map(|c| node(nodes, *c as usize)).collect::<Vec<_>>() } })
        }
        Node::Kvlist(kv) => json!({ "kvlistValue": { "values": kv.iter()
            .map(|(k, c)| json!({ "key": k, "value": node(nodes, *c as usize) })).collect::<Vec<_>>() } }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::metrics::{InstrumentationScope, Metric, NumberDataPoint, Sum};
    use crate::model::{empty_resource, kv_str};

    #[test]
    fn matches_the_design_cpu_example() {
        let mut resource = empty_resource();
        resource.attributes = vec![kv_str("host.name", "web1"), kv_str("os.type", "linux")];
        let point = |mode: &str, v: f64| NumberDataPoint {
            attributes: vec![
                KeyValue {
                    key: "cpu.logical_number".into(),
                    value: AnyValue::Leaf(Leaf::Int(0)),
                },
                kv_str("cpu.mode", mode),
            ],
            start_time_unix_nano: 1761868800000000000,
            time_unix_nano: 1761955200000000000,
            value: Number::Double(v),
            exemplars: vec![],
            flags: DataPointFlags::empty(),
        };
        let b = BatchData {
            resources: vec![resource],
            scopes: vec![InstrumentationScope {
                name: "tern/receivers/cpu".into(),
                version: "1.0.0".into(),
                attributes: vec![],
                dropped_attributes_count: 0,
                schema_url: String::new(),
            }],
            metrics: vec![Metric {
                resource: 0,
                scope: 0,
                name: "system.cpu.time".into(),
                description: "Seconds each logical CPU spent in each mode".into(),
                unit: "s".into(),
                data: Data::Sum(Sum {
                    data_points: vec![point("user", 51234.5), point("system", 8120.25)],
                    aggregation_temporality: AggregationTemporality::Cumulative,
                    is_monotonic: true,
                }),
                metadata: vec![],
            }],
        };
        let out: Value = serde_json::from_slice(&encode(EncodeOptions::default(), &b).unwrap()).unwrap();
        let expected: Value = serde_json::from_str(
            r#"{
          "resourceMetrics": [{
            "resource": { "attributes": [
              { "key": "host.name", "value": { "stringValue": "web1" } },
              { "key": "os.type",   "value": { "stringValue": "linux" } }
            ]},
            "scopeMetrics": [{
              "scope": { "name": "tern/receivers/cpu", "version": "1.0.0" },
              "metrics": [{
                "name": "system.cpu.time",
                "description": "Seconds each logical CPU spent in each mode",
                "unit": "s",
                "sum": {
                  "aggregationTemporality": 2,
                  "isMonotonic": true,
                  "dataPoints": [
                    { "attributes": [
                        { "key": "cpu.logical_number", "value": { "intValue": "0" } },
                        { "key": "cpu.mode", "value": { "stringValue": "user" } } ],
                      "startTimeUnixNano": "1761868800000000000",
                      "timeUnixNano": "1761955200000000000",
                      "asDouble": 51234.5 },
                    { "attributes": [
                        { "key": "cpu.logical_number", "value": { "intValue": "0" } },
                        { "key": "cpu.mode", "value": { "stringValue": "system" } } ],
                      "startTimeUnixNano": "1761868800000000000",
                      "timeUnixNano": "1761955200000000000",
                      "asDouble": 8120.25 }
                  ]
                }
              }]
            }]
          }]
        }"#,
        )
        .unwrap();
        assert_eq!(out, expected);
    }
}
