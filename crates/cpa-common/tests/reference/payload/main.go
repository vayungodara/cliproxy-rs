// Command main writes goldens for cpa_common::payload and cpa_common::headers from
// CLIProxyAPI v8.0.35 (ae3ee3be): ApplyPayloadConfigWithTrackedPathsForExecutor and
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
    - models: [{name: "wild-*"}]
      params: {"items.#(k==1)#.d": "dv"}
    - models: [{name: "wild-*"}]
      params: {"items.#(k==1)#.n.m": 5}
    - models: [{name: "wild-*"}]
      params: {"strs.#(==\"[1,2]\")#.0": "scalar"}
    - models: [{name: "lin-*"}]
      params: {"items.#(k==1)#.0": 9}
    - models: [{name: "lin-*"}]
      params: {"whole.#(k==2)#": 5}
    - models: [{name: "lin-*", exist: ["whole.#(k==1)#"]}]
      params: {whole_seen: true}
    - models: [{name: "lin-*", match: [{"nums.#(>5)#": 7}]}]
      params: {nums_matched: true}
    - models: [{name: "wild-*", exist: ["strs.#(==\"[1,2]\")#.0"]}]
      params: {scalar_exists: true}
    - models: [{name: "lno-*"}]
      params: {"items.#(k==1)#.x": 9}
    - models: [{name: "lno-*"}]
      params: {"whole.#(k==2)#": 5}
    - models: [{name: "lns-*"}]
      params: {"-1.#(k==1)#.d": 2}
    - models: [{name: "lnx-*", exist: ["items.#(k==1)#.z"]}]
      params: {z_seen: true}
    - models: [{name: "subc-*", exist: ["{a}"]}]
      params: {sub_seen: true}
    - models: [{name: "subn-*", not-exist: ["{a}"]}]
      params: {subn_seen: true}
    - models: [{name: "esc-*", exist: ["k\\|@reverse"]}]
      params: {esc_seen: true}
  default-raw:
    - models: [{name: "rawd-*"}]
      params: {"@reverse": '{', x: '9'}
    - models: [{name: "gpt-*"}]
      params: {response_format: '{"type":"json_object"}'}
    - models: [{name: "gpt-*"}]
      params: {broken: '{"type":'}
    - models: [{name: "wild-*"}]
      params: {"items.#(k==2)#.r": '{"x":1}'}
    - models: [{name: "lin-*"}]
      params: {"multi.#(k==1)#.r": '{"y":2}'}
    - models: [{name: "lno-*"}]
      params: {"items.#(k==1)#.y": '{"a":1}'}
    - models: [{name: "time-*"}]
      params: {raw_at: 2001-12-14T21:59:43.10-05:00}
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
    - models: [{name: "wild-*"}]
      params: {"items.#(k==1)#.o": "ov"}
    - models: [{name: "wild-*"}]
      params: {"groups.#(g==1)#.members.#(m==1)#.flag": true}
    - models: [{name: "wild-*"}]
      params: {"i*.#(k==1)#.new": "complex"}
    - models: [{name: "lin-*"}]
      params: {"whole.#(k==1)#": "replaced"}
    - models: [{name: "lin-*"}]
      params: {"lap.#(k==1)#.b": 2}
    - models: [{name: "lin-*"}]
      params: {"arr.#(k==1)#.list.-1": "app"}
    - models: [{name: "lin-*"}]
      params: {"eq.#(k==1)#.v": 1}
    - models: [{name: "lin-*"}]
      params: {"a*.#(k==1)#.new": "complex"}
    - models: [{name: "lnc-*"}]
      params: {"a*.#(k==1)#.b.#(k==2)#.x": 9}
    - models: [{name: "lng-*"}]
      params: {"g.#(k==1)#.m.#(k==2)#.x": 9}
    - models: [{name: "lns-*"}]
      params: {"-1.#(k==1)#.s": 1}
    - models: [{name: "sub-*"}]
      params: {"[a].#(k==1)#.x": 9}
    - models: [{name: "mod-*"}]
      params: {"@reverse": 1}
    - models: [{name: "ne0-*", not-exist: ["@this:{}"]}]
      params: {hit: 9}
    - models: [{name: "ne1-*", not-exist: ["a|!true"]}]
      params: {hit: 9}
    - models: [{name: "ne2-*", not-exist: ["a|..#"]}]
      params: {hit: 9}
    - models: [{name: "ne3-*", not-exist: ["a.#(k==1)|@reverse"]}]
      params: {hit: 9}
    - models: [{name: "ne4-*", not-exist: ["a.#(@keys)"]}]
      params: {hit: 9}
    - models: [{name: "ne5-*", not-exist: ["a.#[k==1]"]}]
      params: {hit: 9}
    - models: [{name: "ne6-*", not-exist: ["a|#(k==1)"]}]
      params: {hit: 9}
    - models: [{name: "ne7-*", not-exist: ["a(|@reverse"]}]
      params: {hit: 9}
    - models: [{name: "lnw-*"}]
      params: {"a.18446744073709551616.#(k==1)#.x": 9}
    - models: [{name: "qp0-*", not-exist: ['a.#(s=="")']}]
      params: {hit: 9}
    - models: [{name: "qp1-*", not-exist: ["a.#(x==~null)"]}]
      params: {hit: 9}
    - models: [{name: "qp2-*", not-exist: ['a.#(s%"?")']}]
      params: {hit: 9}
    - models: [{name: "qp3-*"}]
      params: {'a.#(s!="").v': 9}
    - models: [{name: "qp4-*", not-exist: ['a.#([k])']}]
      params: {hit: 9}
    - models: [{name: "qp5-*"}]
      params: {'a.#(!=0)': 9}
    - models: [{name: "qp6-*", not-exist: ['a.#(==~null)']}]
      params: {hit: 9}
    - models: [{name: "time-*"}]
      params: {at: 2001-12-14 21:59:43.10}
    - models: [{name: "time-*", match: [{when: 2001-1-2}]}]
      params: {matched: true}
    - models: [{name: "time-*"}]
      params: {text: '2001-12-14'}
  override-raw:
    - models: [{name: "rawo-*"}]
      params: {"@reverse": '{', x: '9'}
    - models: [{name: "rawk-*"}]
      params: {"@reverse": '1', x: '9'}
    - models: [{name: "gpt-*"}]
      params: {logit_bias: '{"1": -100}'}
    - models: [{name: "wild-*"}]
      params: {"items.#(k==1)#.raw": '[1, 2]'}
    - models: [{name: "lin-*"}]
      params: {"lap.#(k==1)#": '{"k":1,"z":true}'}
    - models: [{name: "lin-*"}]
      params: {"eq.#(k==1)#.w": '[1, 2]'}
  filter:
    - models: [{name: "gpt-*"}]
      params: [frequency_penalty, "messages.#(role==\"developer\")#"]
    - models: [{name: "wild-*"}]
      params: ["items.#(k==1)#.drop"]
    - models: [{name: "wild-*"}]
      params: ["list.#(t==\"x\")#"]
    - models: [{name: "wild-*"}]
      params: ["nested.#(a==1)#.inner.#(b==1)#"]
    - models: [{name: "wild-*"}]
      params: ["i*.#(k==1)#.x"]
    - models: [{name: "lin-*"}]
      params: ["diagnostics.a*.#(k==1)#"]
    - models: [{name: "lin-*"}]
      params: [":a.#(k==1)#.x"]
    - models: [{name: "lin-*"}]
      params: ["neg.#(k==1)#.list.-1"]
    - models: [{name: "lnm-*"}]
      params: ["-1.#(k==1)#"]
    - models: [{name: "lnf-*"}]
      params: ["-1.#(k==1)#.y"]
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

const wild = `{"items":[ {"k":1,"d":"have","o":"ov","drop":1} , {"k":2} ,{"k":1,"drop":2,"o":"old"},{"k":3}],` +
	`"list":[ {"t":"x"} ,{"t":"x"},  {"t":"y"} , {"t":"x"}],` +
	`"groups":[{"g":1,"members":[{"m":1},{"m":2},{"m":1,"flag":false}]},{"g":2,"members":[{"m":1}]},{"g":1,"members":[ {"m":1} ]}],` +
	`"nested":[{"a":1,"inner":[{"b":1},{"b":2},{"b":1}]},{"a":2,"inner":[{"b":1}]},{"a":1,"inner":[{"b":1}]}]}`

const lin = `{"diagnostics":{"ab":[{"k":1},{"k":1}]},":a":[{"k":1,"x":1},{"k":1,"x":2}],"a":[{"k":1,"x":3},{"k":1}],` +
	`"whole":[{"k":1},{"k":2},{"k":1},{"k":2}],"lap":[{"k":1,"b":0},{"k":2},{"k":1}],` +
	`"arr":[{"k":2,"list":[1]},{"k":1,"list":[1,2]},{"k":1,"list":[]}],"neg":[{"k":1,"list":[1,2,3]},{"k":1,"list":[4]}],` +
	`"multi":[{"k":1},{"k":1,"r":{"old":1}},{"k":1}],"eq":[{"k":1,"v":1,"w":[1, 2]},{"k":1,"v":2,"w":[1,2]}],"nums":[3,7,9]}`

const lno = `{"items":[{"k":1},{"k":1},{"k":1}],"whole":[{"k":2},{"k":2}]}`

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
	// Wildcard targets: element-local edits, nested queries, and whole-element deletes
	// (first, adjacent, last and all elements) with uneven whitespace.
	{Name: "wildcards", Model: "wild-1", Protocol: "openai", Payload: wild, Tracked: []string{"items.2", "list", "groups.0.members.2.flag", "nested.2", "items.1.r"}},
	{Name: "wildcards_all_deleted", Model: "wild-1", Protocol: "openai",
		Payload: "{\"list\":[ {\"t\":\"x\"} ,\n {\"t\":\"x\"}\t]}"},
	{Name: "wildcards_first_two_deleted", Model: "wild-1", Protocol: "openai",
		Payload: "{\"list\": [\n  {\"t\":\"x\"},\n  {\"t\":\"x\"} ,\n  {\"t\":\"y\"}  ,{\"t\":\"x\"},{\"t\":\"y\"}\n]}"},
	{Name: "wildcards_middle_deleted", Model: "wild-1", Protocol: "openai",
		Payload: "{\"list\":[{\"t\":\"y\"} , {\"t\":\"x\"},{\"t\":\"x\"} ,  {\"t\":\"y\"}]}"},
	// A scalar element is not a container, and a wildcard before the query makes the
	// whole path complex for sjson.
	{Name: "wildcards_scalars_and_complex_base", Model: "wild-1", Protocol: "openai",
		Payload: `{"strs":["[1,2]","[1,2]"],"items":[{"k":1,"x":1},{"k":2,"x":2},{"k":1,"x":3}]}`,
		Tracked: []string{"strs", "items.0.x", "items.2.new"}},
	// Whole-element setters and reads, complex prefixes, a leading-colon key, append and
	// negative indexes inside elements, overlapping parameters, equal values, and
	// default-raw on several elements.
	{Name: "linear_targets", Model: "lin-1", Protocol: "openai", Payload: lin,
		Tracked: []string{"diagnostics", ":a", "a", "whole", "lap.0", "lap.2.b", "arr.1.list", "neg", "multi", "eq.0.v", "eq.1.w"}},
	{Name: "linear_whole_defaults", Model: "lin-1", Protocol: "openai",
		Payload: `{"whole":[{"k":2},{"k":2}]}`, Original: `{"whole":[]}`, Tracked: []string{"whole"}},
	// A scalar in the original is not a container: the defaults inside it apply.
	{Name: "linear_scalar_original", Model: "lin-1", Protocol: "openai",
		Payload: `{"items":[{"k":1},{"k":1}]}`, Original: `{"items":["[1,2]","[1,2]"]}`, Tracked: []string{"items.0.0", "items.1.0"}},
	{Name: "wildcards_defaults_read_original", Model: "wild-1", Protocol: "openai",
		Payload: `{"items":[{"k":1},{"k":2},{"k":1}]}`, Original: `{"items":[{"k":1},{"k":2},{"k":1,"d":"orig","n":{"m":1}}]}`},
	// gjson's search from the root moves past a wildcard or duplicate key whose rest is
	// missing; sjson's delete reads an unforced -1 as the last element; defaults read an
	// original whose target array is missing, a scalar, an object or duplicated.
	{Name: "nested_complex_prefix", Model: "lnc-1", Protocol: "openai",
		Payload: `{"aa":[{"k":0},{"k":1}],"ab":[{},{"b":[{"k":2,"x":0}]}]}`, Tracked: []string{"a*.1.b.0.x"}},
	{Name: "nested_duplicate_key", Model: "lng-1", Protocol: "openai",
		Payload: `{"g":[{"k":1}],"g":[{"k":1,"m":[{"k":2}]}]}`, Tracked: []string{"g"}},
	{Name: "condition_duplicate_key", Model: "lnx-1", Protocol: "openai",
		Payload: `{"items":[{"k":1},{"k":1}],"items":[{"z":1},{"z":1}]}`},
	{Name: "delete_under_minus_one", Model: "lnm-1", Protocol: "openai",
		Payload: `{"#":1,"0":[{"k":1},{"k":1}],"-1":[{"k":1},{"k":1}]}`, Tracked: []string{"-1.0", "-1.1", "0"}},
	{Name: "delete_fields_under_minus_one", Model: "lnf-1", Protocol: "openai",
		Payload: `{"#":1,"0":[{"k":1,"y":1},{"k":1,"y":2}],"-1":[{"k":1,"y":3},{"k":1,"y":4}]}`, Tracked: []string{"-1", "0"}},
	// gjson's parseUint wraps: 2^64 indexes element 0.
	{Name: "wrapped_index", Model: "lnw-1", Protocol: "openai",
		Payload: `{"a":[[{"k":1},{"k":1}]]}`, Tracked: []string{"a"}},
	// gjson syntax the port does not read: cliproxy-rs ignores these rules
	// (REFUSED in tests/payload.rs).
	{Name: "subselector_override", Model: "sub-1", Protocol: "openai",
		Payload: `{"[a]":[{"k":1},{"k":1}],"a":[{"k":1}]}`, Tracked: []string{"[a]", "a"}},
	{Name: "modifier_override", Model: "mod-1", Protocol: "openai", Payload: `{"a":1}`, Tracked: []string{"@reverse"}},
	// An escaped pipe is part of the key, so @reverse is not a modifier here.
	{Name: "escaped_pipe_key", Model: "esc-1", Protocol: "openai", Payload: `{"k|@reverse":1}`},
	// Read contexts the port does not implement, under not-exist: Go reads each as
	// present, so the rule does not apply.
	{Name: "unported_not_exist_0", Model: "ne0-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	{Name: "unported_not_exist_1", Model: "ne1-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	{Name: "unported_not_exist_2", Model: "ne2-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	{Name: "unported_not_exist_3", Model: "ne3-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	{Name: "unported_not_exist_4", Model: "ne4-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	{Name: "unported_not_exist_5", Model: "ne5-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	{Name: "unported_not_exist_6", Model: "ne6-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	{Name: "unported_not_exist_7", Model: "ne7-1", Protocol: "openai", Payload: `{"a":[{"k":1}]}`},
	// Query predicates as gjson v1.18.0 matches them: "" unquoted, ~null on a missing
	// field, ? against one rune, and a != "" write that matches nothing.
	{Name: "query_empty_string", Model: "qp0-1", Protocol: "openai", Payload: `{"a":[{"s":""}]}`},
	{Name: "query_tilde_null_missing", Model: "qp1-1", Protocol: "openai", Payload: `{"a":[{}]}`},
	{Name: "query_pattern_rune", Model: "qp2-1", Protocol: "openai", Payload: `{"a":[{"s":"é"}]}`},
	{Name: "query_not_empty_write", Model: "qp3-1", Protocol: "openai", Payload: `{"a":[{"s":""}]}`},
	// A query path in syntax the port does not read (a subselector): refused.
	{Name: "query_subselector_path", Model: "qp4-1", Protocol: "openai", Payload: `{"a":[{"k":1},{"k":2}]}`},
	// gjson's array-query loop reads nan as a number (NaN), where Array() says null.
	{Name: "query_nan_not_zero", Model: "qp5-1", Protocol: "openai", Payload: `{"a":[nan]}`},
	{Name: "query_nan_not_null", Model: "qp6-1", Protocol: "openai", Payload: `{"a":[nan]}`},
	// A raw rule with an invalid value is dropped whole, unported parameters included.
	{Name: "raw_override_invalid_with_unported", Model: "rawo-1", Protocol: "openai", Payload: `{}`, Tracked: []string{"x"}},
	{Name: "raw_default_invalid_with_unported", Model: "rawd-1", Protocol: "openai", Payload: `{}`, Tracked: []string{"x"}},
	{Name: "raw_override_valid_with_unported", Model: "rawk-1", Protocol: "openai", Payload: `{}`, Tracked: []string{"x"}},
	{Name: "subselector_condition", Model: "subc-1", Protocol: "openai", Payload: `{"b":1}`},
	{Name: "subselector_not_exist", Model: "subn-1", Protocol: "openai", Payload: `{"b":1}`},
	{Name: "set_under_minus_one", Model: "lns-1", Protocol: "openai",
		Payload: `{"#":1,"0":[{"k":1}],"-1":[{"k":1},{"k":1}]}`, Tracked: []string{"-1", "0"}},
	{Name: "original_lacks_array", Model: "lno-1", Protocol: "openai", Payload: lno, Original: `{"pad":"pp"}`, Tracked: []string{"items", "whole"}},
	{Name: "original_scalar_base", Model: "lno-1", Protocol: "openai", Payload: lno, Original: `{"items":"[{\"x\":1}]","whole":7}`, Tracked: []string{"items", "whole"}},
	{Name: "original_object_base", Model: "lno-1", Protocol: "openai", Payload: lno,
		Original: `{"items":{"0":{"x":1},"1":"s","2":{"y":0}},"whole":{"1":0}}`, Tracked: []string{"items", "whole"}},
	{Name: "original_object_duplicate_keys", Model: "lno-1", Protocol: "openai", Payload: lno,
		Original: `{"items":{"0":"s","0":{"x":1},"2":{"y":1},"2":"t"},"whole":{"0":"a","0":"b"}}`, Tracked: []string{"items", "whole"}},
	{Name: "original_duplicate_base", Model: "lno-1", Protocol: "openai", Payload: lno,
		Original: `{"items":[{}],"items":[{"x":1},{"y":1},{}]}`, Tracked: []string{"items", "whole"}},
	{Name: "codex_target_skips_integer_tools", Target: "codex", Model: "zz", Protocol: "codex", Headers: [][]string{{"User-Agent", "codex_cli_rs/0.50"}},
		Payload: `{"tools":[{"type":"function","name":"exec_command","parameters":{"properties":{"timeout_ms":{"type":"number"}}}}]}`},
	// A plain timestamp in params is a time.Time, marshaled as RFC 3339 text, raw too.
	{Name: "timestamps", Model: "time-1", Protocol: "openai", Payload: `{"when":"2001-01-02T00:00:00Z"}`},
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
