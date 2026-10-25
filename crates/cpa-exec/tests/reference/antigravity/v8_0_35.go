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
	return []execScenario{
		{Name: "stream_client_drop_mid_read", Op: "stream", Source: "gemini", Model: "gemini-3.7-flash",
			Payload: geminiHello, Upstream: stalled(partial), CancelWhenIdle: true},
		{Name: "stream_client_drop_after_terminal", Op: "stream", Source: "gemini", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStream_ClientDisconnectPlainContextAfterTerminalChunkDoesNotReportFailure"},
			Payload: geminiHello, Upstream: stalled(partial + final), CancelWhenIdle: true},
		// A Claude client: the upstream finish reason marks the terminal chunk.
		{Name: "stream_client_drop_after_terminal_claude", Op: "stream", Source: "claude", Model: "gemini-3.7-flash",
			Payload: `{"model":"gemini-3.7-flash","max_tokens":256,"stream":true,"messages":[{"role":"user","content":"hello"}]}`,
			Upstream: stalled(partial + final), CancelWhenIdle: true},
	}
}
