// Package export_wit_world implements the scraper world's exports, which the
// generated wit_exports package calls.
package export_wit_world

import (
	witTypes "go.bytecodealliance.org/pkg/wit/types"

	plexigraf "github.com/powersj/plexigraf/sdk/go"
	"github.com/powersj/plexigraf/sdk/go/internal/imports/plexigraf_metrics_metrics"
	"github.com/powersj/plexigraf/sdk/go/internal/rt"
)

type batchResult = witTypes.Result[*plexigraf_metrics_metrics.Batch, plexigraf.PluginError]

func Scrape(now uint64) batchResult {
	b, err := rt.Plugin().(plexigraf.Scraper).Scrape(plexigraf.Time(now))
	if err != nil {
		return witTypes.Err[*plexigraf_metrics_metrics.Batch](plexigraf.ToPluginError(err, plexigraf.KindTransient))
	}
	if b == nil {
		b = plexigraf.NewBatch()
	}
	return witTypes.Ok[*plexigraf_metrics_metrics.Batch, plexigraf.PluginError](b.Handle())
}
