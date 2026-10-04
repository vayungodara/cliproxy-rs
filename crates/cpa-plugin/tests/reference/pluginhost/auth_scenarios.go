package main

// host.auth.* and host.affinity.lookup, first without an auth manager (Go's host
// before SetAuthManager: disk listing, the manager unavailable), then with a real
// coreauth.Manager holding registered credentials. The recorder drives each callback
// directly; the upstream example host-callback-auth-files renders them as a page.

import (
	"context"
	"os"
	"path/filepath"
	"strings"

	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

const authConfig = `auth-dir: AUTHDIR
plugins:
  enabled: true
  dir: PLUGINDIR
  configs:
    recorder-a:
      enabled: true
      record: RECORDDIR
      label: a
      caps: management_api
    host-callback-auth-files:
      enabled: true
`

var authFiles = map[string]string{
	"a.json":     `{"type":"demo","email":" a@example.com ","project_id":"p1","priority":"3","note":" n ","base_url":"https://b.example","websockets":"true","access_token":"tok"}`,
	"B.json":     `{"type":"other","disabled":true,"priority":2.9}`,
	"c.txt":      `{}`,
	"bad.json":   `not json`,
	"empty.json": ` `,
}

const hostAuthCalls = `{"calls":[
 {"method":"host.auth.list","request":{}},
 {"method":"host.auth.list","request":[]},
 {"method":"host.auth.get","request":{"auth_index":" "}},
 {"method":"host.auth.get","request":{"auth_index":"idx"}},
 {"method":"host.auth.get","request":{"auth_index":7}},
 {"method":"host.auth.get_runtime","request":{"auth_index":"idx"}},
 {"method":"host.auth.save","request":{"name":"../x.json","json":{}}},
 {"method":"host.auth.save","request":{"name":"a\\b.json","json":{}}},
 {"method":"host.auth.save","request":{"name":"x.txt","json":{}}},
 {"method":"host.auth.save","request":{"name":" ","json":{}}},
 {"method":"host.auth.save","request":{"name":"x.json"}},
 {"method":"host.auth.save","request":{"name":"x.json","json":[1]}},
 {"method":"host.auth.save","request":{"name":"x.json","json":{"type":"demo","weight":1.5}}},
 {"method":"host.auth.save","request":{"name":"x.json","json":{"type":"demo","weight":2000000}}},
 {"method":"host.auth.save","request":{"name":" Saved.JSON ","json":{"type":"demo","email":" s@example.com ","disabled":"true","priority":"7","base-url":"https://legacy"}}},
 {"method":"host.auth.list","request":null},
 {"method":"host.affinity.lookup","request":{"provider":" ","model":"m","session_id":"s"}},
 {"method":"host.affinity.lookup","request":{"provider":"p","model":"","session_id":"s"}},
 {"method":"host.affinity.lookup","request":{"provider":"p","model":"m"}},
 {"method":"host.affinity.lookup","request":{"provider":"p","model":"m","session_id":"s"}}
]}`

const hostAuthManagerCalls = `{"calls":[
 {"method":"host.auth.list","request":{}},
 {"method":"host.auth.get","request":{"auth_index":"idx-a"}},
 {"method":"host.auth.get","request":{"auth_index":"idx-rt"}},
 {"method":"host.auth.get","request":{"auth_index":"idx-moved"}},
 {"method":"host.auth.get","request":{"auth_index":"idx-bad"}},
 {"method":"host.auth.get","request":{"auth_index":"idx-empty"}},
 {"method":"host.auth.get","request":{"auth_index":"nope"}},
 {"method":"host.auth.get_runtime","request":{"auth_index":"idx-rt"}},
 {"method":"host.auth.get_runtime","request":{"auth_index":"idx-gone"}},
 {"method":"host.auth.get_runtime","request":{"auth_index":" idx-b "}},
 {"method":"host.affinity.lookup","request":{"provider":"demo","model":"m","session_id":"s"}}
]}`

// Saving registers a credential whose index Go derives from its absolute path, so
// saves come after every listing.
const hostAuthManagerSaves = `{"calls":[
 {"method":"host.auth.save","request":{"name":"new.json","json":{"type":"demo","email":"n@example.com"}}},
 {"method":"host.auth.save","request":{"name":"a.json","json":{"type":"demo","email":"changed@example.com"}}},
 {"method":"host.auth.get","request":{"auth_index":"idx-a"}}
]}`

// authSpec is one registered credential. AccountType and Account are what Go's
// Auth.AccountInfo derives; the Rust replay's manager reports them as given.
type authSpec struct {
	ID            string            `json:"id"`
	Index         string            `json:"index"`
	Provider      string            `json:"provider"`
	FileName      string            `json:"file_name,omitempty"`
	Label         string            `json:"label,omitempty"`
	Status        string            `json:"status"`
	StatusMessage string            `json:"status_message,omitempty"`
	Disabled      bool              `json:"disabled,omitempty"`
	Unavailable   bool              `json:"unavailable,omitempty"`
	Attributes    map[string]string `json:"attributes,omitempty"`
	Metadata      map[string]any    `json:"metadata,omitempty"`
	AccountType   string            `json:"account_type,omitempty"`
	Account       string            `json:"account,omitempty"`
}

var authSpecs = []authSpec{
	{ID: "a.json", Index: "idx-a", Provider: "demo", FileName: "a.json", Label: "a@example.com", Status: "active",
		Attributes: map[string]string{"path": "AUTHDIR/a.json", "source": "AUTHDIR/a.json", "priority": "9", "note": " attr note ", "websockets": "false"},
		Metadata:   map[string]any{"type": "demo", "email": " a@example.com ", "project_id": "p1", "access_token": "tok"},
		AccountType: "oauth", Account: "a@example.com"},
	{ID: "rt-1", Index: "idx-rt", Provider: " demo ", Label: "runtime", Status: "error", StatusMessage: "boom", Unavailable: true,
		Attributes:  map[string]string{"runtime_only": "true", "api_key": "k", "email": "attr@example.com", "project_id": " ap ", "base_url": " https://attr "},
		Metadata:    map[string]any{"websockets": "yes", "priority": " 4 "},
		AccountType: "api_key", Account: "k"},
	{ID: "gone.json", Index: "idx-gone", Provider: "demo", FileName: "gone.json", Status: "disabled", Disabled: true,
		Attributes: map[string]string{"path": "AUTHDIR/gone.json"}},
	{ID: "moved.json", Index: "idx-moved", Provider: "demo", Status: "active",
		Attributes: map[string]string{"path": "AUTHDIR/moved.json"}},
	{ID: "B.json", Index: "idx-b", Provider: "other", FileName: "B.json", Status: "active",
		Attributes: map[string]string{"path": "AUTHDIR/B.json"},
		Metadata:   map[string]any{"type": "other", "priority": 2.9, "note": " meta note "}},
	{ID: "bad.json", Index: "idx-bad", Provider: "demo", FileName: "bad.json", Status: "active",
		Attributes: map[string]string{"path": "AUTHDIR/bad.json"}},
	{ID: "empty.json", Index: "idx-empty", Provider: "demo", FileName: "empty.json", Status: "active",
		Attributes: map[string]string{"path": "AUTHDIR/empty.json"}},
	{ID: "rt-off", Index: "idx-rt-off", Provider: "demo", Status: "active", Disabled: true,
		Attributes: map[string]string{"runtime_only": "TRUE"}},
}

// authFiles writes auth files, plus a directory named like one (listing skips it).
func (r *runner) authFiles(files map[string]string) {
	for name, content := range files {
		check(os.WriteFile(filepath.Join(r.authDir, name), []byte(content), 0o600))
	}
	check(os.MkdirAll(filepath.Join(r.authDir, "dir.json"), 0o755))
	r.add("auth_files", files, nil)
}

// authManager gives the host a manager holding specs (nil: none).
func (r *runner) authManager(specs []authSpec) {
	if specs == nil {
		r.host.SetAuthManager(nil)
		r.add("auth_manager", specs, nil)
		return
	}
	manager := coreauth.NewManager(nil, nil, nil)
	for _, spec := range specs {
		attributes := map[string]string{}
		for k, v := range spec.Attributes {
			attributes[k] = strings.ReplaceAll(v, "AUTHDIR", r.authDir)
		}
		metadata := map[string]any{}
		for k, v := range spec.Metadata {
			metadata[k] = v
		}
		_, err := manager.Register(context.Background(), &coreauth.Auth{
			ID: spec.ID, Index: spec.Index, Provider: spec.Provider, FileName: spec.FileName, Label: spec.Label,
			Status: coreauth.Status(spec.Status), StatusMessage: spec.StatusMessage,
			Disabled: spec.Disabled, Unavailable: spec.Unavailable,
			Attributes: attributes, Metadata: metadata,
		})
		check(err)
	}
	r.host.SetAuthManager(manager)
	r.add("auth_manager", specs, nil)
}

func authScenarios(r *runner) {
	r.clear()
	r.files(map[string]string{"recorder-a.so": "recorder", "host-callback-auth-files.so": "host-callback-auth-files"})
	r.authFiles(authFiles)
	r.apply(authConfig)
	r.registerManagement()
	r.serve("management", httpArgs{Method: "POST", Target: "/v0/management/rec/a/calls", Body: hostAuthCalls})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/host-callback-auth-files/status?op=list"})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/host-callback-auth-files/status?op=get&auth_index=idx-a"})

	r.authManager(authSpecs)
	r.serve("management", httpArgs{Method: "POST", Target: "/v0/management/rec/a/calls", Body: hostAuthManagerCalls})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/host-callback-auth-files/status?op=list"})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/host-callback-auth-files/status?op=get&auth_index=idx-a"})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/host-callback-auth-files/status?op=runtime&auth_index=idx-rt"})
	r.serve("management", httpArgs{Method: "POST", Target: "/v0/management/rec/a/calls", Body: hostAuthManagerSaves})
	r.serve("resource", httpArgs{Method: "GET", Target: "/v0/resource/plugins/host-callback-auth-files/status?op=save&name=page.json&json=%7B%22type%22%3A%22demo%22%7D"})
	r.records()
	r.authManager(nil)
	r.shutdown()
	r.records()
}
