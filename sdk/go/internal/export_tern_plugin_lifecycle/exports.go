// Package export_tern_plugin_lifecycle implements the tern:plugin/lifecycle
// exports shared by every world.
package export_tern_plugin_lifecycle

import "github.com/tern-dev/tern-sdk-go/internal/rt"

func ValidateConfig(cfg string) rt.Result { return rt.ValidateConfig(cfg) }
func Start(cfg string) rt.Result          { return rt.Start(cfg) }
func Shutdown()                           { rt.Shutdown() }
