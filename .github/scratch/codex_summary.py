"""Scratch: summarizes a `codex exec --json` run. Exits 1 unless the turn completed,
a shell command ran, and the final message contains the secret word."""
import json, sys

events = []
for line in open(sys.argv[1]):
    try:
        events.append(json.loads(line))
    except ValueError:
        pass
ran = completed = False
answer = ""
for e in events:
    t = e.get("type")
    item = e.get("item", {})
    if t == "item.completed":
        kind = item.get("type")
        if kind == "command_execution":
            ran = True
            print("command:", item.get("command"), "exit", item.get("exit_code"), repr(str(item.get("aggregated_output"))[:80]))
        elif kind == "agent_message":
            answer = item.get("text", "")
            print("message:", repr(answer[:200]))
        else:
            print(kind + ":", json.dumps(item)[:200])
    elif t == "turn.completed":
        completed = True
        print("turn.completed", e.get("usage"))
    elif t in ("turn.failed", "error"):
        print(t, json.dumps(e)[:400])
err = open(sys.argv[2]).read().strip().splitlines()
if err:
    print("stderr tail:", *err[-8:], sep="\n  ")
ok = completed and ran and "pelican" in answer.lower()
print("RESULT", "PASS" if ok else "FAIL", f"(completed={completed} ran_command={ran} word={'pelican' in answer.lower()})")
sys.exit(0 if ok else 1)
