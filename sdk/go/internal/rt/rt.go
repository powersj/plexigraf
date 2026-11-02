// Package rt holds the registered plugin and the lifecycle glue shared by
// every world.
package rt

import (
	"errors"
	"fmt"
	"reflect"
	"strings"

	"github.com/BurntSushi/toml"
	witTypes "go.bytecodealliance.org/pkg/wit/types"

	tern "github.com/tern-dev/tern-sdk-go"
)

type Result = witTypes.Result[witTypes.Unit, tern.PluginError]

var plugin any

// Register stores the plugin. Its current field values are the defaults that
// config overrides.
func Register(p any) {
	v := reflect.ValueOf(p)
	if v.Kind() != reflect.Pointer || v.Elem().Kind() != reflect.Struct {
		panic(fmt.Sprintf("tern: Register needs a pointer to a struct, got %T", p))
	}
	plugin = p
}

// Plugin returns the registered plugin.
func Plugin() any { return plugin }

func decode(cfg string, into any) error {
	md, err := toml.Decode(cfg, into)
	if err != nil {
		return err
	}
	if extra := md.Undecoded(); len(extra) > 0 {
		keys := make([]string, len(extra))
		for i, k := range extra {
			keys[i] = k.String()
		}
		return fmt.Errorf("unknown setting(s): %s", strings.Join(keys, ", "))
	}
	return nil
}

func fail(err error) Result {
	return witTypes.Err[witTypes.Unit](tern.ToPluginError(err, tern.KindConfig))
}

// ValidateConfig decodes into a copy of the plugin, so checking settings
// never changes the running instance.
func ValidateConfig(cfg string) Result {
	v := reflect.ValueOf(plugin)
	c := reflect.New(v.Elem().Type())
	c.Elem().Set(v.Elem())
	if err := decode(cfg, c.Interface()); err != nil {
		return fail(err)
	}
	if val, ok := c.Interface().(tern.Validator); ok {
		if err := val.Validate(); err != nil {
			return fail(err)
		}
	}
	return witTypes.Ok[witTypes.Unit, tern.PluginError](witTypes.Unit{})
}

func Start(cfg string) Result {
	if plugin == nil {
		return fail(errors.New("no plugin registered; call Register from init"))
	}
	if err := decode(cfg, plugin); err != nil {
		return fail(err)
	}
	if val, ok := plugin.(tern.Validator); ok {
		if err := val.Validate(); err != nil {
			return fail(err)
		}
	}
	if s, ok := plugin.(tern.Starter); ok {
		if err := s.Start(); err != nil {
			return fail(err)
		}
	}
	return witTypes.Ok[witTypes.Unit, tern.PluginError](witTypes.Unit{})
}

func Shutdown() {
	if s, ok := plugin.(tern.Shutdowner); ok {
		s.Shutdown()
	}
}
