// Package export_wit_world implements the processor world's exports, which
// the generated wit_exports package calls.
package export_wit_world

import (
	witTypes "go.bytecodealliance.org/pkg/wit/types"

	plexigraf "github.com/powersj/plexigraf/sdk/go"
	"github.com/powersj/plexigraf/sdk/go/internal/imports/plexigraf_metrics_metrics"
	"github.com/powersj/plexigraf/sdk/go/internal/rt"
)

type batchResult = witTypes.Result[*plexigraf_metrics_metrics.Batch, plexigraf.PluginError]

func finish(in *plexigraf_metrics_metrics.Batch, out *plexigraf.Batch, err error) batchResult {
	if err != nil {
		in.Drop()
		return witTypes.Err[*plexigraf_metrics_metrics.Batch](plexigraf.ToPluginError(err, plexigraf.KindTransient))
	}
	if out == nil {
		out = plexigraf.NewBatch()
	}
	// A processor that built a new batch is done with its input.
	if in != nil && out.Handle() != in {
		in.Drop()
	}
	return witTypes.Ok[*plexigraf_metrics_metrics.Batch, plexigraf.PluginError](out.Handle())
}

func Process(input *plexigraf_metrics_metrics.Batch) batchResult {
	out, err := rt.Plugin().(plexigraf.Processor).Process(plexigraf.FromHandle(input))
	return finish(input, out, err)
}

func Flush(now uint64) batchResult {
	f, ok := rt.Plugin().(plexigraf.Flusher)
	if !ok {
		return finish(nil, nil, nil)
	}
	out, err := f.Flush(plexigraf.Time(now))
	return finish(nil, out, err)
}
