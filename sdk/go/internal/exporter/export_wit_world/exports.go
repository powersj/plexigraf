// Package export_wit_world implements the exporter world's exports, which the
// generated wit_exports package calls.
package export_wit_world

import (
	witTypes "go.bytecodealliance.org/pkg/wit/types"

	plexigraf "github.com/powersj/plexigraf/sdk/go"
	"github.com/powersj/plexigraf/sdk/go/internal/imports/plexigraf_metrics_metrics"
	"github.com/powersj/plexigraf/sdk/go/internal/rt"
)

func Export(b *plexigraf_metrics_metrics.Batch) witTypes.Result[plexigraf.Outcome, plexigraf.PluginError] {
	defer b.Drop()
	out, err := rt.Plugin().(plexigraf.Exporter).Export(plexigraf.FromHandle(b))
	if err != nil {
		return witTypes.Err[plexigraf.Outcome](plexigraf.ToPluginError(err, plexigraf.KindTransient))
	}
	return witTypes.Ok[plexigraf.Outcome, plexigraf.PluginError](out)
}
