"""Scratch: Responses API checks through cliproxy-rs against a local Chat Completions server.

Usage: local_tests.py <outdir>. The proxy listens on 127.0.0.1:8317 with client key
test-client-key and model alias "local". Prints one PASS/FAIL line per check and writes
each response to <outdir>.
"""
import json, sys, time, urllib.request, urllib.error

OUT = sys.argv[1]
BASE = "http://127.0.0.1:8317/v1"
HDRS = {"Authorization": "Bearer test-client-key", "Content-Type": "application/json"}
TOOLS = [{
    "type": "function", "name": "get_weather", "description": "Get the current weather for a city.",
    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
}]
results = []


def post(name, body, stream):
    t = time.time()
    req = urllib.request.Request(BASE + "/responses", json.dumps(body).encode(), HDRS)
    try:
        with urllib.request.urlopen(req, timeout=900) as r:
            raw = r.read().decode()
            status = r.status
    except urllib.error.HTTPError as e:
        raw, status = e.read().decode(), e.code
    except Exception as e:  # timeout, reset
        raw, status = repr(e), 0
    open(f"{OUT}/{name}.txt", "w").write(f"status {status}, {time.time() - t:.1f}s\n{raw}")
    if status != 200:
        return status, None, raw
    if not stream:
        return status, json.loads(raw), raw
    events = []
    for line in raw.splitlines():
        if line.startswith("data: ") and line[6:] != "[DONE]":
            try:
                events.append(json.loads(line[6:]))
            except ValueError:
                pass
    return status, events, raw


def record(name, ok, detail):
    results.append((name, ok))
    print(f"{'PASS' if ok else 'FAIL'} {name}: {detail}", flush=True)


def text_of(resp):
    return "".join(c.get("text", "") for o in resp.get("output", []) if o.get("type") == "message" for c in o.get("content", []))


# 1. Plain text, not streamed.
s, resp, raw = post("1-text", {"model": "local", "input": "Reply with one word: hello.", "stream": False}, False)
record("responses, not streamed", bool(resp and text_of(resp).strip()),
       f"status {s}, text {text_of(resp)[:80]!r}, usage {resp.get('usage') if resp else raw[:200]}")

# 2. Plain text, streamed.
s, ev, raw = post("2-text-stream", {"model": "local", "input": "Reply with one word: hello.", "stream": True}, True)
types = [e.get("type") for e in ev or []]
deltas = "".join(e.get("delta", "") for e in ev or [] if e.get("type") == "response.output_text.delta")
done = next((e for e in ev or [] if e.get("type") == "response.completed"), None)
record("responses, streamed", bool(done and deltas.strip()),
       f"status {s}, {len(types)} events, text {deltas[:80]!r}, completed usage {done['response'].get('usage') if done else None}")

# 3. A tool call, streamed, tool_choice auto as Codex sends it.
q = "What is the weather in Paris right now? Use the get_weather tool."
s, ev, raw = post("3-tool-call", {"model": "local", "input": q, "tools": TOOLS, "tool_choice": "auto", "stream": True}, True)
done = next((e for e in ev or [] if e.get("type") == "response.completed"), None)
calls = [o for o in (done or {}).get("response", {}).get("output", []) if o.get("type") == "function_call"]
args_ok = False
if calls:
    try:
        args_ok = "paris" in json.loads(calls[0]["arguments"]).get("city", "").lower()
    except (ValueError, AttributeError):
        pass
record("tool call (auto, streamed)", bool(calls) and calls[0].get("name") == "get_weather" and args_ok,
       f"status {s}, calls {[(c.get('name'), c.get('arguments')) for c in calls]}, raw {'' if s == 200 else raw[:300]}")

# 4. The tool result goes back; the model answers with it.
if calls:
    c = calls[0]
    inp = [{"role": "user", "content": q},
           {"type": "function_call", "call_id": c["call_id"], "name": c["name"], "arguments": c["arguments"]},
           {"type": "function_call_output", "call_id": c["call_id"], "output": json.dumps({"temp_c": 17, "sky": "sunny"})}]
    s, ev, raw = post("4-tool-result", {"model": "local", "input": inp, "tools": TOOLS, "stream": True}, True)
    deltas = "".join(e.get("delta", "") for e in ev or [] if e.get("type") == "response.output_text.delta")
    record("tool result turn", "17" in deltas, f"status {s}, text {deltas[:120]!r}")
else:
    record("tool result turn", False, "skipped: no tool call")

sys.exit(0 if all(ok for _, ok in results) else 1)
