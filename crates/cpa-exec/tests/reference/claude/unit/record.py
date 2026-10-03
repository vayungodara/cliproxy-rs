#!/usr/bin/env python3
"""Records Go's own Claude unit cases for cliproxy-rs (see zz_rsfix_claude_test.go).

Usage: record.py <CLIProxyAPI checkout at 6fecc6e> <output.json>

Copies the checkout's tracked files to a scratch directory under /tmp, rewrites call
sites in the listed Go test files to the recording wrappers, runs those tests and the
writer, and stores the records grouped by Go test. The checkout is never modified.
Nothing contacts the network beyond what the Go tests themselves do (local servers).
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
PKG = "internal/runtime/executor"

# Go test files whose call sites are rewritten, relative to PKG.
FILES = [
    "apply_patch_integration_test.go",
    "claude_cloaked_cache_repro_test.go",
    "claude_executor_auth_race_test.go",
    "claude_executor_auth_test.go",
    "claude_executor_beta_passthrough_test.go",
    "claude_executor_beta_policy_test.go",
    "claude_executor_cloaking_display_test.go",
    "claude_executor_diagnostics_test.go",
    "claude_executor_fable_ratelimit_test.go",
    "claude_executor_fast_error_test.go",
    "claude_executor_native_helper_test.go",
    "claude_executor_ratelimit_test.go",
    "claude_executor_request_remap_test.go",
    "claude_executor_stream_terminal_test.go",
    "claude_executor_subagent_ttl_regression_test.go",
    "claude_executor_test.go",
    "claude_executor_thinking_signature_test.go",
    "claude_executor_wire_casing_test.go",
    "claude_fingerprint_policy_test.go",
    "claude_issue_6120_test.go",
    "claude_issue_6193_test.go",
    "claude_messages_passthrough_test.go",
    "claude_mid_system_model_test.go",
    "claude_signing_test.go",
    "claude_thinking_replay_test.go",
]

# Call-site rewrites: function name -> wrapper. `\b` plus the opening parenthesis keeps
# longer names (for example the ...Legacy variant) intact.
CALLS = {
    "remapOAuthToolNamesWithOptions": "rsfixRemapWithOptions",
    "remapOAuthToolNames": "rsfixRemap",
    "remapOAuthToolNamesWithOptionsLegacy": "rsfixRemapLegacy",
    "remapOAuthToolNamesWithBatchedEdits": "rsfixRemapBatched",
    "prepareClaudeOAuthToolNamesForUpstream": "rsfixRemapWithOptions",
    "reverseRemapOAuthToolNames": "rsfixRestore",
    "restoreClaudeOAuthToolNamesFromResponse": "rsfixRestore",
    "reverseRemapOAuthToolNamesFromStreamLine": "rsfixRestoreLine",
    "restoreClaudeOAuthToolNamesFromStreamLine": "rsfixRestoreLine",
    "parseClaudeMCPAlias": "rsfixParseAlias",
}


# Executor entry points: `<receiver>.Execute(` -> `rsfixExecute(<receiver>, `. The
# wrappers record only *ClaudeExecutor receivers (manager.Execute is the conductor).
METHODS = {"Execute": "rsfixExecute", "ExecuteStream": "rsfixExecuteStream", "CountTokens": "rsfixCountTokens"}
RECEIVERS = ("executor", "exec", "ex", "e", "claudeExec", "claudeExecutor")


def rewrite(source: str) -> str:
    for name, wrapper in CALLS.items():
        source = re.sub(r"(?<![\w.])" + name + r"\(", wrapper + "(", source)
    receivers = "|".join(RECEIVERS)
    for method, wrapper in METHODS.items():
        source = re.sub(r"(?<![\w.])(" + receivers + r")\." + method + r"\(", wrapper + r"(\1, ", source)
    return source


def main() -> None:
    checkout, output = sys.argv[1], os.path.abspath(sys.argv[2])
    scratch = tempfile.mkdtemp(prefix="cpa-rsfix-", dir="/tmp")
    archive = subprocess.run(["git", "-C", checkout, "archive", "HEAD"], check=True, capture_output=True).stdout
    subprocess.run(["tar", "-x", "-C", scratch], input=archive, check=True)
    tests, owner = [], {}
    for name in FILES:
        path = os.path.join(scratch, PKG, name)
        with open(path, encoding="utf-8") as f:
            source = f.read()
        found = re.findall(r"^func ((?:Test|Fuzz)\w+)\(", source, re.M)
        tests += found
        owner.update({test: name for test in found})
        with open(path, "w", encoding="utf-8") as f:
            f.write(rewrite(source))
    with open(os.path.join(HERE, "zz_rsfix_claude_test.go"), encoding="utf-8") as f:
        recorder = f.read()
    with open(os.path.join(scratch, PKG, "zz_rsfix_claude_test.go"), "w", encoding="utf-8") as f:
        f.write(recorder)
    raw = os.path.join(scratch, "records.json")
    only = os.environ.get("RSFIX_ONLY")
    selected = [t for t in tests if not only or re.search(only, t)]
    pattern = "^(" + "|".join(selected + ["TestZZZRSFixWrite"]) + ")$"
    env = dict(os.environ, RSFIX_OUT=raw, GOFLAGS="-mod=mod")
    run = subprocess.run(
        ["go", "test", "-count=1", "-parallel", "1", "-run", pattern, "./" + PKG + "/"],
        cwd=scratch, env=env, capture_output=True, text=True,
    )
    failures = re.findall(r"^--- FAIL: (\S+)", run.stdout, re.M)
    if failures:
        with open(output + ".go-test.log", "w", encoding="utf-8") as f:
            f.write(run.stdout + run.stderr)
    if not os.path.exists(raw):
        sys.stderr.write(run.stdout[-4000:] + run.stderr[-4000:])
        sys.exit("no records written; scratch kept at " + scratch)
    with open(raw, encoding="utf-8") as f:
        records = json.load(f)
    grouped = {}
    for record in records:
        test = record.pop("test") or "(helper)"
        grouped.setdefault(owner.get(test, "(other)"), {}).setdefault(test, []).append(record)
    result = {
        "source": "CLIProxyAPI 6fecc6e, Go's own tests in " + PKG,
        "go_failures": failures,
        "files": grouped,
    }
    with open(output, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=1, ensure_ascii=False, sort_keys=True)
        f.write("\n")
    shutil.rmtree(scratch)
    count = sum(len(tests) for tests in grouped.values())
    print(f"{len(records)} records from {count} tests; Go failures: {failures}")


if __name__ == "__main__":
    main()
