//go:build v8035

// Scenarios for the Antigravity behaviour ported from CLIProxyAPI v8.0.35. They need the
// v8.0.35 tree, so they build only with `-tags v8035` and run with
// ANTIGRAVITY_GO_SET=v8.0.35 (README.md).
package main

func init() {
	scenarioSets["v8.0.35"] = v8035Scenarios
}

func v8035Scenarios() []execScenario {
	// A client that goes away mid-stream while the upstream stays silent: before the
	// terminal chunk and after it (dbc4aad9's rule, which Antigravity shares).
	stalled := func(body string) *upstream {
		return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: body, Missing: 40, Stall: true}
	}
	partial := `data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]}}],"usageMetadata":{"promptTokenCount":3,"totalTokenCount":3},"modelVersion":"gemini-3.7-flash"}}` + "\n\n"
	final := `data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":" done"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5},"modelVersion":"gemini-3.7-flash"}}` + "\n\n"
	geminiHello := `{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}`
	// a4acc9f7: once a terminal chunk went out, the reasoning replay ledger is committed
	// however the stream ends (a client that goes away, an error payload, a read error);
	// before it, only a clean end commits. Each reads the ledger of its own session.
	thought := `data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"weighing","thoughtSignature":"c2lnbmF0dXJlLXJlcGxheS1hZnRlci10ZXJtaW5hbA=="}]}}],"usageMetadata":{"promptTokenCount":3,"totalTokenCount":3},"modelVersion":"gemini-3.7-flash"}}` + "\n\n"
	replay := func(name, source, payload string, up *upstream) execScenario {
		return execScenario{Name: name, Op: "stream", Source: source, Model: "gemini-3.7-flash", Payload: payload,
			Upstream: up, CancelWhenIdle: up.Stall, Headers: map[string]string{"Session-Id": name}, ReplayKey: "responses:" + name}
	}
	events := func(body string, missing int) *upstream {
		return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: body, Missing: missing}
	}
	responsesHello := `{"model":"gemini-3.7-flash","input":"hello","stream":true}`
	out := []execScenario{
		{Name: "stream_client_drop_mid_read", Op: "stream", Source: "gemini", Model: "gemini-3.7-flash",
			Payload: geminiHello, Upstream: stalled(partial), CancelWhenIdle: true},
		{Name: "stream_client_drop_after_terminal", Op: "stream", Source: "gemini", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStream_ClientDisconnectPlainContextAfterTerminalChunkDoesNotReportFailure"},
			Payload: geminiHello, Upstream: stalled(partial + final), CancelWhenIdle: true},
		// A Claude client: the upstream finish reason marks the terminal chunk.
		{Name: "stream_client_drop_after_terminal_claude", Op: "stream", Source: "claude", Model: "gemini-3.7-flash",
			Payload:  `{"model":"gemini-3.7-flash","max_tokens":256,"stream":true,"messages":[{"role":"user","content":"hello"}]}`,
			Upstream: stalled(partial + final), CancelWhenIdle: true},
		// Base URLs Go refuses: an escaped host fails http.NewRequest before the request
		// is logged; no host, or a scheme other than http(s), fails in the transport after
		// the log. The access token reaches no host.
		{Name: "execute_escaped_host_base", Op: "execute", Source: "gemini", Model: "gemini-3.7-flash",
			Payload: geminiHello, Attributes: map[string]string{"base_url": "https://%64aily-cloudcode-pa.googleapis.com"}},
		{Name: "stream_hostless_base", Op: "stream", Source: "gemini", Model: "gemini-3.7-flash",
			Payload: geminiHello, Attributes: map[string]string{"base_url": "https:relay.example"}},
		{Name: "count_schemeless_base", Op: "count", Source: "gemini", Model: "gemini-3.7-flash",
			Payload: geminiHello, Attributes: map[string]string{"base_url": "relay.example"}},
		replay("replay_clean_end", "gemini", geminiHello, events(thought+final, 0)),
		replay("replay_drop_before_terminal", "gemini", geminiHello, stalled(thought)),
		replay("replay_drop_after_terminal", "gemini", geminiHello, stalled(thought+final)),
		replay("replay_drop_after_terminal_responses", "openai-response", responsesHello, stalled(thought+final)),
		// The error event ends the body: the stream stops reading there.
		replay("replay_error_after_terminal", "gemini", geminiHello,
			events(thought+final+`data: {"error":{"code":503,"message":"late","status":"UNAVAILABLE"}}`+"\n", 0)),
		replay("replay_read_error_after_terminal", "gemini", geminiHello, events(thought+final, 40)),
		replay("replay_read_error_before_terminal", "gemini", geminiHello, events(thought, 40)),
		// An apply_patch call the bridge cannot translate ends the stream at the terminal
		// chunk (the body ends there too), and nothing is committed.
		replay("replay_apply_patch_failure_at_terminal", "openai-response",
			`{"model":"gemini-3.7-flash","stream":true,"input":[{"role":"user","content":"edit a.txt"}],"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}`,
			events(thought+`data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"apply_patch","args":{"input":7}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5},"modelVersion":"gemini-3.7-flash"}}`+"\n", 0)),
		// A Responses client: Go commits as soon as a payload translates into
		// response.completed (c997cbb5), so a later payload's signature stays out.
		replay("replay_after_completed_responses", "openai-response", responsesHello,
			events(thought+final+`data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"late","thoughtSignature":"bGF0ZS1zaWduYXR1cmUtYWZ0ZXItY29tcGxldGlvbg=="}]}}],"modelVersion":"gemini-3.7-flash"}}`+"\n\n", 0)),
		// The same stream for a Gemini client: no completion event, so the clean end commits
		// every payload.
		replay("replay_after_completed_gemini", "gemini", geminiHello,
			events(thought+final+`data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"late","thoughtSignature":"bGF0ZS1zaWduYXR1cmUtYWZ0ZXItY29tcGxldGlvbg=="}]}}],"modelVersion":"gemini-3.7-flash"}}`+"\n\n", 0)),
	}
	// a2718314: trailing model turns made only of thought parts are dropped before the
	// boundary user turns (Gemini models only), keeping at least one turn. The Responses
	// cases are Go's TestAntigravityDropsTrailingStructuredReasoningTurn.
	okStream := sseOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"OK"}]},"finishReason":"STOP"}],"responseId":"resp-tt"}}`)
	okJSON := jsonOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"OK"}]},"finishReason":"STOP"}],"responseId":"resp-tt"}}`)
	reasoningTail := func(model string, stream bool, prefix, summary, content string) string {
		streamValue := "false"
		if stream {
			streamValue = "true"
		}
		return `{"model":"` + model + `","stream":` + streamValue + `,` +
			`"reasoning":{"effort":"high","summary":"detailed"},"include":["reasoning.encrypted_content"],"input":[` +
			`{"type":"message","role":"user","content":[{"type":"input_text","text":"Say OK."}]},` + prefix +
			`{"type":"reasoning","id":"rs_repro_0","summary":` + summary + `,"content":` + content + `}` +
			`]}`
	}
	const toolPrefix = `{"type":"function_call","call_id":"call_1","name":"run","arguments":"{}"},` +
		`{"type":"function_call_output","call_id":"call_1","output":"done"},`
	const structured = `[{"type":"summary_text","text":"thinking"}]`
	trailing := func(name, model string, stream bool, prefix, summary, content string) execScenario {
		op, up := "execute", okJSON
		if stream {
			op, up = "stream", okStream
		}
		return execScenario{Name: name, Op: op, Source: "openai-response", Model: model,
			GoTests: []string{"TestAntigravityDropsTrailingStructuredReasoningTurn"},
			Payload: reasoningTail(model, stream, prefix, summary, content), Upstream: up}
	}
	geminiTail := func(name, model, contents string) execScenario {
		return execScenario{Name: name, Op: "stream", Source: "gemini", Model: model,
			Payload: `{"contents":` + contents + `}`, Upstream: okStream}
	}
	const userTurn = `{"role":"user","parts":[{"text":"Say OK."}]}`
	const thoughtTurn = `{"role":"model","parts":[{"thought":true,"text":"thinking"}]}`
	out = append(out,
		trailing("trailing_thought_stream", "gemini-3.8-flash-high", true, "", structured, "null"),
		trailing("trailing_thought_execute", "gemini-3.8-flash-high", false, "", structured, "null"),
		trailing("trailing_thought_empty_content", "gemini-3.8-flash-high", true, "", structured, "[]"),
		trailing("trailing_thought_pro_agent", "gemini-pro-agent", true, "", structured, "null"),
		trailing("trailing_thought_after_tool_output", "gemini-3.8-flash-high", true, toolPrefix, structured, "null"),
		trailing("trailing_thought_plain_summary", "gemini-3.8-flash-high", true, "", `["thinking"]`, "null"),
		// Every trailing thought-only turn goes; one with text, a function call, a false
		// thought or no parts stays, as does one ahead of a user turn, and a lone turn is kept.
		geminiTail("trailing_thought_two_turns", "gemini-3.8-flash-high", `[`+userTurn+`,`+thoughtTurn+`,{"role":"model","parts":[{"thought":true,"text":"more"},{"thought":true,"text":"again"}]}]`),
		geminiTail("trailing_thought_with_text", "gemini-3.8-flash-high", `[`+userTurn+`,{"role":"model","parts":[{"thought":true,"text":"thinking"},{"text":"answer"}]}]`),
		geminiTail("trailing_thought_with_call", "gemini-3.8-flash-high", `[`+userTurn+`,{"role":"model","parts":[{"thought":true,"functionCall":{"name":"run","args":{}}}]}]`),
		geminiTail("trailing_thought_false", "gemini-3.8-flash-high", `[`+userTurn+`,{"role":"model","parts":[{"thought":false,"text":"thinking"}]}]`),
		geminiTail("trailing_thought_empty_parts", "gemini-3.8-flash-high", `[`+userTurn+`,{"role":"model","parts":[]}]`),
		// No user text to derive a session from: the conductor's session ID keeps it stable.
		func() execScenario {
			s := geminiTail("trailing_thought_only_turn", "gemini-3.8-flash-high", `[`+thoughtTurn+`]`)
			s.DerivedSession = "session-trailing-thought"
			return s
		}(),
		geminiTail("trailing_thought_before_user", "gemini-3.8-flash-high", `[`+userTurn+`,`+thoughtTurn+`,`+userTurn+`]`),
	)
	// Base scenarios (executor.go) whose OpenAI chat output v8.0.35 changed: thoughts
	// tokens count in completion_tokens (ab151f5d) and stream deltas carry no
	// "tool_calls": null (dfe0a0ef). Recorded again under their own names; the Rust
	// replay takes these in place of the 6fecc6e recordings.
	rerecorded := map[string]bool{
		"openai_stream": true, "go_stream_no_finish_reason_openai": true, "go_stream_split_terminal_usage": true,
	}
	for _, s := range execScenarios() {
		if rerecorded[s.Name] {
			out = append(out, s)
		}
	}
	return out
}
