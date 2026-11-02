// Package exporter registers a Tern exporter plugin. Importing it selects the
// tern:plugin/exporter world, so build with -w tern:plugin/exporter.
package exporter

import (
	tern "github.com/tern-dev/tern-sdk-go"
	"github.com/tern-dev/tern-sdk-go/internal/rt"

	// The generated exports for this world.
	_ "github.com/tern-dev/tern-sdk-go/internal/exporter/wit_exports"
)

// Register makes p the plugin, usually from init. p must be a pointer to a
// struct; the SDK decodes the component's TOML settings into it, so the
// field values it already has are the defaults.
func Register(p tern.Exporter) { rt.Register(p) }
