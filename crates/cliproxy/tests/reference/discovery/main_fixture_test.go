package main

// Overlaid into cmd/server at 6fecc6e (see README.md). Records argv pre-scan results
// and the management base URL fallback.

import (
	"encoding/json"
	"os"
	"testing"

	"github.com/joho/godotenv"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
)

func TestZZMainFixture(t *testing.T) {
	type argvCase struct {
		Args         []string `json:"args"`
		DiscoverJSON bool     `json:"discover_json"`
		Discover     bool     `json:"discover"`
		ConfigPath   string   `json:"config_path"`
	}
	var argv []argvCase
	for _, args := range [][]string{
		{"-discover-json"}, {"--discover-json=false"}, {"-discover-json=FALSE", "-discover"}, {"-discover-json=maybe"},
		{"-config", "-discover-json"}, {"-config=-discover-json", "-discover"}, {"-tui", "-discover-json"},
		{"x", "-discover-json"}, {"--", "-discover-json"}, {"-", "-discover-json"}, {"-config"},
		{"-password", "p", "--discover"}, {"-discover-timeout", "5", "-discover"}, {"--discover=1"}, {"-discover=t"},
		{"--config=a.yaml"}, {"-config", "b.yaml", "--config", "c.yaml"}, {},
	} {
		argv = append(argv, argvCase{
			Args: args, DiscoverJSON: argvEnablesBoolFlag(args, "discover-json"),
			Discover: argvEnablesBoolFlag(args, "discover"), ConfigPath: pluginBootstrapConfigPath(args, "default.yaml"),
		})
	}

	type urlCase struct {
		Flag   string `json:"flag"`
		Remote string `json:"remote"`
		Port   int    `json:"port"`
		Out    string `json:"out"`
	}
	var urls []urlCase
	for _, c := range []urlCase{{"", "", 0, ""}, {" https://x ", "http://y", 9, ""}, {"", " http://y ", 9, ""}, {"", "", 9, ""}, {"", "", -1, ""}} {
		cfg := &config.Config{}
		cfg.RemoteManagement.BaseURL = c.Remote
		cfg.Port = c.Port
		c.Out = resolveManagementBaseURL(c.Flag, cfg)
		urls = append(urls, c)
	}
	urls = append(urls, urlCase{Out: resolveManagementBaseURL("", nil)})

	type planCase struct {
		Local, Home                bool
		Models, CodexClient, Devin bool
	}
	var plans []planCase
	for _, c := range [][2]bool{{false, false}, {false, true}, {true, false}, {true, true}} {
		m, cc, d := modelCatalogUpdaterPlan(c[0], c[1])
		plans = append(plans, planCase{c[0], c[1], m, cc, d})
	}

	type dotenvCase struct {
		In  string            `json:"in"`
		Out map[string]string `json:"out"`
		Err string            `json:"err"`
	}
	var dotenv []dotenvCase
	for _, in := range []string{
		"A=1\nB=2\n", "export A=1", "exportA=1", "  A = spaced value  \n", "A=x # comment\nB=y#notcomment", "A=a #b #c",
		"# only comment\n\nA=1", "A='single $A \\n'", "A=\"double \\n \\r \\t \\\\ \\$A\"", "A=1\nB=${A}-$A-$(A)-\\$A-${A", "B=$lower $ ${}",
		"A=\"unterminated", "A=1\nB", "B", "Ü=1", "\u00e9=1", "A.B=1\nC_D=2", "A:yaml", "A=\"q\\\"uoted\"", "A=''", "A=\"\"\"x\"",
		"A=1\r\nB=2\r\n", "A=\"x\" trailing", "A=1\nA=2\nB=$A", "A=\"multi\nline\"", "A B=1", "A=\u00a0v\u00a0", "\u00a0A=1", "A=$A$A",
		"A='a\\'b'", "", "   \n\t", "A=\"$(B)\"\nB=2\nC=$(B)",
	} {
		out, err := godotenv.Unmarshal(in)
		errText := ""
		if err != nil {
			errText = err.Error()
		}
		dotenv = append(dotenv, dotenvCase{In: in, Out: out, Err: errText})
	}

	data, err := json.MarshalIndent(map[string]any{"argv": argv, "management_base_url": urls, "catalog_plans": plans, "dotenv": dotenv}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
