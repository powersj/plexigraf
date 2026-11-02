// Package export_plexigraf_plugin_lifecycle implements the plexigraf:plugin/lifecycle
// exports shared by every world.
package export_plexigraf_plugin_lifecycle

import "github.com/powersj/plexigraf/sdk/go/internal/rt"

func ValidateConfig(cfg string) rt.Result { return rt.ValidateConfig(cfg) }
func Start(cfg string) rt.Result          { return rt.Start(cfg) }
func Shutdown()                           { rt.Shutdown() }
