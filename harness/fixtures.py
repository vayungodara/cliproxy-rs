"""Fake identities and scripted upstream responses. Nothing here is a real token."""
import copy
import json

MODEL = "claude-sonnet-4-6"
SESSION = "11111111-2222-4333-8444-555555555555"
CREDENTIAL = {
    "type": "claude", "access_token": "sk-ant-oat01-FAKE-LOCAL-ONLY",
    "refresh_token": "sk-ant-ort01-FAKE-LOCAL-ONLY", "id_token": "",
    "email": "local-fixture@example.invalid", "account_uuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
    "organization_uuid": "12345678-1234-4234-8234-123456789abc", "organization_name": "Local fixture",
    "claude_device_ids": ["0123456789abcdef" * 4],
    "expired": "2099-01-01T00:00:00Z", "last_refresh": "2026-10-01T00:00:00Z",
    "fixture_unknown_field": {"must_survive": True},
}


def encoded(value):
    return json.dumps(value, separators=(",", ":"))


MESSAGE = encoded({"id": "msg_fixture", "type": "message", "role": "assistant", "model": MODEL,
                   "content": [{"type": "text", "text": "local answer"}], "stop_reason": "end_turn",
                   "stop_sequence": None, "usage": {"input_tokens": 13, "output_tokens": 3}})


def event(kind, data):
    return f"event: {kind}\ndata: {encoded(data)}\n\n"


START = event("message_start", {"type": "message_start", "message": {
    "id": "msg_fixture", "type": "message", "role": "assistant", "model": MODEL, "content": [],
    "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 13, "output_tokens": 0}}})
DELTA = event("content_block_delta", {"type": "content_block_delta", "index": 0,
                                      "delta": {"type": "text_delta", "text": "local answer"}})
STOP = event("message_stop", {"type": "message_stop"})
SSE = (START + event("content_block_start", {"type": "content_block_start", "index": 0,
                                          "content_block": {"type": "text", "text": ""}})
       + DELTA + event("content_block_stop", {"type": "content_block_stop", "index": 0})
       + event("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                                 "usage": {"output_tokens": 3}}) + STOP)
MID_ERROR = START + event("error", {"type": "error", "error": {
    "type": "overloaded_error", "message": "scripted midstream failure"}})


def cases():
    body = {"model": MODEL, "max_tokens": 17, "messages": [{"role": "user", "content": "Local question"}]}
    base = {"method": "POST", "path": "/v1/messages", "body": encoded(body),
            "headers": [["Authorization", "Bearer fixture-client-key"], ["Content-Type", "application/json"],
                        ["Anthropic-Version", "2023-06-01"], ["X-Claude-Code-Session-Id", SESSION]],
            "script": {"status": 200, "body": MESSAGE}}
    out = []

    def add(name, **changes):
        case = copy.deepcopy(base)
        case.update(changes)
        case["name"] = name
        out.append(case)

    add("buffered")
    complex_body = {**body, "system": "Preserve caller instructions", "tools": [
        {"name": "fixture_lookup", "description": "Local tool", "input_schema": {"type": "object"}},
        {"name": "mcp__fixture__native", "description": "Native MCP tool", "input_schema": {"type": "object"}}],
        "metadata": {"user_id": "caller-owned-identity"}}
    add("cloak-tools-beta", body=encoded(complex_body), headers=base["headers"] + [
        ["Anthropic-Beta", "unknown-fixture-beta-2026-10-02"]])
    # Clean Go capture for fixture-client-key + fixture_lookup. Both proxies get
    # this identical scripted name, never a response generated from Rust output.
    alias = "mcp__poem_real__leisure_fixture_lookup"
    tool_reply = json.loads(MESSAGE)
    tool_reply.update(content=[{"type": "tool_use", "id": "tool_fixture", "name": alias,
                               "input": {"city": "Local"}}], stop_reason="tool_use")
    tool_body = {**complex_body, "tool_choice": {"type": "tool", "name": "fixture_lookup"}, "messages": [
        {"role": "user", "content": "Local question"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "previous_tool", "name": "fixture_lookup", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "previous_tool", "content": "Local result"}]}]}
    add("tool-roundtrip", body=encoded(tool_body), script={"status": 200, "body": encoded(tool_reply)})
    tool_sse = (START + event("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "tool_fixture", "name": alias, "input": {}}})
                + event("content_block_delta", {"type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": '{"city":"Local"}'}})
                + event("content_block_stop", {"type": "content_block_stop", "index": 0})
                + event("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": 3}}) + STOP)
    add("tool-stream-roundtrip", body=encoded({**tool_body, "stream": True}),
        script={"status": 200, "body": tool_sse, "sse": True, "fragment": 13})
    native_body = {**body, "system": "Native caller system", "metadata": {"user_id": encoded({
        "device_id": CREDENTIAL["claude_device_ids"][0], "account_uuid": CREDENTIAL["account_uuid"],
        "session_id": SESSION})}}
    # Go helps/claude_client_detection_test.go's four verified strong signals.
    add("native-signals", body=encoded(native_body), headers=base["headers"] + [
        ["User-Agent", "claude-cli/2.1.280 (external, cli)"], ["X-App", "cli"],
        ["Anthropic-Beta", "claude-code-20250219,interleaved-thinking-2025-05-14"]])
    streaming = encoded({**body, "stream": True})
    add("stream-fragmented", body=streaming,
        script={"status": 200, "body": SSE, "sse": True, "fragment": 7})
    add("stream-crlf", body=streaming,
        script={"status": 200, "body": SSE.replace("\n", "\r\n"), "sse": True, "fragment": 11})
    add("stream-bootstrap-error", body=streaming, script={"status": 429, "body": encoded({
        "type": "error", "error": {"type": "rate_limit_error", "message": "scripted bootstrap rejection"}}),
        "headers": [["Retry-After", "7"]]})
    add("stream-mid-error", body=streaming,
        script={"status": 200, "body": MID_ERROR, "sse": True, "fragment": 19})
    add("stream-truncated", body=streaming,
        script={"status": 200, "body": START + DELTA, "sse": True, "truncate": True})
    add("stream-after-stop", body=streaming,
        script={"status": 200, "body": SSE + event("ping", {"type": "ping", "after_stop": True}), "sse": True})
    add("client-disconnect", body=streaming, disconnect=True,
        script={"status": 200, "body": START, "sse": True, "slow_tail": True})
    add("count-tokens", path="/v1/messages/count_tokens", body=encoded({k: v for k, v in body.items() if k != "max_tokens"}),
        script={"status": 200, "body": '{"input_tokens":13}'})
    for status, kind in [(400, "invalid_request_error"), (401, "authentication_error"),
                         (429, "rate_limit_error"), (500, "api_error"), (503, "overloaded_error")]:
        add(f"upstream-{status}", script={"status": status, "body": encoded({"type": "error", "error": {
            "type": kind, "message": f"scripted {status}"}}), "headers": [
                ["Retry-After", "7"], ["Anthropic-Ratelimit-Unified-5h-Status", "rejected"],
                ["Anthropic-Ratelimit-Unified-5h-Reset", "1790942400"]]})
    add("empty-500", script={"status": 500, "body": ""})
    add("gzip-json", script={"status": 200, "body": MESSAGE, "encoding": "gzip"})
    add("gzip-unlabelled", script={"status": 200, "body": MESSAGE, "encoding": "gzip", "unlabelled": True})
    add("gzip-sse", body=streaming, script={"status": 200, "body": SSE, "sse": True, "encoding": "gzip"})
    add("deflate-json", script={"status": 200, "body": MESSAGE, "encoding": "deflate"})
    for name, headers, path in [
        ("auth-missing", [], "/v1/messages"),
        ("auth-wrong", [["Authorization", "Bearer wrong-fixture-key"]], "/v1/messages"),
        ("auth-query-first", [], "/v1/messages?key=wrong-fixture-key&key=fixture-client-key"),
        ("auth-query-encoded", [], "/v1/messages?%6bey=fixture-client-key"),
    ]:
        add(name, headers=headers, path=path)
    add("models-openai", method="GET", path="/v1/models", body="", headers=base["headers"][:1])
    add("models-anthropic", method="GET", path="/v1/models", body="", headers=base["headers"][:3])
    add("refresh-expired", expired=True, wait_refresh=True)
    add("continuity-two-turns", turns=2)
    add("tls-reconnect-two-turns", turns=2,
        script={"status": 200, "body": MESSAGE, "close_connection": True})
    add("disabled-credential", disabled=True)
    # Route surface beyond Messages. Cases marked no_upstream must not reach a provider.
    openai_headers = [["Authorization", "Bearer fixture-client-key"], ["Content-Type", "application/json"]]
    chat = {"model": MODEL, "max_tokens": 17, "messages": [{"role": "user", "content": "Local question"}]}
    sse = {"status": 200, "body": SSE, "sse": True}
    add("chat-completions", path="/v1/chat/completions", body=encoded(chat), headers=openai_headers, script=sse)
    add("chat-completions-stream", path="/v1/chat/completions", body=encoded({**chat, "stream": True}),
        headers=openai_headers, script=sse)
    add("chat-upstream-429", path="/v1/chat/completions", body=encoded(chat), headers=openai_headers,
        script={"status": 429, "body": encoded({"type": "error", "error": {
            "type": "rate_limit_error", "message": "scripted 429"}})})
    add("completions", path="/v1/completions", headers=openai_headers, script=sse,
        body=encoded({"model": MODEL, "prompt": "Local question", "max_tokens": 17}))
    add("completions-stream", path="/v1/completions", headers=openai_headers, script=sse,
        body=encoded({"model": MODEL, "prompt": "Local question", "max_tokens": 17, "stream": True}))
    add("unknown-model", body=encoded({**body, "model": "no-such-model"}), no_upstream=True)
    add("chat-unknown-model", path="/v1/chat/completions", headers=openai_headers, no_upstream=True,
        body=encoded({**chat, "model": "no-such-model"}))
    add("compact-claude", path="/v1/responses/compact", headers=openai_headers, no_upstream=True,
        body=encoded({"model": MODEL, "input": "Local question"}))
    add("compact-stream-rejected", path="/v1/responses/compact", headers=openai_headers, no_upstream=True,
        body=encoded({"model": MODEL, "input": "Local question", "stream": True}))
    add("models-anthropic-ua", method="GET", path="/v1/models", body="", no_upstream=True,
        headers=base["headers"][:1] + [["User-Agent", "claude-cli/2.1.280 (external, cli)"]])
    add("models-gemini", method="GET", path="/v1beta/models", body="", headers=base["headers"][:1], no_upstream=True)
    add("models-gemini-get", method="GET", path=f"/v1beta/models/{MODEL}", body="", headers=base["headers"][:1],
        no_upstream=True)
    add("models-gemini-get-missing", method="GET", path="/v1beta/models/no-such-model", body="",
        headers=base["headers"][:1], no_upstream=True)
    add("gemini-unknown-method", path=f"/v1beta/models/{MODEL}:foo", body="{}", headers=openai_headers,
        no_upstream=True)
    add("gemini-bad-action", path=f"/v1beta/models/{MODEL}", body="{}", headers=openai_headers, no_upstream=True)
    add("interactions-invalid", path="/v1beta/interactions", body="{}", headers=openai_headers, no_upstream=True)
    add("interactions-bad-stream", path="/v1beta/interactions", headers=openai_headers, no_upstream=True,
        body=encoded({"model": MODEL, "stream": "yes"}))
    add("healthz-head", method="HEAD", path="/healthz", body="", headers=[], no_upstream=True)
    # gin without HandleMethodNotAllowed: wrong methods and unregistered HEADs are NoRoute 404s.
    add("wrong-method-chat", method="GET", path="/v1/chat/completions", body="", headers=openai_headers,
        no_upstream=True)
    add("models-head", method="HEAD", path="/v1/models", body="", headers=openai_headers, no_upstream=True)
    add("healthz-post", method="POST", path="/healthz", body="", headers=[], no_upstream=True)
    add("unknown-path", method="GET", path="/v2/nothing", body="", headers=[], no_upstream=True)
    add("root", method="GET", path="/", body="", headers=[], no_upstream=True)
    add("dd-model-messages", body=encoded({**body, "model": "claude-fable-5-dd-6-4-tennos-edualc"}))
    return out


TOKEN_REPLY = encoded({"access_token": "sk-ant-oat01-FAKE-ROTATED-LOCAL-ONLY",
                       "refresh_token": "sk-ant-ort01-FAKE-ROTATED-LOCAL-ONLY",
                       "token_type": "Bearer", "expires_in": 7200,
                       "scope": "user:profile user:inference"})
PROFILE_REPLY = encoded({"account": {"uuid": CREDENTIAL["account_uuid"], "email": CREDENTIAL["email"]},
                         "organization": {"uuid": CREDENTIAL["organization_uuid"], "name": "Local fixture"}})
