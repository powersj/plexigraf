// Package plexigraf is the SDK for writing Plexigraf plugins in Go.
//
// A plugin is a plain struct registered with the package for its world:
// scraper.Register, processor.Register, or exporter.Register. The SDK decodes
// the component's TOML settings into the struct and calls its methods; plugin
// code never touches the generated WIT bindings.
//
// Build with componentize-go:
//
//	go tool componentize-go -d <wit> -w plexigraf:plugin/scraper build
package plexigraf

import (
	"errors"
	"fmt"
	"time"

	witTypes "go.bytecodealliance.org/pkg/wit/types"

	"github.com/powersj/plexigraf/sdk/go/internal/imports/plexigraf_metrics_metrics"
	"github.com/powersj/plexigraf/sdk/go/internal/imports/plexigraf_plugin_encoding"
	"github.com/powersj/plexigraf/sdk/go/internal/imports/plexigraf_plugin_host"
	"github.com/powersj/plexigraf/sdk/go/internal/imports/plexigraf_plugin_types"
)

// Raw plexigraf:metrics types, which mirror the OTLP data model.
type (
	RawMetric    = plexigraf_metrics_metrics.Metric
	KeyValue     = plexigraf_metrics_metrics.KeyValue
	Resource     = plexigraf_metrics_metrics.Resource
	Scope        = plexigraf_metrics_metrics.InstrumentationScope
	PluginError  = plexigraf_plugin_types.PluginError
	ExportResult = plexigraf_plugin_types.ExportOutcome
)

// Time is nanoseconds since the Unix epoch, as handed out by the core.
type Time uint64

func (t Time) UnixNano() uint64 { return uint64(t) }

// ---- Errors ----

type kindError struct {
	kind uint8
	err  error
}

func (e *kindError) Error() string { return e.err.Error() }
func (e *kindError) Unwrap() error { return e.err }

// ConfigError marks bad settings; the component fails to start.
func ConfigError(err error) error { return &kindError{plexigraf_plugin_types.PluginError_Config, err} }

// TransientError marks a failure worth retrying.
func TransientError(err error) error { return &kindError{plexigraf_plugin_types.PluginError_Transient, err} }

// FatalError stops the component.
func FatalError(err error) error { return &kindError{plexigraf_plugin_types.PluginError_Fatal, err} }

// ToPluginError converts a Go error to the WIT error, using def for plain errors.
func ToPluginError(err error, def uint8) PluginError {
	kind := def
	var k *kindError
	if errors.As(err, &k) {
		kind = k.kind
	}
	switch kind {
	case plexigraf_plugin_types.PluginError_Config:
		return plexigraf_plugin_types.MakePluginErrorConfig(err.Error())
	case plexigraf_plugin_types.PluginError_Fatal:
		return plexigraf_plugin_types.MakePluginErrorFatal(err.Error())
	default:
		return plexigraf_plugin_types.MakePluginErrorTransient(err.Error())
	}
}

// Error kinds for ToPluginError.
const (
	KindConfig    = plexigraf_plugin_types.PluginError_Config
	KindTransient = plexigraf_plugin_types.PluginError_Transient
	KindFatal     = plexigraf_plugin_types.PluginError_Fatal
)

// ---- Batches ----

// Batch is a host-owned batch of metrics.
type Batch struct {
	h *plexigraf_metrics_metrics.Batch
}

// NewBatch creates an empty batch on the host.
func NewBatch() *Batch { return &Batch{plexigraf_metrics_metrics.MakeBatch()} }

// FromHandle wraps a batch handed to the plugin by the host.
func FromHandle(h *plexigraf_metrics_metrics.Batch) *Batch { return &Batch{h} }

// Handle gives the batch back to the host.
func (b *Batch) Handle() *plexigraf_metrics_metrics.Batch { return b.h }

func (b *Batch) Push(m *Metric)                { b.h.Push(m.raw) }
func (b *Batch) Len() int                      { return int(b.h.Len()) }
func (b *Batch) Get(i int) RawMetric           { return b.h.Get(uint32(i)) }
func (b *Batch) Set(i int, m RawMetric)        { b.h.Set(uint32(i), m) }
func (b *Batch) Extend(ms []RawMetric)         { b.h.Extend(ms) }
func (b *Batch) TakeAll() []RawMetric          { return b.h.TakeAll() }
func (b *Batch) Resource(i uint32) Resource    { return b.h.ResourceAt(i) }
func (b *Batch) Scope(i uint32) Scope          { return b.h.ScopeAt(i) }
func (b *Batch) AddResource(r Resource) uint32 { return b.h.AddResource(r) }
func (b *Batch) AddScope(s Scope) uint32       { return b.h.AddScope(s) }

// Retain keeps only the metrics for which keep returns true, in one pass.
func (b *Batch) Retain(keep func(RawMetric) bool) {
	flags := make([]bool, b.Len())
	for i := range flags {
		flags[i] = keep(b.Get(i))
	}
	b.h.Retain(flags)
}

// ---- Metrics ----

// Metric is a metric under construction: one name, unit, and type, plus
// data points.
type Metric struct {
	raw    RawMetric
	start  uint64
	points []plexigraf_metrics_metrics.NumberDataPoint
	sum    *plexigraf_metrics_metrics.Sum
}

// Gauge starts a sampled value, such as a temperature or utilization.
func Gauge(name, unit string) *Metric {
	return &Metric{raw: RawMetric{Name: name, Unit: unit}}
}

// Counter starts a monotonic, cumulative sum, such as bytes sent.
func Counter(name, unit string) *Metric {
	return &Metric{raw: RawMetric{Name: name, Unit: unit}, sum: &plexigraf_metrics_metrics.Sum{
		AggregationTemporality: plexigraf_metrics_metrics.AggregationTemporality_Cumulative, IsMonotonic: true}}
}

// UpDown starts a cumulative sum that can go down, such as memory in use.
func UpDown(name, unit string) *Metric {
	return &Metric{raw: RawMetric{Name: name, Unit: unit}, sum: &plexigraf_metrics_metrics.Sum{
		AggregationTemporality: plexigraf_metrics_metrics.AggregationTemporality_Cumulative}}
}

// Description sets the metric's description.
func (m *Metric) Description(d string) *Metric { m.raw.Description = d; return m }

// StartTime sets the start time for the points that follow, for sums.
func (m *Metric) StartTime(t Time) *Metric { m.start = uint64(t); return m }

// Point adds a data point. value is any Go integer or float type.
func (m *Metric) Point(now Time, value any, attrs []KeyValue) *Metric {
	m.points = append(m.points, plexigraf_metrics_metrics.NumberDataPoint{
		Attributes:        attrs,
		StartTimeUnixNano: m.start,
		TimeUnixNano:      uint64(now),
		Value:             number(value),
	})
	if m.sum != nil {
		m.sum.DataPoints = m.points
		m.raw.Data = plexigraf_metrics_metrics.MakeDataSum(*m.sum)
	} else {
		m.raw.Data = plexigraf_metrics_metrics.MakeDataGauge(plexigraf_metrics_metrics.Gauge{DataPoints: m.points})
	}
	return m
}

// Raw returns the metric as the WIT record.
func (m *Metric) Raw() RawMetric { return m.raw }

func number(v any) plexigraf_metrics_metrics.Number {
	switch n := v.(type) {
	case int:
		return plexigraf_metrics_metrics.MakeNumberInt(int64(n))
	case int8:
		return plexigraf_metrics_metrics.MakeNumberInt(int64(n))
	case int16:
		return plexigraf_metrics_metrics.MakeNumberInt(int64(n))
	case int32:
		return plexigraf_metrics_metrics.MakeNumberInt(int64(n))
	case int64:
		return plexigraf_metrics_metrics.MakeNumberInt(n)
	case uint8:
		return plexigraf_metrics_metrics.MakeNumberInt(int64(n))
	case uint16:
		return plexigraf_metrics_metrics.MakeNumberInt(int64(n))
	case uint32:
		return plexigraf_metrics_metrics.MakeNumberInt(int64(n))
	case uint:
		return plexigraf_metrics_metrics.MakeNumberDouble(float64(n))
	case uint64:
		return plexigraf_metrics_metrics.MakeNumberDouble(float64(n))
	case float32:
		return plexigraf_metrics_metrics.MakeNumberDouble(float64(n))
	case float64:
		return plexigraf_metrics_metrics.MakeNumberDouble(n)
	case bool:
		if n {
			return plexigraf_metrics_metrics.MakeNumberInt(1)
		}
		return plexigraf_metrics_metrics.MakeNumberInt(0)
	default:
		panic(fmt.Sprintf("plexigraf: metric value must be a number, got %T", v))
	}
}

// Attrs builds string attributes from key, value pairs.
func Attrs(kv ...string) []KeyValue {
	if len(kv)%2 != 0 {
		panic("plexigraf.Attrs needs an even number of arguments")
	}
	out := make([]KeyValue, 0, len(kv)/2)
	for i := 0; i < len(kv); i += 2 {
		out = append(out, Attr(kv[i], kv[i+1]))
	}
	return out
}

// Attr builds one attribute from a string, bool, integer, or float.
func Attr(key string, value any) KeyValue {
	var leaf plexigraf_metrics_metrics.Leaf
	switch v := value.(type) {
	case string:
		leaf = plexigraf_metrics_metrics.MakeLeafStr(v)
	case bool:
		leaf = plexigraf_metrics_metrics.MakeLeafBool(v)
	case int:
		leaf = plexigraf_metrics_metrics.MakeLeafInt(int64(v))
	case int64:
		leaf = plexigraf_metrics_metrics.MakeLeafInt(v)
	case float64:
		leaf = plexigraf_metrics_metrics.MakeLeafDouble(v)
	case []byte:
		leaf = plexigraf_metrics_metrics.MakeLeafBytes(v)
	default:
		leaf = plexigraf_metrics_metrics.MakeLeafStr(fmt.Sprint(v))
	}
	return KeyValue{Key: key, Value: plexigraf_metrics_metrics.MakeAnyValueLeaf(leaf)}
}

// ---- Export outcomes ----

// Outcome is an exporter's result for one batch, shaped like OTLP partial success.
type Outcome = ExportResult

func OK() Outcome                  { return plexigraf_plugin_types.MakeExportOutcomeOk() }
func Reject(reason string) Outcome { return plexigraf_plugin_types.MakeExportOutcomeReject(reason) }

// Retry asks the core to retry the batch, after at least `after` if non-zero.
func Retry(after time.Duration) Outcome {
	if after <= 0 {
		return plexigraf_plugin_types.MakeExportOutcomeRetry(witTypes.None[uint32]())
	}
	return plexigraf_plugin_types.MakeExportOutcomeRetry(witTypes.Some(uint32(after.Milliseconds())))
}

// Partial reports that some data points were rejected.
func Partial(rejected uint64, reason string) Outcome {
	return plexigraf_plugin_types.MakeExportOutcomePartial(plexigraf_plugin_types.PartialFailure{
		RejectedDataPoints: rejected, Reason: reason, Indices: witTypes.None[[]uint32]()})
}

// ---- Host services ----

func log(level uint8, msg string, kv []string) {
	fields := make([]plexigraf_plugin_host.Kv, 0, len(kv)/2)
	for i := 0; i+1 < len(kv); i += 2 {
		fields = append(fields, plexigraf_plugin_host.Kv{Key: kv[i], Value: kv[i+1]})
	}
	plexigraf_plugin_host.Log(level, msg, fields)
}

// Debug, Info, Warn, and Error log through the agent; kv are key, value pairs.
func Debug(msg string, kv ...string) { log(plexigraf_plugin_host.Level_Debug, msg, kv) }
func Info(msg string, kv ...string)  { log(plexigraf_plugin_host.Level_Info, msg, kv) }
func Warn(msg string, kv ...string)  { log(plexigraf_plugin_host.Level_Warn, msg, kv) }
func Error(msg string, kv ...string) { log(plexigraf_plugin_host.Level_Error, msg, kv) }

// Secret returns a secret the operator approved for this plugin.
func Secret(name string) (string, bool) {
	s := plexigraf_plugin_host.Secret(name)
	return s.SomeOr(""), s.IsSome()
}

// StateLoad and StateSave persist a small blob across restarts.
func StateLoad() ([]byte, bool) {
	s := plexigraf_plugin_host.StateLoad()
	return s.SomeOr(nil), s.IsSome()
}
func StateSave(blob []byte) { plexigraf_plugin_host.StateSave(blob) }

// Encode and Decode use the core's encodings, such as "influx" and "json".
func Encode(format, optionsTOML string, b *Batch) ([]byte, error) {
	r := plexigraf_plugin_encoding.Encode(format, optionsTOML, b.h)
	if r.IsErr() {
		return nil, errors.New(r.Err())
	}
	return r.Ok(), nil
}

func Decode(format, optionsTOML string, data []byte) (*Batch, error) {
	r := plexigraf_plugin_encoding.Decode(format, optionsTOML, data)
	if r.IsErr() {
		return nil, errors.New(r.Err())
	}
	return &Batch{r.Ok()}, nil
}

// ---- Plugin interfaces ----

// Validator is optional: check settings without side effects. `plexigraf config
// check` and startup call it after decoding.
type Validator interface{ Validate() error }

// Starter is optional: called once after settings are decoded.
type Starter interface{ Start() error }

// Shutdowner is optional: called before the component stops.
type Shutdowner interface{ Shutdown() }

// Scraper polls on an interval.
type Scraper interface {
	Scrape(now Time) (*Batch, error)
}

// Processor transforms batches.
type Processor interface {
	Process(b *Batch) (*Batch, error)
}

// Flusher is optional for processors whose manifest sets flush = true.
type Flusher interface {
	Flush(now Time) (*Batch, error)
}

// Exporter sends batches somewhere.
type Exporter interface {
	Export(b *Batch) (Outcome, error)
}
