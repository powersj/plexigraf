// Package export_wit_world implements the scraper world's exports, which the
// generated wit_exports package calls.
package export_wit_world

import (
	witTypes "go.bytecodealliance.org/pkg/wit/types"

	tern "github.com/tern-dev/tern-sdk-go"
	"github.com/tern-dev/tern-sdk-go/internal/imports/tern_metrics_metrics"
	"github.com/tern-dev/tern-sdk-go/internal/rt"
)

type batchResult = witTypes.Result[*tern_metrics_metrics.Batch, tern.PluginError]

func Scrape(now uint64) batchResult {
	b, err := rt.Plugin().(tern.Scraper).Scrape(tern.Time(now))
	if err != nil {
		return witTypes.Err[*tern_metrics_metrics.Batch](tern.ToPluginError(err, tern.KindTransient))
	}
	if b == nil {
		b = tern.NewBatch()
	}
	return witTypes.Ok[*tern_metrics_metrics.Batch, tern.PluginError](b.Handle())
}
