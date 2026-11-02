// Package processor registers a Tern processor plugin. Importing it selects the
// tern:plugin/processor world, so build with -w tern:plugin/processor.
package processor

import (
	tern "github.com/tern-dev/tern-sdk-go"
	"github.com/tern-dev/tern-sdk-go/internal/rt"

	// The generated exports for this world.
	_ "github.com/tern-dev/tern-sdk-go/internal/processor/wit_exports"
)

// Register makes p the plugin, usually from init. p must be a pointer to a
// struct; the SDK decodes the component's TOML settings into it, so the
// field values it already has are the defaults.
func Register(p tern.Processor) { rt.Register(p) }
