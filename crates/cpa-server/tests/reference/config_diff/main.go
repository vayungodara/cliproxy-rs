// Run inside pinned Go module with external networking denied. No real secrets.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/diff"
	"gopkg.in/yaml.v3"
)

func main() {
	root, err := os.MkdirTemp("", "cpa-diff-go-")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(root)
	load := func(text string) *config.Config {
		path := filepath.Join(root, "config.yaml")
		if err := os.WriteFile(path, []byte(text), 0600); err != nil {
			panic(err)
		}
		cfg, err := config.LoadConfig(path)
		if err != nil {
			panic(err)
		}
		cfg.AuthDir, err = util.ResolveAuthDir(cfg.AuthDir)
		if err != nil {
			panic(err)
		}
		return cfg
	}
	cases := []map[string]any{}
	pairs := [][2]string{
		{"", ""},
		{"", `client: {codex: {enable-apply-patch: true, optimize-multi-agent-v2: true}}
port: 9001
auth-dir: /tmp/new-auth
debug: true
pprof: {enable: true, addr: ' 127.0.0.1:9002 '}
logging-to-file: true
usage-statistics-enabled: true
redis-usage-queue-retention-seconds: 89
disable-cooling: true
save-cooldown-status: true
transient-error-cooldown-seconds: 12
disable-claude-cloak-mode: true
claude-code: {disable-cloaking-model-list: true}
disable-image-generation: true
gpt-image-2-base-model: ' custom '
request-log: true
logs-max-total-size-mb: 45
error-logs-max-files: 4
request-retry: 2
max-retry-credentials: 3
max-retry-interval: 7
proxy-url: http://fixture-user:fixture-pass@127.0.0.1:9000/private?key=fixture-secret
ws-auth: false
force-model-prefix: true
nonstream-keepalive-interval: 5
quota-exceeded: {switch-project: true, switch-preview-model: true, antigravity-credits: true}
antigravity: {sensitive-words: [a,b], connection-pool: {enabled: true, idle-conn-timeout: 2s, max-idle-conns-per-host: 3}}
devin: {sensitive-words: [c]}
codex: {disable-codex-cloaking: true, stream-bootstrap-buffering: true, stream-bootstrap-timeout: 3s, orphan-delegation-compatibility: true, live-media-relay: {enabled: true, max-sessions: 3, disable-private-remote-ips: true, public-ip: 1.2.3.4, udp-port-min: 4000, udp-port-max: 5000, ice-servers: [{urls: ['stun:fixture'], username: fixture-user, credential: fixture-secret}]}}
xai: {inject-x-search: true}
routing: {strategy: fill-first}
remote-management: {allow-remote: true, disable-control-panel: true, disable-auto-update-panel: true, panel-github-repository: 'https://user:pass@example.com/private?q=fixture-secret', base-url: 'https://user:pass@panel.test/private'}
`},
		{"api-keys: [fixture-one, fixture-two]\n", "api-keys: [fixture-three, fixture-four]\n"},
		{"api-keys: [fixture-one]\n", "api-keys: [fixture-two, fixture-three]\n"},
		{"", "payload: {default: [{models: [{name: a}], params: {key: fixture-secret}}], override: [{models: [{name: b}], params: {value: 5}}], filter: [{models: [{name: c}], params: [secret]}]}\n"},
		{"", "gemini-api-key: [{api-key: fixture-secret, base-url: https://user:pass@gemini.test/path}]\nclaude-api-key: [{api-key: fixture-secret}]\ncodex-api-key: [{api-key: fixture-secret, base-url: https://codex.test}]\nxai-api-key: [{api-key: fixture-secret, base-url: https://xai.test}]\nmeta-api-key: [{api-key: fixture-secret}]\nvertex-api-key: [{api-key: fixture-secret}]\ninteractions-api-key: [{api-key: fixture-secret}]\n"},
		{"gemini-api-key: [{api-key: fixture-one, base-url: https://old.test, models: [{name: A, alias: B}]}]\n", "gemini-api-key: [{api-key: fixture-two, base-url: https://user:pass@new.test/private, proxy-url: http://user:pass@proxy.test/private, prefix: test, disable-cooling: false, request-retry: 0, headers: {X-Foo: fixture-secret}, models: [{name: A, alias: B, is-compat: true}], excluded-models: [A,B]}]\n"},
		{"claude-api-key: [{api-key: fixture-one, cloak: {mode: auto}}]\n", "claude-api-key: [{api-key: fixture-two, cloak: {mode: always, strict-mode: true, sensitive-words: [fixture-secret]}, rebuild-mid-system-message: true, fingerprint-profile: default, models: [{name: a, alias: b}]}]\n"},
		{"codex-api-key: [{api-key: fixture-one, base-url: https://codex.test}]\n", "codex-api-key: [{api-key: fixture-two, base-url: https://codex.test, websockets: true, alpha-search: true, disable-codex-cloaking: true, disable-cooling: true, request-retry: 2}]\n"},
		{"oauth-excluded-models: {codex: [A, B], claude: [a]}\n", "oauth-excluded-models: {codex: [a, b, b], gemini: [b]}\n"},
		{"oauth-model-alias: {codex: [{name: a, alias: b, display-name: Old}]}\n", "oauth-model-alias: {codex: [{name: a, alias: b, display-name: New}], claude: [{name: c, alias: d, fork: true}]}\n"},
		{"oauth-settings: {codex: [{name: a, max-context-length: 1024}, {name: b}]}\n", "oauth-settings: {codex: [{name: b}, {name: a, max-context-length: 1024}]}\n"},
		{"oauth-request-scoped-errors: {codex: [{status: 400, match: [a], action: request}]}\n", "oauth-request-scoped-errors: {codex: [{status: 400, match: [b], action: request}], claude: [{status: 500, match-regexr: [error], action: cooldown}]}\n"},
		{"openai-compatibility: [{name: same, base-url: https://a.test, api-key-entries: [{api-key: fixture-one}]}, {name: same, base-url: https://b.test}]\n", "openai-compatibility: [{name: same, base-url: https://a.test, api-key-entries: [{api-key: fixture-two}], disabled: true, support-prompt-cache-key: true, disable-cooling: false, request-retry: 2}, {name: same, base-url: https://b.test, models: [{name: a}]}]\n"},
	}
	for _, family := range []string{"gemini", "interactions", "claude", "codex", "xai", "meta", "vertex"} {
		key := func(fields string) string {
			return fmt.Sprintf("%s-api-key: [{api-key: fixture-key, base-url: https://fixture.test, %s}]\n", family, fields)
		}
		pairs = append(pairs,
			[2]string{key("disable-cooling: null, request-retry: null"), key("disable-cooling: false, request-retry: 0, priority: 3, websockets: true")},
			[2]string{key("models: [{name: A, alias: B}]"), key("models: [{name: A, alias: B, thinking: {min: 1, levels: [low, high]}}]")},
			[2]string{key("models: [{name: A, alias: B}]"), key("models: [{name: A, alias: B, is-compat: true}]")},
			[2]string{key("models: [{name: A, alias: B}, {name: C, alias: D}]"), key("models: [{name: C, alias: D}, {name: A, alias: B}, {name: A, alias: B}]")},
		)
	}
	pairs = append(pairs,
		[2]string{"payload: {default: [{models: [], params: {temperature: 1000000.0}}]}\n", "payload: {default: [{models: [], params: {temperature: 1000000.0}}]}\n"},
		[2]string{"payload: {default: [{models: [], params: {temperature: 1000000.0}}]}\n", "payload: {default: [{models: [], params: {temperature: 1000000}}]}\n"},
		[2]string{"payload: {filter: [{models: [], params: [42]}]}\n", "payload: {filter: [{models: [], params: ['42']}]}\n"},
		[2]string{"", "payload: {default: []}\nantigravity: {sensitive-words: []}\n"},
		[2]string{"api-keys: [fixture-one]\n", "api-keys: [fixture-one, fixture-one]\n"},
		[2]string{"payload: {default: [{params: {temperature: 1}}]}\n", "payload: {default: [{params: {temperature: 1.0}}]}\n"},
		[2]string{"gemini-api-key: [{api-key: fixture-key, models: [{name: a, thinking: {}}]}]\n", "gemini-api-key: [{api-key: fixture-key, models: [{name: a, thinking: {levels: []}}]}]\n"},
		[2]string{"api-keys: [' fixture-one ']\n", "api-keys: [fixture-one]\n"},
		[2]string{"proxy-url: http://user:password@same.test/old\n", "proxy-url: http://user:password@same.test/new\n"},
	)
	for _, section := range []string{"default", "default-raw", "override", "override-raw", "filter"} {
		for _, field := range []string{"match", "not-match"} {
			rule := func(number string) string {
				match, notMatch := "[]", "[]"
				value := "[{temperature: " + number + "}]"
				if field == "match" {
					match = value
				} else {
					notMatch = value
				}
				params := "{temperature: '{}'}"
				if section == "filter" {
					params = "[temperature]"
				}
				return fmt.Sprintf("payload: {%s: [{models: [{name: fixture-model, headers: {}, match: %s, not-match: %s, exist: [], not-exist: []}], params: %s}]}\n", section, match, notMatch, params)
			}
			pairs = append(pairs, [2]string{rule("1"), rule("1.0")})
		}
	}
	for _, pair := range pairs {
		// Keep the golden independent of the machine's home directory. The one
		// explicit auth-dir change still exercises the actual resolved value.
		for i := range pair {
			if !strings.Contains(pair[i], "auth-dir:") {
				pair[i] = "auth-dir: /tmp/cpa-diff-fixture-auth\n" + pair[i]
			}
		}
		oldCfg, newCfg := load(pair[0]), load(pair[1])
		// Watcher rebuilds the old side from its own YAML snapshot, without loading
		// or re-sanitizing it; nil payload slices roundtrip as non-nil empty slices.
		previous, err := yaml.Marshal(oldCfg)
		if err != nil {
			panic(err)
		}
		var snapshot config.Config
		if err := yaml.Unmarshal(previous, &snapshot); err != nil {
			panic(err)
		}
		oldCfg = &snapshot
		cases = append(cases, map[string]any{"old": pair[0], "new": pair[1], "changes": diff.BuildConfigChangeDetails(oldCfg, newCfg)})
	}
	if err := json.NewEncoder(os.Stdout).Encode(cases); err != nil {
		panic(err)
	}
}
