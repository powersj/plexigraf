// Package scraper registers a Plexigraf scraper plugin. Importing it selects the
// plexigraf:plugin/scraper world, so build with -w plexigraf:plugin/scraper.
package scraper

import (
	plexigraf "github.com/powersj/plexigraf/sdk/go"
	"github.com/powersj/plexigraf/sdk/go/internal/rt"

	// The generated exports for this world.
	_ "github.com/powersj/plexigraf/sdk/go/internal/scraper/wit_exports"
)

// Register makes p the plugin, usually from init. p must be a pointer to a
// struct; the SDK decodes the component's TOML settings into it, so the
// field values it already has are the defaults.
func Register(p plexigraf.Scraper) { rt.Register(p) }
