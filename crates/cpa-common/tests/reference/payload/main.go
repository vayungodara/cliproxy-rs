// Command main writes goldens for cpa_common::payload and cpa_common::headers from
// CLIProxyAPI at 6fecc6e: ApplyPayloadConfigWithTrackedPathsForExecutor and
// ApplyCustomHeadersFromAttrs, called in-process. No sockets, no providers.
package main

import (
	"encoding/json"
	"net/http"
	"os"
	"sort"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
)

// configYAML exercises every rule section and gate. Each rule's params hold a single
// path so Go's random map iteration cannot change the result.
const configYAML = `
disable-image-generation: chat
payload:
  default:
    - models: [{name: "gpt-*"}]
      params: {temperature: 0.5}
    - models: [{name: "gpt-*"}]
      params: {temperature: 0.9}
    - models: [{name: "gpt-*", protocol: openai}]
      params: {metadata.tags: ["a", "b"]}
    - models: [{name: "gpt-*", from-protocol: openai-response}]
      params: {reasoning.effort: "high"}
    - models: [{name: "claude-*", headers: {X-Team: "red-*"}}]
      params: {top_k: 40}
    - models: [{name: "*-thinking"}]
      params: {suffix_hit: true}
    - models: [{name: "gpt-*", match: [{stream: true}]}]
      params: {stream_options: {include_usage: true, "<tag>": 1.25e1}}
    - models: [{name: "gpt-*", not-match: [{n: 2}]}]
      params: {n_checked: 1}
    - models: [{name: "gpt-*", exist: [user]}]
      params: {user_seen: "yes"}
    - models: [{name: "gpt-*", not-exist: [seed]}]
      params: {seed: 7}
    - models: [{name: "img-*"}]
      params: {tool_choice: image_generation}
  default-raw:
    - models: [{name: "gpt-*"}]
      params: {response_format: '{"type":"json_object"}'}
    - models: [{name: "gpt-*"}]
      params: {broken: '{"type":'}
  override:
    - models: [{name: "gpt-*"}]
      params: {max_tokens: 100}
    - models: [{name: "gpt-*"}]
      params: {max_tokens: 200}
    - models: [{name: "gpt-*"}]
      params: {model_label: "<b>&"}
    - models: [{name: "gpt-*"}]
      params: {big: 1e21}
    - models: [{name: "gpt-*"}]
      params: {"messages.#(role==\"system\").content": "rewritten"}
    - models: [{name: "gpt-*"}]
      params: {"tools.#(type==\"function\")#.strict": true}
    - models: [{name: "gemini-*"}]
      params: {generationConfig.maxOutputTokens: 64}
    - models: [{name: "gpt-*"}]
      params: {nullable: null}
    - models: [{name: "uni-*"}]
      params: {"items.#(é==1)#.x": true}
  override-raw:
    - models: [{name: "gpt-*"}]
      params: {logit_bias: '{"1": -100}'}
  filter:
    - models: [{name: "gpt-*"}]
      params: [frequency_penalty, "messages.#(role==\"developer\")#"]
`

type payloadCase struct {
	Name           string     `json:"name"`
	Target         string     `json:"target"`
	Model          string     `json:"model"`
	RequestedModel string     `json:"requested_model"`
	Protocol       string     `json:"protocol"`
	FromProtocol   string     `json:"from_protocol"`
	Root           string     `json:"root"`
	Payload        string     `json:"payload"`
	Original       string     `json:"original"`
	RequestPath    string     `json:"request_path"`
	Headers        [][]string `json:"headers"`
	Tracked        []string   `json:"tracked"`
	Out            string     `json:"out"`
	Touched        []string   `json:"touched"`
}

const chat = `{"model":"gpt-5","stream":true, "user":"u1","messages":[{"role":"system","content":"s"},{"role":"developer","content":"d1"},{"role":"user","content":"hi"},{"role":"developer","content":"d2"}],"tools":[{"type":"function","name":"a"},{"type":"image_generation"},{"type":"function","name":"b"}],"tool_choice":{"type":"image_generation"},"frequency_penalty":0.5,"max_tokens":100,"n":1,"big":1e21}`

var payloadInputs = []payloadCase{
	{Name: "chat_all_sections", Model: "gpt-5", RequestedModel: "gpt-5", Protocol: "openai", FromProtocol: "openai", Payload: chat, RequestPath: "/v1/chat/completions", Tracked: []string{"temperature", "messages", "max_tokens", "nope"}},
	{Name: "defaults_skip_fields_in_original", Model: "gpt-5", Protocol: "openai", Payload: `{"model":"gpt-5"}`, Original: `{"temperature":1,"seed":3}`},
	{Name: "responses_from_protocol", Model: "gpt-5", Protocol: "codex", FromProtocol: "openai-response", Payload: `{"input":"x"}`},
	{Name: "images_path_keeps_image_tool", Model: "gpt-image", Protocol: "openai", Payload: `{"tools":[{"type":"image_generation"}],"tool_choice":"image_generation"}`, RequestPath: "/v1/images/generations"},
	{Name: "string_tool_choice_stripped", Model: "x", Protocol: "openai", Payload: `{"tools":[{"type":"image_generation"}],"tool_choice":"image_generation"}`, RequestPath: "/v1/responses"},
	{Name: "header_gate_hit", Model: "claude-sonnet", Protocol: "claude", Payload: `{"messages":[]}`, Headers: [][]string{{"X-Team", "red-1"}}},
	{Name: "header_gate_miss", Model: "claude-sonnet", Protocol: "claude", Payload: `{"messages":[]}`, Headers: [][]string{{"X-Team", "blue"}}},
	{Name: "suffix_candidate", Model: "o3", RequestedModel: "o3-thinking(high)", Protocol: "openai", Payload: `{}`},
	{Name: "suffix_base_candidate", Model: "o3", RequestedModel: "gpt-5(high)", Protocol: "openai", Payload: `{}`},
	{Name: "root_envelope", Model: "gemini-2.5-pro", Protocol: "gemini", Root: "request", Payload: `{"request":{"contents":[]}}`},
	{Name: "no_model", Protocol: "openai", Payload: `{"a":1}`},
	{Name: "defaults_see_body_before_image_strip", Model: "img-1", Protocol: "openai", Payload: `{"tools":[{"type":"image_generation"}],"tool_choice":"image_generation"}`, RequestPath: "/v1/responses"},
	{Name: "unicode_query_key", Model: "uni-1", Protocol: "openai", Payload: `{"items":[{"é":1},{"é":2},{"é":1,"x":false}]}`},
	{Name: "codex_client_integer_tools", Target: "openai", Model: "zz", Protocol: "openai", Headers: [][]string{{"User-Agent", "codex_cli_rs/0.50"}},
		Payload: `{"tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"timeout_ms":{"type":"number"},"cmd":{"type":"string"},"yield_time_ms":{"type":["number","null","integer"]}}}},{"type":"function","name":"other","parameters":{"properties":{"timeout_ms":{"type":"number"}}}},{"type":"namespace","tools":[{"name":"collab__wait_agent","input_schema":{"properties":{"timeout_ms":{"type":"number"}}}}]}],"input":[{"type":"additional_tools","tools":[{"function":{"name":"functions__sleep","parameters":{"properties":{"duration_ms":{"type":"number"}}}}}]}]}`},
	{Name: "codex_target_skips_integer_tools", Target: "codex", Model: "zz", Protocol: "codex", Headers: [][]string{{"User-Agent", "codex_cli_rs/0.50"}},
		Payload: `{"tools":[{"type":"function","name":"exec_command","parameters":{"properties":{"timeout_ms":{"type":"number"}}}}]}`},
}

type headerCase struct {
	Name      string            `json:"name"`
	Attrs     map[string]string `json:"attrs"`
	Client    [][]string        `json:"client"`
	SessionID string            `json:"session_id"`
	Out       map[string]string `json:"out"`
	Host      string            `json:"host"`
}

var headerInputs = []headerCase{
	{Name: "mixed", Attrs: map[string]string{
		"header:X-Literal": "  plain  ", "header:X-Copy": "$X-Claude-Code-Session-Id", "header:X-Missing": "$X-Absent",
		"header:X-Session": "$cpa-session-id", "header:X-Embedded": "a-$CPA-SESSION-ID-b", "header: ": "x", "header:X-Blank": " ",
		"api_key": "fake", "header:Host": "upstream.invalid",
	}, Client: [][]string{{"X-Claude-Code-Session-Id", "client-sid"}}, SessionID: "header:s1"},
	{Name: "no_session", Attrs: map[string]string{"header:X-Session": "$CPA-SESSION-ID", "header:X-Embedded": "pre $CPA-SESSION-ID", "header:X-Lit": "v"}},
	{Name: "multi_value", Attrs: map[string]string{"header:X-Copy": "$x-multi"}, Client: [][]string{{"X-Multi", "first"}, {"X-Multi", "second"}}},
}

func main() {
	cfg, err := config.ParseConfigBytes([]byte(configYAML))
	if err != nil {
		panic(err)
	}
	out := map[string]any{"config": configYAML}
	for i, c := range payloadInputs {
		headers := http.Header{}
		for _, h := range c.Headers {
			headers.Add(h[0], h[1])
		}
		var original []byte
		if c.Original != "" {
			original = []byte(c.Original)
		}
		result, touched := helps.ApplyPayloadConfigWithTrackedPathsForExecutor(cfg, c.Target, c.Model, c.Protocol, c.FromProtocol, c.Root,
			[]byte(c.Payload), original, c.RequestedModel, c.RequestPath, headers, c.Tracked...)
		c.Out = string(result)
		c.Touched = []string{}
		for k, v := range touched {
			if v {
				c.Touched = append(c.Touched, k)
			}
		}
		sort.Strings(c.Touched)
		payloadInputs[i] = c
	}
	out["payload"] = payloadInputs
	for i, c := range headerInputs {
		client := http.Header{}
		for _, h := range c.Client {
			client.Add(h[0], h[1])
		}
		req, _ := http.NewRequest(http.MethodPost, "http://default.invalid/v1", nil)
		ctx := util.WithSessionID(req.Context(), c.SessionID)
		req = req.WithContext(ctx)
		util.ApplyCustomHeadersFromAttrs(req, c.Attrs, client)
		c.Out = map[string]string{}
		for k, v := range req.Header {
			c.Out[k] = v[0]
		}
		c.Host = req.Host
		headerInputs[i] = c
	}
	out["headers"] = headerInputs
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
