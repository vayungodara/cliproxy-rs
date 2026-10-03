package discovery

// Overlaid into internal/discovery at 6fecc6e (see README.md). Records outputs of the
// real, partly unexported, discovery helpers. No sockets are opened.

import (
	"encoding/json"
	"net"
	"os"
	"strings"
	"testing"

	"github.com/libp2p/zeroconf/v2"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
)

func errText(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

func TestZZDiscoveryFixture(t *testing.T) {
	out := map[string]any{}

	type txtCase struct {
		Opts    TXTOptions `json:"opts"`
		Records []string   `json:"records"`
	}
	long := strings.Repeat("x", 300)
	many := make([]string, 0, 40)
	for i := 0; i < 40; i++ {
		many = append(many, "feature-"+strings.Repeat("f", i%7))
	}
	withID := DefaultTXTOptions()
	withID.InstanceID = "8F3B"
	tls := withID
	tls.TLS = true
	tls.AuthRequired = false
	tls.AdvertiseManagement = true
	dirty := withID
	dirty.Version = " 2\x00\u00e9 "
	dirty.Product = "\t"
	dirty.AuthMethods = []string{"api_key", "bearer"}
	longCase := withID
	longCase.APIPathOpenAI = "/" + long
	manyCase := withID
	manyCase.Features = many
	manyCase.Protocols = many[:20]
	empty := TXTOptions{}
	var txt []txtCase
	for _, o := range []TXTOptions{DefaultTXTOptions(), withID, tls, dirty, longCase, manyCase, empty} {
		txt = append(txt, txtCase{Opts: o, Records: BuildTXTRecords(o)})
	}
	out["txt"] = txt

	type parseCase struct {
		In  []string          `json:"in"`
		Out map[string]string `json:"out"`
	}
	var parsed []parseCase
	for _, in := range [][]string{
		{"version=1", "Product=cliproxyapi", "a=b=c", "flag", "=x", " k =v", "", "dup=1", "DUP=2"},
		{"tls=1", "api_openai=/v1"},
	} {
		parsed = append(parsed, parseCase{In: in, Out: ParseTXTRecords(in)})
	}
	out["parse_txt"] = parsed

	type nameCase struct {
		Name string `json:"name"`
		ID   string `json:"id"`
		Out  string `json:"out"`
	}
	var names []nameCase
	for _, c := range [][2]string{
		{"", "8f3b"}, {"CPA-8F3B", "8F3B"}, {"cpa-8f3b", "8F3B"}, {"office", "8F3B"}, {"office-8f3b", "8F3B"},
		{"office-8F3B", "8F3B"}, {"  spaced  ", "ABCD"}, {"bad\x01ctl\x7f", "ABCD"}, {strings.Repeat("n", 80), "ABCD"},
		{strings.Repeat("\u00e9", 40), "ABCD"}, {"office", "zz"}, {"office", ""}, {"-8F3B", "8F3B"},
		{"My Server.home", "0A0B"}, {strings.Repeat("\u4e2d", 20) + "-0A0B", "0A0B"},
	} {
		names = append(names, nameCase{Name: c[0], ID: c[1], Out: FormatInstanceName(c[0], c[1])})
	}
	out["instance_names"] = names

	type strCase struct {
		In  string `json:"in"`
		Out string `json:"out"`
	}
	var subtypes []strCase
	for _, s := range []string{"_responses", "responses", " _messages ", "", "_", "_-bad", "_bad-", "_a.b", "_x._sub", "_under_score", "_" + strings.Repeat("a", 62), "_" + strings.Repeat("a", 63), "_Mixed-9"} {
		subtypes = append(subtypes, strCase{In: s, Out: sanitizeSubtype(s)})
	}
	out["subtypes"] = subtypes

	var serviceTypes []strCase
	for _, s := range []string{"_ai-gateway._tcp", "", "  ", "_ai-gateway._udp", "ai-gateway._tcp", "_._tcp", "_" + strings.Repeat("a", 16) + "._tcp", "_a_b._tcp", "_-ab._tcp", "_ab-._tcp", "_" + strings.Repeat("a", 60) + "._tcp", "_ok-1._tcp", " _ai-gateway._tcp "} {
		serviceTypes = append(serviceTypes, strCase{In: s, Out: errText(validateServiceType(s))})
	}
	out["service_types"] = serviceTypes

	var instanceNames []strCase
	for _, s := range []string{"plain", " pad ", "a\x00b\x1fc\x7fd", strings.Repeat("\u00e9", 40)} {
		instanceNames = append(instanceNames, strCase{In: s, Out: sanitizeInstanceName(s)})
	}
	out["sanitize_instance_name"] = instanceNames

	type ifaceCase struct {
		Name      string   `json:"name"`
		Include   []string `json:"include"`
		Exclude   []string `json:"exclude"`
		Virtual   bool     `json:"virtual"`
		Physical  bool     `json:"physical"`
		InInclude bool     `json:"in_include"`
		InExclude bool     `json:"in_exclude"`
	}
	var ifaces []ifaceCase
	for _, c := range []struct {
		name             string
		include, exclude []string
	}{
		{"eth0", nil, nil}, {"en0", nil, nil}, {"wlan0", []string{"WLAN*"}, nil}, {"docker0", []string{"docker0"}, nil},
		{"br-1234", nil, []string{"br-*"}}, {"tailscale0", nil, nil}, {"bond0", nil, nil}, {"enp3s0", []string{" ", "en*"}, []string{"enp3s0"}},
		{"lo", nil, nil}, {"wi-fi", nil, nil}, {"ethernet 2", nil, nil}, {"vethabc", nil, nil}, {"awdl0", nil, nil}, {"utun3", nil, nil},
		{"ix0", nil, nil}, {"re0", nil, nil}, {"em1", nil, nil}, {"igb0", nil, nil}, {"wg0", nil, nil}, {"tap0", nil, nil}, {"x*", []string{"*"}, nil},
	} {
		ifaces = append(ifaces, ifaceCase{
			Name: c.name, Include: c.include, Exclude: c.exclude,
			Virtual: isVirtualOrTunnel(c.name), Physical: isLikelyPhysicalLAN(c.name),
			InInclude: matchesAny(c.name, c.include), InExclude: matchesAny(c.name, c.exclude),
		})
	}
	out["interfaces"] = ifaces

	var lists []struct {
		In  string   `json:"in"`
		Out []string `json:"out"`
	}
	for _, s := range []string{"a,b,a", " x , ,y,", "", strings.Repeat("p,", 40), strings.Repeat("z", 65) + ",ok"} {
		lists = append(lists, struct {
			In  string   `json:"in"`
			Out []string `json:"out"`
		}{s, parseTXTList(s)})
	}
	out["txt_lists"] = lists

	var paths []strCase
	for _, s := range []string{"/v1", " /v1beta ", "v1", "//evil", "/a\\b", "/http://x", "/a/../b", "/ok\x7f", "/ok path", ""} {
		paths = append(paths, strCase{In: s, Out: sanitizeEndpointPath(s)})
	}
	out["endpoint_paths"] = paths

	type entryCase struct {
		Instance string            `json:"instance"`
		Service  string            `json:"service"`
		Domain   string            `json:"domain"`
		Host     string            `json:"host"`
		Port     int               `json:"port"`
		IPv4     []string          `json:"ipv4"`
		IPv6     []string          `json:"ipv6"`
		Text     []string          `json:"text"`
		Within   bool              `json:"within_limits"`
		Out      DiscoveredService `json:"out"`
	}
	ips := func(ss []string) []net.IP {
		var r []net.IP
		for _, s := range ss {
			r = append(r, net.ParseIP(s))
		}
		return r
	}
	var entries []entryCase
	for _, c := range []entryCase{
		{Instance: "CPA-8F3B", Service: "_ai-gateway._tcp", Domain: "local.", Host: "box.local.", Port: 8317,
			IPv4: []string{"192.168.1.5", "127.0.0.1", "0.0.0.0"}, IPv6: []string{"fe80::1", "::1", "2001:db8::5"},
			Text: BuildTXTRecords(withID)},
		{Instance: "other", Service: "_ai-gateway._tcp", Domain: "local.", Host: "x.local.", Port: 70000,
			IPv4: []string{"10.0.0.9"}, Text: []string{"auth_required=TRUE", "api_openai=//evil", "api_gemini=/g", "protocols=a,,a,b", "product=other"}},
		{Instance: "big", Service: "_ai-gateway._tcp", Domain: "local.", Port: 1, Text: []string{strings.Repeat("k", 256)}},
	} {
		e := &zeroconf.ServiceEntry{
			ServiceRecord: zeroconf.ServiceRecord{Instance: c.Instance, Service: c.Service, Domain: c.Domain},
			HostName:      c.Host, Port: c.Port, AddrIPv4: ips(c.IPv4), AddrIPv6: ips(c.IPv6), Text: c.Text,
		}
		c.Within = browseEntryWithinLimits(e)
		c.Out = entryToDiscovered(e)
		entries = append(entries, c)
	}
	// Snapshot before merging: the merge below mutates maps shared with entries[0].
	entriesJSON, errEntries := json.Marshal(entries)
	if errEntries != nil {
		t.Fatal(errEntries)
	}
	out["entries"] = json.RawMessage(entriesJSON)

	a := entries[0].Out
	b := entries[1].Out
	b.InstanceName = a.InstanceName
	b.Host = ""
	b.RawTXT["auth_required"] = "false"
	mergeDiscoveredService(&a, b)
	out["merged"] = a

	// BuildServiceSpec errors that do not depend on host interfaces.
	var specErrors []strCase
	for _, yamlText := range []string{
		"discovery:\n  enabled: true\n  service-type: _bad\n",
		"discovery:\n  enabled: true\n  interfaces:\n    include: [definitely-missing0]\n",
	} {
		cfg, errParse := config.ParseConfigBytes([]byte(yamlText))
		if errParse != nil {
			t.Fatal(errParse)
		}
		_, errSpec := BuildServiceSpec(cfg, 8317, false)
		specErrors = append(specErrors, strCase{In: yamlText, Out: errText(errSpec)})
	}
	cfg, _ := config.ParseConfigBytes([]byte("discovery:\n  enabled: true\n"))
	_, errPort := BuildServiceSpec(cfg, 0, false)
	specErrors = append(specErrors, strCase{In: "port 0", Out: errText(errPort)})
	out["spec_errors"] = specErrors

	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
