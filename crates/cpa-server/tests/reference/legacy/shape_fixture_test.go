package config

// Overlaid into internal/config at 6fecc6e (see README.md). Dumps what the v0
// management reads need to reproduce Go's json.Marshal of runtime config values: the
// JSON layout of Config (field order, names, omitempty, kinds, YAML names) and the
// legacy-to-v8 document path table.

import (
	"encoding/json"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"
)

type zzShape struct {
	JSON   string     `json:"json,omitempty"`
	YAML   string     `json:"yaml,omitempty"`
	Omit   bool       `json:"omit,omitempty"`
	Kind   string     `json:"kind"`
	Ptr    bool       `json:"ptr,omitempty"`
	Fields []*zzShape `json:"fields,omitempty"`
	Elem   *zzShape   `json:"elem,omitempty"`
}

var zzDuration = reflect.TypeOf(time.Duration(0))

func zzTag(tag string) (name string, omit bool) {
	parts := strings.Split(tag, ",")
	for _, p := range parts[1:] {
		if p == "omitempty" {
			omit = true
		}
	}
	return parts[0], omit
}

func zzWalk(t reflect.Type, seen map[reflect.Type]bool) *zzShape {
	s := &zzShape{}
	for t.Kind() == reflect.Ptr {
		s.Ptr = true
		t = t.Elem()
	}
	if t == zzDuration {
		s.Kind = "duration"
		return s
	}
	if _, ok := reflect.New(t).Interface().(json.Marshaler); ok {
		s.Kind = "marshaler:" + t.String()
		return s
	}
	switch t.Kind() {
	case reflect.Bool:
		s.Kind = "bool"
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		s.Kind = "int"
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		s.Kind = "uint"
	case reflect.Float32, reflect.Float64:
		s.Kind = "float"
	case reflect.String:
		s.Kind = "string"
	case reflect.Interface:
		s.Kind = "any"
	case reflect.Slice, reflect.Array:
		s.Kind = "slice"
		s.Elem = zzWalk(t.Elem(), seen)
	case reflect.Map:
		s.Kind = "map"
		s.Elem = zzWalk(t.Elem(), seen)
	case reflect.Struct:
		s.Kind = "struct"
		if seen[t] {
			s.Kind = "recursive:" + t.String()
			return s
		}
		seen[t] = true
		s.Fields = zzFields(t, seen)
		delete(seen, t)
	default:
		s.Kind = "unsupported:" + t.Kind().String()
	}
	return s
}

// zzFields lists the JSON-visible fields in encoding/json order, inlining embedded
// structs without a JSON name as encoding/json does.
func zzFields(t reflect.Type, seen map[reflect.Type]bool) []*zzShape {
	var out []*zzShape
	for i := 0; i < t.NumField(); i++ {
		f := t.Field(i)
		jsonName, omit := zzTag(f.Tag.Get("json"))
		yamlName, _ := zzTag(f.Tag.Get("yaml"))
		if jsonName == "-" || (!f.IsExported() && !f.Anonymous) {
			continue
		}
		if f.Anonymous && jsonName == "" {
			et := f.Type
			for et.Kind() == reflect.Ptr {
				et = et.Elem()
			}
			if et.Kind() == reflect.Struct {
				out = append(out, zzFields(et, seen)...)
				continue
			}
		}
		if jsonName == "" {
			jsonName = f.Name
		}
		s := zzWalk(f.Type, seen)
		s.JSON = jsonName
		s.YAML = yamlName
		s.Omit = omit
		out = append(out, s)
	}
	return out
}

func TestZZShapeFixture(t *testing.T) {
	paths := map[string]string{}
	for _, p := range v8Paths {
		paths[p.old] = p.current
	}
	structPaths := map[string]string{}
	for _, p := range v8StructPaths {
		structPaths[p.old] = p.current
	}
	out := map[string]any{
		"config":       zzWalk(reflect.TypeOf(Config{}), map[reflect.Type]bool{}),
		"v8_paths":     paths,
		"struct_paths": structPaths,
	}
	data, err := json.MarshalIndent(out, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
