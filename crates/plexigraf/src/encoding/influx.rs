//! Influx line protocol, for Telegraf compatibility at the edges.
//!
//! Output writes one line per data point, with point and resource attributes
//! as tags and the value in a `value` field. `multi_field_lines` regroups
//! metrics that share a name prefix, tags, and time into one multi-field line.
//! Input turns each field into its own OTel metric named
//! `<measurement>.<field>`.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::Deserialize;

use crate::bindings::metrics::{
    AggregationTemporality, Data, DataPointFlags, Gauge, KeyValue, Metric, Number, NumberDataPoint, Sum,
};
use crate::model::{BatchData, any_value_to_string, empty_resource, kv_str};
use crate::telemetry;
use crate::util::glob_match;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodeOptions {
    #[serde(default)]
    multi_field_lines: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeOptions {
    /// Field-name globs whose values become monotonic cumulative sums.
    #[serde(default)]
    counter_fields: Vec<String>,
    /// What to do with string field values: drop, attribute, or info.
    #[serde(default)]
    string_values: StringValues,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum StringValues {
    #[default]
    Drop,
    Attribute,
    Info,
}

#[derive(Clone, Debug, PartialEq)]
enum Field {
    Int(i64),
    Float(f64),
}

struct Line {
    measurement: String,
    tags: Vec<(String, String)>,
    fields: Vec<(String, Field)>,
    time: u64,
}

#[expect(clippy::unnecessary_wraps, reason = "matches the signature of the other encoders")]
pub fn encode(opts: &EncodeOptions, batch: &BatchData) -> Result<Vec<u8>, String> {
    let mut lines = Vec::new();
    for m in &batch.metrics {
        let resource_attrs = batch
            .resources
            .get(m.resource as usize)
            .map(|r| r.attributes.as_slice())
            .unwrap_or_default();
        metric_lines(m, resource_attrs, &mut lines);
    }
    if opts.multi_field_lines {
        lines = regroup(lines);
    }
    let mut out = String::new();
    for line in &lines {
        write_line(&mut out, line);
    }
    Ok(out.into_bytes())
}

fn tags_for(point_attrs: &[KeyValue], resource_attrs: &[KeyValue]) -> Vec<(String, String)> {
    let mut tags: Vec<(String, String)> = point_attrs
        .iter()
        .chain(resource_attrs)
        .map(|kv| (kv.key.clone(), any_value_to_string(&kv.value)))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect();
    tags.sort_by(|a, b| a.0.cmp(&b.0));
    // A point attribute wins over a resource attribute with the same key.
    tags.dedup_by(|a, b| a.0 == b.0);
    tags
}

/// A count as an integer field, clamped because line protocol integers are signed.
fn count(n: u64) -> Field {
    Field::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn number_field(n: &Number) -> Option<Field> {
    match n {
        Number::Int(i) => Some(Field::Int(*i)),
        Number::Double(d) if d.is_finite() => Some(Field::Float(*d)),
        Number::Double(_) => {
            telemetry::lossy("internal", "influx", "non_finite_value", 1);
            None
        }
    }
}

fn float(name: &str, v: f64, fields: &mut Vec<(String, Field)>) {
    if v.is_finite() {
        fields.push((name.to_string(), Field::Float(v)));
    }
}

fn metric_lines(m: &Metric, resource_attrs: &[KeyValue], out: &mut Vec<Line>) {
    let skip = |flags: DataPointFlags| flags.contains(DataPointFlags::NO_RECORDED_VALUE);
    let mut push = |attrs: &[KeyValue], time: u64, fields: Vec<(String, Field)>| {
        if !fields.is_empty() {
            out.push(Line {
                measurement: m.name.clone(),
                tags: tags_for(attrs, resource_attrs),
                fields,
                time,
            });
        }
    };
    match &m.data {
        Data::Gauge(Gauge { data_points }) | Data::Sum(Sum { data_points, .. }) => {
            for p in data_points.iter().filter(|p| !skip(p.flags)) {
                let fields = number_field(&p.value)
                    .map(|f| ("value".to_string(), f))
                    .into_iter()
                    .collect();
                push(&p.attributes, p.time_unix_nano, fields);
            }
        }
        Data::Histogram(h) => {
            telemetry::lossy("internal", "influx", "histogram", h.data_points.len() as u64);
            for p in h.data_points.iter().filter(|p| !skip(p.flags)) {
                let mut fields = vec![("count".to_string(), count(p.count))];
                p.sum.inspect(|s| float("sum", *s, &mut fields));
                p.min.inspect(|v| float("min", *v, &mut fields));
                p.max.inspect(|v| float("max", *v, &mut fields));
                // Cumulative buckets, keyed by upper bound, as Telegraf writes them.
                let mut acc = 0u64;
                for (i, c) in p.bucket_counts.iter().enumerate() {
                    acc = acc.saturating_add(*c);
                    let le = p.explicit_bounds.get(i).map_or("+Inf".to_string(), ToString::to_string);
                    fields.push((le, count(acc)));
                }
                push(&p.attributes, p.time_unix_nano, fields);
            }
        }
        Data::ExponentialHistogram(h) => {
            telemetry::lossy(
                "internal",
                "influx",
                "exponential_histogram",
                h.data_points.len() as u64,
            );
            for p in h.data_points.iter().filter(|p| !skip(p.flags)) {
                let mut fields = vec![
                    ("count".to_string(), count(p.count)),
                    ("zero_count".to_string(), count(p.zero_count)),
                ];
                p.sum.inspect(|s| float("sum", *s, &mut fields));
                p.min.inspect(|v| float("min", *v, &mut fields));
                p.max.inspect(|v| float("max", *v, &mut fields));
                push(&p.attributes, p.time_unix_nano, fields);
            }
        }
        Data::Summary(s) => {
            for p in s.data_points.iter().filter(|p| !skip(p.flags)) {
                let mut fields = vec![("count".to_string(), count(p.count))];
                float("sum", p.sum, &mut fields);
                for q in &p.quantile_values {
                    float(&q.quantile.to_string(), q.value, &mut fields);
                }
                push(&p.attributes, p.time_unix_nano, fields);
            }
        }
    }
}

/// A regrouped line's identity: measurement, tags, and timestamp.
type GroupKey = (String, Vec<(String, String)>, u64);

/// Merge single-value lines that share a name prefix, tags, and time.
/// `random.int` and `random.float` become `random int=..,float=..`.
fn regroup(lines: Vec<Line>) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut index: HashMap<GroupKey, usize> = HashMap::new();
    for line in lines {
        let single = line.fields.len() == 1 && line.fields[0].0 == "value";
        let Some((prefix, field)) = line.measurement.rsplit_once('.').filter(|_| single) else {
            out.push(line);
            continue;
        };
        let key = (prefix.to_string(), line.tags.clone(), line.time);
        let value = line
            .fields
            .into_iter()
            .next()
            .expect("`single` means exactly one field")
            .1;
        match index.get(&key) {
            Some(&i) if !out[i].fields.iter().any(|(f, _)| f == field) => {
                out[i].fields.push((field.to_string(), value));
            }
            _ => {
                index.insert(key, out.len());
                out.push(Line {
                    measurement: prefix.to_string(),
                    tags: line.tags,
                    fields: vec![(field.to_string(), value)],
                    time: line.time,
                });
            }
        }
    }
    out
}

/// Backslash-escape `chars` and backslash itself; an unescaped trailing
/// backslash would otherwise escape the separator that follows it.
fn escape(s: &str, chars: &[char], out: &mut String) {
    for c in s.chars() {
        if c == '\\' || chars.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
}

fn write_line(out: &mut String, line: &Line) {
    escape(&line.measurement, &[',', ' '], out);
    for (k, v) in &line.tags {
        out.push(',');
        escape(k, &[',', '=', ' '], out);
        out.push('=');
        escape(v, &[',', '=', ' '], out);
    }
    out.push(' ');
    for (i, (k, v)) in line.fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        escape(k, &[',', '=', ' '], out);
        out.push('=');
        match v {
            Field::Int(n) => {
                let _ = write!(out, "{n}i");
            }
            Field::Float(f) => out.push_str(&f.to_string()),
        }
    }
    if line.time != 0 {
        out.push(' ');
        out.push_str(&line.time.to_string());
    }
    out.push('\n');
}

// ---- Decoding ----

#[derive(Debug, PartialEq)]
enum RawField {
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
    Str(String),
}

pub fn decode(opts: &DecodeOptions, data: &[u8], now: u64) -> Result<BatchData, String> {
    let text = std::str::from_utf8(data).map_err(|_| "line protocol must be UTF-8".to_string())?;
    let mut batch = BatchData {
        resources: vec![empty_resource()],
        ..Default::default()
    };
    // (metric name, is counter) -> index, so repeated names share one metric.
    let mut index: HashMap<(String, bool), usize> = HashMap::new();
    for (n, raw) in text.lines().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let (measurement, tags, fields, time) = parse_line(raw).map_err(|e| format!("line {}: {e}", n + 1))?;
        let time = time.unwrap_or(now);
        let mut attrs: Vec<KeyValue> = tags.iter().map(|(k, v)| kv_str(k, v)).collect();
        if opts.string_values == StringValues::Attribute {
            for (k, v) in &fields {
                if let RawField::Str(s) = v {
                    attrs.push(kv_str(k, s));
                }
            }
        }
        for (field, value) in fields {
            let name = format!("{measurement}.{field}");
            let mut point_attrs = attrs.clone();
            let value = match value {
                RawField::Int(i) => Number::Int(i),
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "values past i64::MAX fall back to the nearest double"
                )]
                RawField::Uint(u) => i64::try_from(u).map_or(Number::Double(u as f64), Number::Int),
                RawField::Float(f) => Number::Double(f),
                RawField::Bool(b) => Number::Int(i64::from(b)),
                RawField::Str(s) => match opts.string_values {
                    StringValues::Info => {
                        point_attrs.push(kv_str("value", &s));
                        Number::Int(1)
                    }
                    StringValues::Drop | StringValues::Attribute => {
                        if opts.string_values == StringValues::Drop {
                            telemetry::lossy("influx", "internal", "string_value", 1);
                        }
                        continue;
                    }
                },
            };
            let counter = opts.counter_fields.iter().any(|g| glob_match(g, &field));
            let point = NumberDataPoint {
                attributes: point_attrs,
                start_time_unix_nano: 0,
                time_unix_nano: time,
                value,
                exemplars: Vec::new(),
                flags: DataPointFlags::empty(),
            };
            let i = *index.entry((name.clone(), counter)).or_insert_with(|| {
                batch.metrics.push(Metric {
                    resource: 0,
                    scope: 0,
                    name,
                    description: String::new(),
                    unit: String::new(),
                    data: if counter {
                        Data::Sum(Sum {
                            data_points: Vec::new(),
                            aggregation_temporality: AggregationTemporality::Cumulative,
                            is_monotonic: true,
                        })
                    } else {
                        Data::Gauge(Gauge {
                            data_points: Vec::new(),
                        })
                    },
                    metadata: Vec::new(),
                });
                batch.metrics.len() - 1
            });
            match &mut batch.metrics[i].data {
                Data::Gauge(g) => g.data_points.push(point),
                Data::Sum(s) => s.data_points.push(point),
                _ => unreachable!(),
            }
        }
    }
    if batch.metrics.is_empty() {
        batch.resources.clear();
    }
    Ok(batch)
}

type Parsed = (String, Vec<(String, String)>, Vec<(String, RawField)>, Option<u64>);

fn parse_line(line: &str) -> Result<Parsed, String> {
    let mut cur = Cursor {
        s: line.as_bytes(),
        i: 0,
    };
    let measurement = cur.until(&[',', ' '], false);
    if measurement.is_empty() {
        return Err("missing measurement".into());
    }
    let mut tags = Vec::new();
    while cur.peek() == Some(b',') {
        cur.i += 1;
        let key = cur.until(&['='], false);
        if cur.peek() != Some(b'=') {
            return Err(format!("tag {key:?} has no value"));
        }
        cur.i += 1;
        let value = cur.until(&[',', ' '], false);
        tags.push((key, value));
    }
    if cur.peek() != Some(b' ') {
        return Err("missing fields".into());
    }
    cur.i += 1;
    let mut fields = Vec::new();
    loop {
        let key = cur.until(&['='], false);
        if cur.peek() != Some(b'=') {
            return Err(format!("field {key:?} has no value"));
        }
        cur.i += 1;
        let raw = cur.until(&[',', ' '], true);
        fields.push((
            key.clone(),
            parse_field(&raw).map_err(|e| format!("field {key:?}: {e}"))?,
        ));
        match cur.peek() {
            Some(b',') => cur.i += 1,
            _ => break,
        }
    }
    let time = match cur.peek() {
        Some(b' ') => {
            let t = line[cur.i + 1..].trim();
            Some(t.parse::<u64>().map_err(|_| format!("invalid timestamp {t:?}"))?)
        }
        None => None,
        Some(_) => return Err("unexpected text after fields".into()),
    };
    Ok((measurement, tags, fields, time))
}

fn parse_field(raw: &str) -> Result<RawField, String> {
    if let Some(s) = raw.strip_prefix('"') {
        let s = s.strip_suffix('"').ok_or("unterminated string")?;
        return Ok(RawField::Str(s.replace("\\\"", "\"").replace("\\\\", "\\")));
    }
    match raw {
        "t" | "T" | "true" | "True" | "TRUE" => return Ok(RawField::Bool(true)),
        "f" | "F" | "false" | "False" | "FALSE" => return Ok(RawField::Bool(false)),
        _ => {}
    }
    if let Some(n) = raw.strip_suffix('i') {
        return n
            .parse()
            .map(RawField::Int)
            .map_err(|_| format!("invalid integer {raw:?}"));
    }
    if let Some(n) = raw.strip_suffix('u') {
        return n
            .parse()
            .map(RawField::Uint)
            .map_err(|_| format!("invalid unsigned {raw:?}"));
    }
    raw.parse()
        .map(RawField::Float)
        .map_err(|_| format!("invalid value {raw:?}"))
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// Read up to an unescaped stop character. Backslash escapes are removed
    /// unless `quoted` is set, where the raw text is kept for `parse_field` and
    /// stop characters inside double quotes are ignored.
    fn until(&mut self, stops: &[char], quoted: bool) -> String {
        let mut out = Vec::new();
        let mut in_quotes = false;
        while let Some(c) = self.peek() {
            if c == b'\\' && self.i + 1 < self.s.len() {
                if quoted {
                    out.push(c);
                }
                out.push(self.s[self.i + 1]);
                self.i += 2;
                continue;
            }
            if quoted && c == b'"' {
                in_quotes = !in_quotes;
            } else if !in_quotes && stops.contains(&(c as char)) {
                break;
            }
            out.push(c);
            self.i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::metrics::InstrumentationScope;

    fn gauge(name: &str, value: Number, attrs: Vec<KeyValue>, time: u64) -> Metric {
        Metric {
            resource: 0,
            scope: 0,
            name: name.into(),
            description: String::new(),
            unit: "1".into(),
            data: Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    attributes: attrs,
                    start_time_unix_nano: 0,
                    time_unix_nano: time,
                    value,
                    exemplars: vec![],
                    flags: DataPointFlags::empty(),
                }],
            }),
            metadata: vec![],
        }
    }

    fn batch(metrics: Vec<Metric>) -> BatchData {
        BatchData {
            resources: vec![empty_resource()],
            scopes: vec![InstrumentationScope {
                name: "test".into(),
                version: String::new(),
                attributes: vec![],
                dropped_attributes_count: 0,
                schema_url: String::new(),
            }],
            metrics,
        }
    }

    fn text(opts: &EncodeOptions, b: &BatchData) -> String {
        String::from_utf8(encode(opts, b).unwrap()).unwrap()
    }

    const T: u64 = 1_761_955_200_000_000_000;

    #[test]
    fn encodes_one_line_per_metric_as_in_the_design() {
        let attrs = vec![kv_str("source", "plexigraf")];
        let b = batch(vec![
            gauge("random.int", Number::Int(17), attrs.clone(), T),
            gauge("random.float", Number::Double(0.4213), attrs, T),
        ]);
        assert_eq!(
            text(&EncodeOptions::default(), &b),
            "random.int,source=plexigraf value=17i 1761955200000000000\n\
             random.float,source=plexigraf value=0.4213 1761955200000000000\n"
        );
        assert_eq!(
            text(
                &EncodeOptions {
                    multi_field_lines: true
                },
                &b
            ),
            "random,source=plexigraf int=17i,float=0.4213 1761955200000000000\n"
        );
    }

    #[test]
    fn resource_attributes_become_tags() {
        let mut b = batch(vec![gauge(
            "system.cpu.time",
            Number::Double(1.5),
            vec![kv_str("cpu.mode", "user")],
            T,
        )]);
        b.resources[0].attributes.push(kv_str("host.name", "web1"));
        assert_eq!(
            text(&EncodeOptions::default(), &b),
            "system.cpu.time,cpu.mode=user,host.name=web1 value=1.5 1761955200000000000\n"
        );
    }

    #[test]
    fn escapes_special_characters() {
        let b = batch(vec![gauge("a b,c", Number::Int(1), vec![kv_str("k=1", "v 2,x")], T)]);
        assert_eq!(
            text(&EncodeOptions::default(), &b),
            "a\\ b\\,c,k\\=1=v\\ 2\\,x value=1i 1761955200000000000\n"
        );
    }

    #[test]
    fn escapes_backslashes() {
        // Found by the property test below: `host=web\` used to come out as
        // `host=web\ value=..`, where the backslash escaped the space.
        let b = batch(vec![gauge("a", Number::Int(1), vec![kv_str("host", "web\\")], T)]);
        assert_eq!(
            text(&EncodeOptions::default(), &b),
            "a,host=web\\\\ value=1i 1761955200000000000\n"
        );
    }

    #[test]
    fn decodes_the_design_example() {
        let line = br#"nginx,server=web1 active=12i,accepts=4501i,status="ok" 1761955200000000000"#;
        let opts = DecodeOptions {
            counter_fields: vec!["accepts".into()],
            string_values: StringValues::Info,
        };
        let b = decode(&opts, line, 0).unwrap();
        let names: Vec<_> = b.metrics.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["nginx.active", "nginx.accepts", "nginx.status"]);
        assert!(matches!(&b.metrics[0].data, Data::Gauge(_)));
        assert!(matches!(&b.metrics[1].data, Data::Sum(s) if s.is_monotonic));
        let Data::Gauge(g) = &b.metrics[2].data else { panic!() };
        assert!(matches!(g.data_points[0].value, Number::Int(1)));
        assert_eq!(g.data_points[0].attributes.len(), 2);

        // Written back out with multi_field_lines, the numeric fields regroup.
        let mut b = decode(&DecodeOptions::default(), line, 0).unwrap();
        b.normalize("test", "");
        assert_eq!(
            text(
                &EncodeOptions {
                    multi_field_lines: true
                },
                &b
            ),
            "nginx,server=web1 active=12i,accepts=4501i 1761955200000000000\n"
        );
    }

    #[test]
    fn round_trips_line_protocol() {
        let input = "cpu,host=a\\ b,region=us usage=0.5,idle=99i 1700000000000000000\n\
                     mem,host=a used=1024i 1700000000000000000\n";
        let mut b = decode(&DecodeOptions::default(), input.as_bytes(), 0).unwrap();
        b.normalize("test", "");
        let out = text(
            &EncodeOptions {
                multi_field_lines: true,
            },
            &b,
        );
        assert_eq!(out, input);
    }

    #[test]
    fn rejects_bad_lines() {
        assert!(decode(&DecodeOptions::default(), b"cpu", 0).is_err());
        assert!(decode(&DecodeOptions::default(), b"cpu value=", 0).is_err());
        assert!(decode(&DecodeOptions::default(), b"cpu value=1 abc", 0).is_err());
        assert!(decode(&DecodeOptions::default(), b"cpu,host value=1", 0).is_err());
    }

    #[test]
    fn defaults_timestamp_and_handles_quoted_spaces() {
        let b = decode(
            &DecodeOptions {
                string_values: StringValues::Attribute,
                ..Default::default()
            },
            br#"svc msg="hello, world",up=1i"#,
            42,
        )
        .unwrap();
        assert_eq!(b.metrics.len(), 1);
        let Data::Gauge(g) = &b.metrics[0].data else { panic!() };
        assert_eq!(g.data_points[0].time_unix_nano, 42);
        assert_eq!(
            any_value_to_string(&g.data_points[0].attributes[0].value),
            "hello, world"
        );
    }

    fn histogram(count: u64, bucket_counts: Vec<u64>, explicit_bounds: Vec<f64>) -> Metric {
        use crate::bindings::metrics::{Histogram, HistogramDataPoint};
        Metric {
            data: Data::Histogram(Histogram {
                data_points: vec![HistogramDataPoint {
                    attributes: vec![kv_str("host", "web1")],
                    start_time_unix_nano: 0,
                    time_unix_nano: T,
                    count,
                    sum: Some(12.5),
                    bucket_counts,
                    explicit_bounds,
                    exemplars: vec![],
                    flags: DataPointFlags::empty(),
                    min: Some(0.5),
                    max: None,
                }],
                aggregation_temporality: AggregationTemporality::Cumulative,
            }),
            ..gauge("http.duration", Number::Int(0), vec![], T)
        }
    }

    // Snapshot test: the expected output lives in snapshots/*.snap. When the
    // output changes on purpose, run `cargo insta review` to accept it.
    #[test]
    fn histograms_become_cumulative_bucket_fields() {
        let b = batch(vec![
            histogram(6, vec![1, 2, 3], vec![0.1, 1.0]),
            // Counts past i64::MAX clamp instead of wrapping negative.
            histogram(u64::MAX, vec![u64::MAX, 1], vec![1.0]),
        ]);
        insta::assert_snapshot!(text(&EncodeOptions::default(), &b));
    }

    // Property test: proptest generates hundreds of random metrics, including
    // names and tags full of characters that need escaping, and checks that
    // everything the encoder writes decodes back to the same data. On failure
    // it shrinks the input to the smallest one that still fails.
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn encoded_lines_decode_to_the_same_metric(
            measurement in "[a-z][a-z_.]{0,15}",
            tag_key in "[a-z][a-z_.]{0,7}",
            tag_value in "\\PC{1,12}",
            value in prop_oneof![
                any::<i64>().prop_map(Number::Int),
                proptest::num::f64::NORMAL.prop_map(Number::Double),
            ],
        ) {
            let b = batch(vec![gauge(&measurement, value, vec![kv_str(&tag_key, &tag_value)], T)]);
            let encoded = encode(&EncodeOptions::default(), &b).unwrap();
            let decoded = decode(&DecodeOptions::default(), &encoded, 0)
                .map_err(|e| TestCaseError::fail(format!("{e}: {}", String::from_utf8_lossy(&encoded))))?;

            prop_assert_eq!(decoded.metrics.len(), 1);
            let m = &decoded.metrics[0];
            prop_assert_eq!(&m.name, &format!("{measurement}.value"));
            let Data::Gauge(g) = &m.data else { panic!("expected a gauge") };
            let p = &g.data_points[0];
            prop_assert_eq!(p.time_unix_nano, T);
            match (&p.value, &value) {
                (Number::Int(got), Number::Int(want)) => prop_assert_eq!(got, want),
                (Number::Double(got), Number::Double(want)) => prop_assert_eq!(got.to_bits(), want.to_bits()),
                (got, want) => prop_assert!(false, "decoded {got:?}, encoded {want:?}"),
            }
            prop_assert_eq!(p.attributes.len(), 1);
            prop_assert_eq!(&p.attributes[0].key, &tag_key);
            prop_assert_eq!(any_value_to_string(&p.attributes[0].value), tag_value);
        }
    }

    #[test]
    fn decodes_booleans_as_zero_and_one() {
        let b = decode(&DecodeOptions::default(), b"svc up=t,ok=TRUE,down=f,bad=FALSE 1", 0).unwrap();
        let values: Vec<_> = b
            .metrics
            .iter()
            .map(|m| match &m.data {
                Data::Gauge(g) => match g.data_points[0].value {
                    Number::Int(i) => (m.name.as_str(), i),
                    Number::Double(_) => panic!("{} decoded as a double", m.name),
                },
                _ => panic!("{} is not a gauge", m.name),
            })
            .collect();
        assert_eq!(values, [("svc.up", 1), ("svc.ok", 1), ("svc.down", 0), ("svc.bad", 0)]);
    }

    #[test]
    fn drops_non_finite_values() {
        // Line protocol has no NaN or infinity, so those points are left out.
        let b = batch(vec![
            gauge("a", Number::Double(f64::NAN), vec![], T),
            gauge("b", Number::Double(f64::INFINITY), vec![], T),
            gauge("c", Number::Double(1.5), vec![], T),
        ]);
        assert_eq!(text(&EncodeOptions::default(), &b), "c value=1.5 1761955200000000000\n");
    }
}
