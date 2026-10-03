package cmd

// Overlaid into internal/cmd at 6fecc6e (see README.md). Records the exact text and
// JSON that the real discover command prints for fixed browse results.

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/discovery"
)

type zzBrowser struct {
	result []discovery.DiscoveredService
	err    error
}

func (f *zzBrowser) Browse(context.Context, string, string) ([]discovery.DiscoveredService, error) {
	return f.result, f.err
}
func (f *zzBrowser) BrowseWithFallback(context.Context) ([]discovery.DiscoveredService, error) {
	return f.result, f.err
}
func (f *zzBrowser) BrowseWithFallbackServiceType(context.Context, string) ([]discovery.DiscoveredService, error) {
	return f.result, f.err
}

func zzIPs(ss ...string) []net.IP {
	r := []net.IP{}
	for _, s := range ss {
		r = append(r, net.ParseIP(s))
	}
	return r
}

func TestZZCmdFixture(t *testing.T) {
	out := map[string]any{}
	full := discovery.DiscoveredService{
		InstanceName: "CPA-8F3B", ServiceType: "_ai-gateway._tcp", Domain: "local.", Host: "box.local.", Port: 8317,
		IPv4: zzIPs("192.168.1.5", "10.0.0.2"), IPv6: zzIPs("fe80::1", "2001:db8::5"),
		Protocols: []string{"chat-completions", "responses"}, Features: []string{"chat", "evil\x1b[31m"},
		Product: "cliproxyapi", AuthRequired: true, AuthMethods: []string{"api_key"},
		Endpoints: map[string]string{"openai": "/v1", "anthropic": "/v1", "gemini": "/v1beta"},
		NodeRole:  "standalone", Version: "1",
		RawTXT: map[string]string{"tls": "1", "product": "cliproxyapi", "z": "<&>"},
	}
	bare := discovery.DiscoveredService{InstanceName: "bare\u202e", Port: 1234, IPv4: zzIPs(), IPv6: zzIPs("fe80::2"), Host: "gw.local.", Endpoints: map[string]string{}}
	noAuth := discovery.DiscoveredService{InstanceName: "open", Port: 80, IPv4: zzIPs(), IPv6: zzIPs(), Host: "bad host.local.", AuthRequired: true}
	mapped := discovery.DiscoveredService{InstanceName: "mapped", Port: 81, IPv4: zzIPs("127.0.0.1"), IPv6: zzIPs("::ffff:192.0.2.1", "::1")}

	type run struct {
		Name    string `json:"name"`
		Timeout int64  `json:"timeout_ms"`
		JSON    bool   `json:"json"`
		Type    string `json:"service_type"`
		Code    int    `json:"code"`
		Stdout  string `json:"stdout"`
		Stderr  string `json:"stderr"`
	}
	var runs []run
	cases := []struct {
		name     string
		timeout  time.Duration
		json     bool
		st       string
		result   []discovery.DiscoveredService
		err      error
		factory  error
	}{
		{"text-full", 2 * time.Second, false, "", []discovery.DiscoveredService{full, bare, noAuth, mapped}, nil, nil},
		{"json-full", 0, true, " _x._tcp ", []discovery.DiscoveredService{full, bare, noAuth, mapped}, nil, nil},
		{"text-empty", 90 * time.Second, false, "", nil, nil, nil},
		{"json-empty", 0, true, "", nil, nil, nil},
		{"text-browse-error", time.Second, false, "", nil, errors.New("discovery: browse query failed: boom"), nil},
		{"json-browse-error", time.Second, true, "", nil, errors.New("discovery: browse query failed: <boom>"), nil},
		{"text-factory-error", -time.Second, false, "", nil, nil, errors.New("no qualified physical interfaces found for LAN discovery")},
		{"json-factory-error", 59 * time.Second, true, "", nil, nil, errors.New("no qualified physical interfaces found for LAN discovery")},
		{"text-ms", 1500 * time.Millisecond, false, "", nil, nil, nil},
	}
	for _, c := range cases {
		var stdout, stderr bytes.Buffer
		code := runDiscoverWithOptions(DiscoverOptions{Timeout: c.timeout, JSONOutput: c.json, ServiceType: c.st}, &stdout, &stderr, func() (discovery.Browser, error) {
			if c.factory != nil {
				return nil, c.factory
			}
			return &zzBrowser{result: c.result, err: c.err}, nil
		})
		runs = append(runs, run{c.name, c.timeout.Milliseconds(), c.json, c.st, code, stdout.String(), stderr.String()})
	}
	out["runs"] = runs

	var sanitize []map[string]string
	for _, s := range []string{"plain", "tab\there", "esc\x1b[31m", "bidi\u202e\u2066x", "zw\u200b\u200f\u00ad\ufeff", "ls\u2028ps\u2029", "\u00e9\u4e2d", "del\x7f", "nbsp\u00a0"} {
		sanitize = append(sanitize, map[string]string{"in": s, "out": sanitizeTerminal(s), "host": sanitizeDisplayHost(s)})
	}
	for _, s := range []string{" gw.local. ", "localhost", "LOCALHOST.", "a..b", "-a", "a-", ".a", "ok-1.lan", "bad_host", ""} {
		sanitize = append(sanitize, map[string]string{"in": s, "out": sanitizeTerminal(s), "host": sanitizeDisplayHost(s)})
	}
	out["sanitize"] = sanitize

	var interfaceLists []map[string]any
	for _, in := range [][]string{{"eth0,en0", " eth0 ", ""}, {",,"}, {"a", "b,a"}} {
		interfaceLists = append(interfaceLists, map[string]any{"in": in, "out": ParseInterfaceList(in...)})
	}
	out["interface_lists"] = interfaceLists

	dir := t.TempDir()
	var filters []map[string]any
	for _, text := range []string{
		"discovery:\n  interfaces:\n    include: [\"eth0,en0\", \" wl* \"]\n    exclude: [docker0]\n",
		"server:\n  discovery:\n    interfaces:\n      include: [eth0]\n",
		"discovery: [broken\n",
		"discovery:\n  interfaces:\n    include: eth0\n",
		"",
	} {
		path := filepath.Join(dir, "config.yaml")
		if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
			t.Fatal(err)
		}
		include, exclude := LoadDiscoveryScanFilters(path)
		filters = append(filters, map[string]any{"yaml": text, "include": include, "exclude": exclude})
	}
	out["config_filters"] = filters

	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
