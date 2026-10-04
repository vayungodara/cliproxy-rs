package main

import (
	"fmt"
	"strings"
)

const base = "http://UPSTREAM/v1"

func authAttrs(key string, extra ...string) map[string]string {
	attrs := map[string]string{"api_key": key, "base_url": base, "websockets": "true"}
	for i := 0; i+1 < len(extra); i += 2 {
		attrs[extra[i]] = extra[i+1]
	}
	return attrs
}

func user(text string) string {
	return fmt.Sprintf(`{"type":"message","role":"user","content":[{"type":"input_text","text":%q}]}`, text)
}

// request is a downstream Responses request; extra holds raw `"key":value` members.
func request(input []string, extra ...string) string {
	fields := []string{`"model":"grok-4.3"`, `"instructions":"be brief"`, `"input":[` + strings.Join(input, ",") + `]`, `"stream":true`}
	fields = append(fields, extra...)
	return "{" + strings.Join(fields, ",") + "}"
}

// turnEvents is a complete upstream response with one assistant message.
func turnEvents(id string, text string) []string {
	return []string{
		fmt.Sprintf(`{"type":"response.created","sequence_number":0,"response":{"id":%q,"object":"response","status":"in_progress","model":"grok-4.3","output":[]}}`, id),
		fmt.Sprintf(`{"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_%s","output_index":0,"content_index":0,"delta":%q}`, id, text),
		fmt.Sprintf(`{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"id":"msg_%s","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":%q,"annotations":[]}]}}`, id, text),
		fmt.Sprintf(`{"type":"response.completed","sequence_number":3,"response":{"id":%q,"object":"response","status":"completed","model":"grok-4.3","previous_response_id":null,"output":[],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}`, id),
	}
}

func reply1(events []string) []act { return []act{{Send: events}} }

const compactReply = `{"id":"cmp_1","object":"response.compaction","created_at":1700000000,"completed_at":1700000001,"model":"grok-4.3","output":[{"type":"compaction","id":"cmp_item_1","encrypted_content":"ENCRYPTED"}],"usage":{"input_tokens":30,"output_tokens":3,"total_tokens":33}}`

func scenarios() []*scenario {
	return append(ownScenarios(), goTestScenarios()...)
}

func ownScenarios() []*scenario {
	a := authAttrs("sk-fake-xai-a", "header:X-Trace", "trace-1")
	b := authAttrs("sk-fake-xai-b")
	return []*scenario{
		{Name: "basic_then_previous", Session: "sess-basic", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("hello")}), Acts: reply1(turnEvents("resp_1", "hi"))},
				{Auth: "auth-a", Payload: request([]string{user("again")}, `"previous_response_id":"resp_1"`), Acts: reply1(turnEvents("resp_2", "hi again"))},
				{Close: true},
			}},
		{Name: "repeated_response_id", Session: "sess-repeat", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("one")}), Acts: reply1(turnEvents("resp_r", "one"))},
				{Auth: "auth-a", Payload: request([]string{user("two")}, `"previous_response_id":"resp_r"`), Acts: reply1(turnEvents("resp_r", "two"))},
				{Auth: "auth-a", Payload: request([]string{user("three")}, `"previous_response_id":"resp_r-xai-1"`), Acts: reply1(turnEvents("resp_r3", "three"))},
			}},
		{Name: "target_change_replays_transcript", Session: "sess-target", Auths: map[string]map[string]string{"auth-a": a, "auth-b": b},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("first")}), Acts: reply1(turnEvents("resp_t1", "first"))},
				{Auth: "auth-b", Payload: request([]string{user("second")}, `"previous_response_id":"resp_t1"`), Acts: reply1(turnEvents("resp_t2", "second"))},
			}},
		{Name: "compaction_from_transcript", Session: "sess-compact", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("long context")}), Acts: reply1(turnEvents("resp_c1", "noted"))},
				{Auth: "auth-a", Payload: request([]string{`{"type":"compaction_trigger"}`}, `"previous_response_id":"resp_c1"`), Compact: &reply{Status: 200, Body: compactReply}},
				{Auth: "auth-a", Payload: `{"type":"response.append","model":"grok-4.3","input":[` + user("after compaction") + `],"stream":true}`, Acts: reply1(turnEvents("resp_c3", "continued"))},
			}},
		{Name: "compaction_fresh_input", Session: "sess-compact-fresh", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("context"), `{"type":"compaction_trigger"}`}), Compact: &reply{Status: 200, Body: compactReply}},
			}},
		{Name: "compaction_previous_only", Session: "sess-compact-prev", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{`{"type":"compaction_trigger"}`}, `"previous_response_id":"resp_elsewhere"`), Compact: &reply{Status: 200, Body: compactReply}},
			}},
		{Name: "compaction_empty_context", Session: "sess-compact-empty", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{`{"type":"compaction_trigger"}`})},
			}},
		{Name: "compaction_invalid_response", Session: "sess-compact-bad", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("x"), `{"type":"compaction_trigger"}`}), Compact: &reply{Status: 200, Body: `{"id":"cmp_2","output":[{"type":"message"}]}`}},
			}},
		{Name: "warmup_generate_false", Session: "sess-warmup", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("warm")}, `"generate":false`),
					Acts: reply1([]string{`{"type":"response.created","sequence_number":4,"response":{"id":"resp_w","object":"response","status":"in_progress","model":"grok-4.3"}}`})},
				{Auth: "auth-a", Payload: request([]string{user("real")}, `"previous_response_id":"resp_w"`), Acts: reply1(turnEvents("resp_w2", "real"))},
			}},
		{Name: "error_frames", Session: "sess-errors", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("a")}), Acts: reply1([]string{`{"type":"error","status":429,"error":{"type":"rate_limit","code":"free-usage-exhausted","message":"included free usage exhausted"}}`})},
				{Auth: "auth-a", Payload: request([]string{user("b")}), Acts: reply1([]string{`{"type":"error","status":403,"error":{"type":"auth","code":"bad-credentials","message":"Access token could not be validated"}}`})},
				{Auth: "auth-a", Payload: request([]string{user("c")}), Acts: reply1([]string{`{"status":403,"error":{"code":"bad-credentials","message":"Access token could not be validated"}}`})},
				{Auth: "auth-a", Payload: request([]string{user("d")}), Acts: reply1([]string{`{"error":{"code":"400","message":"bad request"}}`})},
				{Auth: "auth-a", Payload: request([]string{user("e")}), Acts: reply1([]string{`{"error":{"message":"{\"code\":\"400\"} Request validation error"}}`})},
				{Auth: "auth-a", Payload: request([]string{user("f")}), Acts: reply1([]string{`{"code":"free-usage-exhausted","error":"included free usage exhausted","status_code":429}`})},
			}},
		{Name: "handshake_rejected", Session: "sess-reject", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("a")}), Reject: &reply{Status: 403, Body: `{"code":"bad-credentials","error":"Access token could not be validated"}`}},
				{Auth: "auth-a", Payload: request([]string{user("b")}), Reject: &reply{Status: 429, Body: `{"code":"free-usage-exhausted","error":"included free usage exhausted"}`}},
				{Auth: "auth-a", Payload: request([]string{user("c")}), Reject: &reply{Status: 500, Body: ``}},
			}},
		{Name: "close_message_too_big", Session: "sess-1009", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("huge")}), Acts: []act{{Close: 1009, CloseText: "frame too large"}}},
				{Auth: "auth-a", Payload: request([]string{user("smaller")}), Acts: reply1(turnEvents("resp_after", "ok"))},
			}},
		{Name: "upstream_closes_mid_turn", Session: "sess-close", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("a")}), Acts: []act{{Send: turnEvents("resp_x", "partial")[:2], Close: 1011, CloseText: "internal"}}},
			}},
		{Name: "binary_frame", Session: "sess-binary", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("a")}), Acts: []act{{Send: turnEvents("resp_b", "x")[:1], Binary: true}}},
			}},
		{Name: "continuation_without_socket", Session: "sess-cont-none", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Continuation: true, Payload: request([]string{user("a")}, `"previous_response_id":"resp_gone"`)},
			}},
		{Name: "continuation_on_socket", Session: "sess-cont", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("a")}), Acts: reply1(turnEvents("resp_k1", "a"))},
				{Auth: "auth-a", Continuation: true, Payload: request([]string{user("b")}, `"previous_response_id":"resp_k1"`), Acts: reply1(turnEvents("resp_k2", "b"))},
			}},
		{Name: "websockets_disabled_continuation", Session: "sess-http", Auths: map[string]map[string]string{"auth-h": {"api_key": "sk-fake-xai-h", "base_url": base}},
			Turns: []*turn{
				{Auth: "auth-h", Continuation: true, Payload: request([]string{user("a")}, `"previous_response_id":"resp_1"`)},
			}},
		{Name: "reasoning_summary_events", Session: "sess-reasoning", Auths: map[string]map[string]string{"auth-a": a},
			Turns: []*turn{
				{Auth: "auth-a", Payload: request([]string{user("think")}, `"reasoning":{"effort":"high","summary":"auto"}`), Acts: reply1([]string{
					`{"type":"response.created","sequence_number":0,"response":{"id":"resp_z","object":"response","status":"in_progress","model":"grok-4.3","output":[]}}`,
					`{"type":"response.reasoning_summary_text.delta","sequence_number":1,"item_id":"rs_1","output_index":0,"summary_index":0,"delta":"thinking"}`,
					`{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"thinking"}],"encrypted_content":"ENC"}}`,
					`{"type":"response.completed","sequence_number":3,"response":{"id":"resp_z","object":"response","status":"completed","model":"grok-4.3","output":[],"usage":{"input_tokens":5,"output_tokens":9,"output_tokens_details":{"reasoning_tokens":7},"total_tokens":14}}}`,
				})},
			}},
	}
}
