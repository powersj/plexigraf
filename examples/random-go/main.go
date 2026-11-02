package main

import (
	"errors"
	"math/rand/v2"

	plexigraf "github.com/powersj/plexigraf/sdk/go"
	"github.com/powersj/plexigraf/sdk/go/scraper"
)

// The SDK decodes the component's TOML settings into this struct. The
// values passed to Register are the defaults.
type Random struct {
	Prefix string `toml:"prefix"`
	Min    int64  `toml:"min"`
	Max    int64  `toml:"max"`
}

// Validate runs on `plexigraf config check` and before Start.
func (r *Random) Validate() error {
	if r.Min >= r.Max {
		return plexigraf.ConfigError(errors.New("min must be less than max"))
	}
	return nil
}

func (r *Random) Scrape(now plexigraf.Time) (*plexigraf.Batch, error) {
	attrs := plexigraf.Attrs("source", "plexigraf")
	b := plexigraf.NewBatch()
	b.Push(plexigraf.Gauge(r.Prefix+".int", "1").Point(now, r.Min+rand.Int64N(r.Max-r.Min), attrs))
	b.Push(plexigraf.Gauge(r.Prefix+".float", "1").Point(now, rand.Float64(), attrs))
	return b, nil
}

// Importing the scraper package selects the scraper world.
func init() { scraper.Register(&Random{Prefix: "random", Max: 100}) }

func main() {}
