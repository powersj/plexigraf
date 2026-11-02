// Package export_wit_world implements the processor world's exports, which
// the generated wit_exports package calls.
package export_wit_world

import (
	witTypes "go.bytecodealliance.org/pkg/wit/types"

	tern "github.com/tern-dev/tern-sdk-go"
	"github.com/tern-dev/tern-sdk-go/internal/imports/tern_metrics_metrics"
	"github.com/tern-dev/tern-sdk-go/internal/rt"
)

type batchResult = witTypes.Result[*tern_metrics_metrics.Batch, tern.PluginError]

func finish(in *tern_metrics_metrics.Batch, out *tern.Batch, err error) batchResult {
	if err != nil {
		in.Drop()
		return witTypes.Err[*tern_metrics_metrics.Batch](tern.ToPluginError(err, tern.KindTransient))
	}
	if out == nil {
		out = tern.NewBatch()
	}
	// A processor that built a new batch is done with its input.
	if in != nil && out.Handle() != in {
		in.Drop()
	}
	return witTypes.Ok[*tern_metrics_metrics.Batch, tern.PluginError](out.Handle())
}

func Process(input *tern_metrics_metrics.Batch) batchResult {
	out, err := rt.Plugin().(tern.Processor).Process(tern.FromHandle(input))
	return finish(input, out, err)
}

func Flush(now uint64) batchResult {
	f, ok := rt.Plugin().(tern.Flusher)
	if !ok {
		return finish(nil, nil, nil)
	}
	out, err := f.Flush(tern.Time(now))
	return finish(nil, out, err)
}
