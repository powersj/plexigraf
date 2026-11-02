// Package export_wit_world implements the exporter world's exports, which the
// generated wit_exports package calls.
package export_wit_world

import (
	witTypes "go.bytecodealliance.org/pkg/wit/types"

	tern "github.com/tern-dev/tern-sdk-go"
	"github.com/tern-dev/tern-sdk-go/internal/imports/tern_metrics_metrics"
	"github.com/tern-dev/tern-sdk-go/internal/rt"
)

func Export(b *tern_metrics_metrics.Batch) witTypes.Result[tern.Outcome, tern.PluginError] {
	defer b.Drop()
	out, err := rt.Plugin().(tern.Exporter).Export(tern.FromHandle(b))
	if err != nil {
		return witTypes.Err[tern.Outcome](tern.ToPluginError(err, tern.KindTransient))
	}
	return witTypes.Ok[tern.Outcome, tern.PluginError](out)
}
