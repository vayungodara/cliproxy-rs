package main

import (
	"fmt"
	"strings"
)

// goTestScenarios ports the executor-level cases of Go's xai_websockets_executor_test.go
// (named after their Go test) with the same request payloads and upstream events. Go's
// own executor produces the recorded output. Each runs with an execution session, as a
// downstream WebSocket turn always has one in this port.
func goTestScenarios() []*scenario {
	token := func(t string) map[string]string {
		return map[string]string{"base_url": base, "websockets": "true", "access_token": t}
	}
	one := func(name, model, payload string, events ...string) *scenario {
		return &scenario{Name: name, Session: "sess-" + name, Auths: map[string]map[string]string{"x": token("xai-token")},
			Turns: []*turn{{Auth: "x", Model: model, Payload: payload, Acts: reply1(events)}}}
	}
	out := []*scenario{
		{Name: "RequiredUpstreamRejectsCompactionHTTPFallback", Session: "sess-req-compact", Auths: map[string]map[string]string{"x": token("xai-token")},
			Turns: []*turn{{Auth: "x", Model: "grok-4", Continuation: true, Payload: `{"model":"grok-4","input":[{"type":"compaction_trigger"}]}`}}},
		one("RestoresNamespaceToolCalls", "grok-4.3",
			`{"model":"grok-4.3","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"mcp__exa","tools":[{"type":"function","name":"web_search_exa","parameters":{"type":"object"}}]}]},{"role":"user","content":"use Exa"}]}`,
			`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"mcp__exa__web_search_exa","call_id":"call_1","arguments":"{}"}}`,
			`{"type":"response.completed","response":{"id":"resp_1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}`),
		one("RestoresAliasedWebSearch", "grok-4.6",
			`{"model":"grok-4.6","input":[{"role":"user","content":"search query"}],"tools":[{"type":"function","name":"web_search","parameters":{"type":"object"}}]}`,
			`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"clientfn_web_search","call_id":"call_1","arguments":"{}"}}`,
			`{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"function_call","name":"clientfn_web_search","call_id":"call_1"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}`),
		one("DoesNotRestoreNamespacedClientfnWebSearch", "grok-4.6",
			`{"model":"grok-4.6","input":[{"role":"user","content":"search query"}],"tools":[{"type":"function","name":"web_search","parameters":{"type":"object"}},{"type":"namespace","name":"acme","tools":[{"type":"function","name":"clientfn_web_search","parameters":{"type":"object"}}]}]}`,
			`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"acme__clientfn_web_search","call_id":"call_1","arguments":"{}"}}`,
			`{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","name":"clientfn_web_search","call_id":"call_2","arguments":"{}"}}`,
			`{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"function_call","name":"acme__clientfn_web_search","call_id":"call_1"},{"type":"function_call","name":"clientfn_web_search","call_id":"call_2"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}`),
		one("PreservesClientSameNameToolsWithXSearch", "grok-4.5",
			`{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"},{"type":"function","name":"x_keyword_search","parameters":{"type":"object"}},{"type":"namespace","name":"acme","tools":[{"type":"function","name":"x_keyword_search","parameters":{"type":"object"}}]}]}`,
			`{"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}","status":"completed"}}`,
			`{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_ns","type":"function_call","call_id":"call_ns","name":"acme__x_keyword_search","arguments":"{}","status":"completed"}}`,
			`{"type":"response.output_item.done","output_index":2,"item":{"id":"fc_plain","type":"function_call","call_id":"call_plain","name":"x_keyword_search","arguments":"{}","status":"completed"}}`,
			`{"type":"response.output_item.done","output_index":3,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}],"status":"completed"}}`,
			`{"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","output":[{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}"},{"id":"fc_ns","type":"function_call","call_id":"call_ns","name":"acme__x_keyword_search","arguments":"{}"},{"id":"fc_plain","type":"function_call","call_id":"call_plain","name":"x_keyword_search","arguments":"{}"},{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}`),
		one("PreservesNormalizedCustomSameNameToolWithXSearch", "grok-4.5",
			`{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"},{"type":"custom","name":"x_keyword_search"}]}`,
			`{"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}","status":"completed"}}`,
			`{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_custom","type":"function_call","call_id":"call_custom","name":"x_keyword_search","arguments":"{}","status":"completed"}}`,
			`{"type":"response.output_item.done","output_index":2,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}],"status":"completed"}}`,
			`{"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","output":[{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}"},{"id":"fc_custom","type":"function_call","call_id":"call_custom","name":"x_keyword_search","arguments":"{}"},{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}`),
		{Name: "NormalizesReasoningTextEvents", Session: "sess-reasoning-text", Auths: map[string]map[string]string{"x": token("xai-token")},
			Turns: []*turn{{Auth: "x", Model: "grok-4.3", Response: "codex", Payload: `{"model":"grok-4.3","input":"hello"}`, Acts: reply1([]string{
				`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"in_progress","summary":[]}}`,
				`{"type":"response.content_part.added","sequence_number":2,"item_id":"rs_1","output_index":0,"content_index":0,"part":{"type":"reasoning_text","text":""}}`,
				`{"type":"response.reasoning_text.delta","sequence_number":3,"item_id":"rs_1","output_index":0,"content_index":0,"delta":"thinking"}`,
				`{"type":"response.reasoning_text.done","sequence_number":4,"item_id":"rs_1","output_index":0,"content_index":0,"text":"thinking"}`,
				`{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"thinking"}]}}`,
				`{"type":"response.completed","sequence_number":6,"response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"grok-4.3","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}`,
			})}}},
	}

	// The upstream repeats resp-real and echoes the previous_response_id it received.
	echo := `{"type":"response.completed","response":{"id":"resp-real","previous_response_id":"PREVIOUS_ID","output":[{"id":"rs_resp-real","type":"reasoning","status":"completed"}],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}`
	followUp := func(previous string) string {
		return fmt.Sprintf(`{"model":"grok-4.3","previous_response_id":%q,"input":[{"type":"function_call_output","call_id":"call-1","output":"ok"}]}`, previous)
	}
	out = append(out, &scenario{Name: "RewritesRepeatedResponseIDForDownstream", Session: "xai-id-map-session", Auths: map[string]map[string]string{"x": token("xai-token")},
		Turns: []*turn{
			{Auth: "x", Payload: `{"model":"grok-4.3","input":[{"type":"message","role":"user","content":"hello"}]}`, Acts: reply1([]string{echo})},
			{Auth: "x", Payload: followUp("resp-real"), Acts: reply1([]string{echo})},
			{Auth: "x", Payload: followUp("resp-real-xai-1"), Acts: reply1([]string{echo})},
		}})
	plain := `{"type":"response.completed","response":{"id":"resp-real","output":[{"id":"msg_resp-real","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}`
	out = append(out, &scenario{Name: "RewritesRepeatedResponseIDWithoutPreviousResponseID", Session: "xai-id-map-no-prev-session", Auths: map[string]map[string]string{"x": token("xai-token")},
		Turns: []*turn{
			{Auth: "x", Payload: `{"model":"grok-4.3","input":[{"type":"message","role":"user","content":"first"}]}`, Acts: reply1([]string{plain})},
			{Auth: "x", Payload: `{"model":"grok-4.3","input":[{"type":"message","role":"user","content":"second"}]}`, Acts: reply1([]string{plain})},
		}})

	byAuth := func(id string) string {
		return fmt.Sprintf(`{"type":"response.completed","response":{"id":%q,"output":[{"type":"message","id":%q,"role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}`, id, "msg-"+id)
	}
	second := `{"model":"grok-4.3","previous_response_id":"resp-auth-a","input":[{"type":"message","id":"user-2","role":"user","content":"second"}]}`
	out = append(out, &scenario{Name: "ReplaysTranscriptWhenAuthChanges", Session: "xai-auth-switch-session",
		Auths: map[string]map[string]string{"auth-a": token("token-a"), "auth-b": token("token-b"), "auth-c": token("token-c")},
		Turns: []*turn{
			{Auth: "auth-a", Payload: `{"model":"grok-4.3","input":[{"type":"message","id":"user-1","role":"user","content":"first"}]}`, Acts: reply1([]string{byAuth("resp-auth-a")})},
			{Auth: "auth-b", Payload: second, Reject: &reply{Status: 401, Body: "rejected\n"}},
			{Auth: "auth-c", Payload: second, Acts: reply1([]string{byAuth("resp-auth-c")})},
		}})

	out = append(out, &scenario{Name: "CompactionTriggerUsesHTTPCompactWithRecordedContext", Session: "xai-compaction-session", Auths: map[string]map[string]string{"x": token("xai-token")},
		Turns: []*turn{
			{Auth: "x", Payload: `{"model":"grok-4.3","stream":true,"input":[{"type":"message","id":"msg-1","role":"user","content":"first"}]}`,
				Acts: reply1([]string{`{"type":"response.completed","response":{"id":"resp-real","output":[{"type":"message","id":"out-1","role":"assistant","content":"first answer"}],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}`})},
			{Auth: "x", Payload: `{"model":"grok-4.3","stream":true,"previous_response_id":"resp-real-xai-1","input":[{"type":"compaction_trigger"}]}`,
				Compact: &reply{Status: 200, Body: `{"id":"resp_compact","model":"grok-4.3","output":[{"type":"compaction","encrypted_content":"GROKENC1"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}`}},
			{Auth: "x", Payload: `{"model":"grok-4.3","stream":true,"previous_response_id":"resp_compact","input":[{"type":"message","id":"msg-2","role":"user","content":"second"}]}`,
				Acts: reply1([]string{`{"type":"response.completed","response":{"id":"resp-after-compact","output":[{"type":"message","id":"out-2","role":"assistant","content":"second answer"}],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}`})},
		}})

	out = append(out,
		one("CompletesGenerateFalseWarmup", "grok-4.3", `{"model":"grok-4.3","generate":false,"input":[{"type":"message","role":"user","content":"warm up"}]}`,
			`{"type":"response.created","response":{"id":"resp-warmup-1","object":"response","status":"in_progress","output":[]}}`),
		&scenario{Name: "HandshakeFreeUsageExhaustedSetsRetryAfter", Session: "sess-free-usage", Auths: map[string]map[string]string{"x": token("xai-token")},
			Turns: []*turn{{Auth: "x", Payload: `{"model":"grok-4.3","input":"hello"}`,
				Reject: &reply{Status: 429, Body: `{"code":"subscription:free-usage-exhausted","error":"You've used all the included free usage for now."}`}}}},
		one("StopsOnBareErrorPayload", "grok-4.3", `{"model":"grok-4.3","input":"hello"}`,
			`{"error":{"message":"Request validation error: {\"code\":\"400\",\"error\":\"Argument not supported: instructions and previous_response_id together\"}","type":"api_error"}}`),
	)

	fresh := &reply{Status: 200, Body: `{"id":"resp_compact_fresh","output":[{"id":"cmp-1","type":"compaction","encrypted_content":"ZW5jcnlwdGVk"}]}`}
	for _, c := range []struct{ name, payload string }{
		{"xai-compaction-fresh-input-session", `{"model":"grok-4.3","stream":true,"previous_response_id":"resp-should-be-dropped","input":[{"type":"message","id":"msg-fresh-1","role":"user","content":"hello"},{"type":"compaction_trigger"}]}`},
		{"xai-compaction-fresh-prev-session", `{"model":"grok-4.3","stream":true,"previous_response_id":"resp-prev-123","input":[{"type":"compaction_trigger"}]}`},
		{"xai-compaction-fresh-empty-session", `{"model":"grok-4.3","stream":true,"input":[{"type":"compaction_trigger"}]}`},
	} {
		out = append(out, &scenario{Name: "CompactionTriggerFreshSessionFallback/" + c.name, Session: c.name, Auths: map[string]map[string]string{"x": token("xai-token")},
			Turns: []*turn{{Auth: "x", Payload: c.payload, Compact: fresh}}})
	}

	// KeepalivePingDuringUpload_WithSession: an upstream ping mid-turn is answered.
	out = append(out, &scenario{Name: "KeepalivePingDuringUpload_WithSession", Session: "xai-session-ping-test",
		Auths: map[string]map[string]string{"x": {"api_key": "xai-test-key", "base_url": base, "websockets": "true"}},
		Turns: []*turn{{Auth: "x", Model: "grok-4", Payload: `{"model":"grok-4","input":[{"type":"message","role":"user","content":"ping test"}]}`,
			Acts: []act{{Ping: "xai-session-ping", Send: []string{`{"type":"response.done","response":{"id":"resp-1","status":"completed","output":[]}}`}}}}}})

	// TestXAIWebsocketApplyPatchTransport, downstream WebSocket cases.
	for _, fold := range []bool{false, true} {
		for _, tc := range []struct{ snapshotOnly, sparseTerminal, lateName bool }{
			{false, false, false}, {true, false, false}, {true, true, false}, {true, false, true}, {true, true, true},
		} {
			name, args := "apply_patch", `{"input":"p\n中😀"}`
			if fold {
				name = "n"
				args = `{"name":"apply_patch","arguments":{"input":"p\n中😀"}}`
			}
			added := fmt.Sprintf(`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":%q,"arguments":""}}`, name)
			done := fmt.Sprintf(`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":%q,"arguments":%q}}`, name, args)
			events := []string{added}
			if !tc.snapshotOnly {
				events = append(events, fmt.Sprintf(`{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"a","delta":%q}`, args))
			} else {
				events[0] = fmt.Sprintf(`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":%q,"arguments":""}}`, name)
				done = fmt.Sprintf(`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":%q}}`, name)
			}
			events = append(events, fmt.Sprintf(`{"type":"response.function_call_arguments.done","item_id":"a","arguments":%q}`, args), done, `{"type":"response.completed","response":{"id":"r","output":[]}}`)
			if tc.lateName {
				events[0] = `{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a"}}`
			}
			if tc.sparseTerminal {
				events[len(events)-1] = fmt.Sprintf(`{"type":"response.completed","response":{"id":"r","output":[{"type":"function_call","id":"a","call_id":"c","name":%q}]}}`, name)
			}
			tools := `[{"type":"custom","name":"apply_patch"}]`
			if fold {
				declarations := []string{`{"type":"custom","name":"apply_patch"}`}
				for i := 0; i < 205; i++ {
					declarations = append(declarations, fmt.Sprintf(`{"type":"function","name":"lookup%d","parameters":{"type":"object"}}`, i))
				}
				tools = `[{"type":"namespace","name":"n","tools":[` + strings.Join(declarations, ",") + `]}]`
			}
			label := fmt.Sprintf("ApplyPatchTransport/fold=%v/snapshotOnly=%v/sparseTerminal=%v/lateName=%v", fold, tc.snapshotOnly, tc.sparseTerminal, tc.lateName)
			out = append(out, &scenario{Name: label, Session: "sess-" + label,
				Auths: map[string]map[string]string{"x": {"api_key": "test", "base_url": base, "websockets": "true"}},
				Turns: []*turn{{Auth: "x", Model: "grok-4", Payload: `{"input":[],"tools":` + tools + `}`, Acts: reply1(events)}}})
		}
	}
	out = append(out, &scenario{Name: "ApplyPatchIncompleteEOF", Session: "sess-apply-patch-eof",
		Auths: map[string]map[string]string{"x": {"api_key": "test", "base_url": base, "websockets": "true"}},
		Turns: []*turn{{Auth: "x", Model: "grok-4", Payload: `{"input":[],"tools":[{"type":"custom","name":"apply_patch"}]}`,
			Acts: []act{{Send: []string{`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}`}, Drop: true}}}}})

	// Usage: the WebSocket turn observes usage on completed and done, not incomplete. An
	// incomplete response only ends the turn while apply_patch is active (otherwise the
	// bridge drops what follows and Go's turn waits for its context).
	out = append(out,
		&scenario{Name: "usage_ignores_incomplete", Session: "sess-usage-incomplete",
			Auths: map[string]map[string]string{"x": {"api_key": "test", "base_url": base, "websockets": "true"}},
			Turns: []*turn{{Auth: "x", Model: "grok-4", Payload: `{"input":[],"tools":[{"type":"custom","name":"apply_patch"}]}`,
				Acts: reply1([]string{`{"type":"response.incomplete","response":{"id":"resp_u","status":"incomplete","model":"grok-4","output":[],"usage":{"input_tokens":4,"output_tokens":4,"total_tokens":8}}}`})}}},
		one("usage_on_done", "grok-4.3", `{"model":"grok-4.3","input":"hello"}`,
			`{"type":"response.done","response":{"id":"resp_d","status":"completed","model":"grok-4.3","output":[],"usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8}}}`),
	)
	return out
}
