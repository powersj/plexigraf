// Package scraper registers a Tern scraper plugin. Importing it selects the
// tern:plugin/scraper world, so build with -w tern:plugin/scraper.
package scraper

import (
	tern "github.com/tern-dev/tern-sdk-go"
	"github.com/tern-dev/tern-sdk-go/internal/rt"

	// The generated exports for this world.
	_ "github.com/tern-dev/tern-sdk-go/internal/scraper/wit_exports"
)

// Register makes p the plugin, usually from init. p must be a pointer to a
// struct; the SDK decodes the component's TOML settings into it, so the
// field values it already has are the defaults.
func Register(p tern.Scraper) { rt.Register(p) }
