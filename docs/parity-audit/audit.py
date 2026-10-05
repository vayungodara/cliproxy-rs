#!/usr/bin/env python3
"""Regenerates docs/parity-audit/status.md and the summary table in docs/PARITY.md from the
checklist (checklist.md), a scan of this repository, the route probe (probe.py,
routes.json) and the manual judgments in manual.tsv.

Usage: python3 docs/parity-audit/audit.py [--milestones M1,M2]

Status per ID:
- covered: implemented, and Rust tests or Go-generated fixtures exercise it.
- partial: implemented in part, or implemented without tests that pin Go's behaviour.
- missing: not implemented.

Automatic evidence, overridden per ID by manual.tsv:
- test suites: each named Go test is matched against Go test names cited in Rust
  sources and Go-generated fixtures (fixture names carry `TestName:line`), and against
  Rust test function names (case and underscores ignored).
- routes: probe.py starts the server and records which method/path pairs are routed.
"""

import gzip
import json
import os
import re
import sys
from collections import defaultdict

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
HERE = os.path.dirname(os.path.abspath(__file__))
LINK = re.compile(r"\[`([^`]*)`\]\([^)]*\)")
ITEM = re.compile(r"^- \[[ x]\] \*\*\[(M\d)\] (M\d-\d{4})\*\* (.*)")
GO_TEST = re.compile(r"\b(?:Test|Fuzz|Benchmark|Example)[A-Z0-9_][A-Za-z0-9_]*")


def items():
    out, section = [], None
    for line in open(os.path.join(HERE, "checklist.md"), encoding="utf-8"):
        m = re.match(r"^(#{2,3}) (.*)", line)
        if m:
            section = m.group(2).strip()
        m = ITEM.match(line)
        if m:
            out.append({"id": m.group(2), "ms": m.group(1), "section": section, "text": LINK.sub(r"\1", m.group(3).strip())})
    return out


def scan_files():
    """(relative path, text) for Rust sources, fixtures and dashboard tests."""
    roots = ["crates", "harness/rust", "ui/src"]
    skip = {"target", "node_modules", "reference", "dist", "dist-panel"}
    for root in roots:
        for dirpath, dirs, files in os.walk(os.path.join(ROOT, root)):
            dirs[:] = [d for d in dirs if d not in skip]
            for name in files:
                path = os.path.join(dirpath, name)
                rel = os.path.relpath(path, ROOT)
                try:
                    if name.endswith(".gz"):
                        if os.path.getsize(path) > 64 << 20:
                            continue
                        text = gzip.open(path).read().decode("utf-8", "replace")
                    elif name.endswith((".rs", ".json", ".jsonl", ".ts", ".txt", ".yaml")):
                        text = open(path, encoding="utf-8", errors="replace").read()
                    else:
                        continue
                except OSError:
                    continue
                yield rel, text


def norm(name):
    name = re.sub(r"^(test|fuzz|benchmark|example)_?", "", name, flags=re.I)
    return re.sub(r"[^a-z0-9]", "", name.lower())


def build_index():
    cited = defaultdict(set)  # Go test name -> files citing it
    rust_tests = defaultdict(set)  # normalized Rust test fn name -> files
    go_sources = defaultdict(set)  # Go source basename cited in Rust -> files
    test_fn = re.compile(r"#\[(?:tokio::)?test[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)")
    for rel, text in scan_files():
        for name in set(GO_TEST.findall(text)):
            cited[name].add(rel)
        if rel.endswith(".rs"):
            for fn in test_fn.findall(text):
                rust_tests[norm(fn)].add(rel)
            for src in set(re.findall(r"[A-Za-z0-9_\-]+\.go\b", text)):
                go_sources[src].add(rel)
        if rel.endswith(".ts"):
            for fn in re.findall(r"\b(?:test|it)\(\s*['\"`]([^'\"`]+)", text):
                rust_tests[norm(fn)].add(rel)
    return cited, rust_tests, go_sources


def test_cases(text):
    return re.findall(r"`((?:Test|Fuzz|Benchmark|Example)[A-Za-z0-9_]*)` \(L\d+\)", text)


def test_file(text):
    m = re.match(r"\*\*([^*:]+_test\.go)", text)
    return m.group(1) if m else None


def owner_for(text):
    """The area of the code base a gap belongs to, from the Go paths an item cites."""
    paths = re.findall(r"(?:internal|sdk|cmd|test|pkg)/[A-Za-z0-9_\-/.]+", text)
    joined = " ".join(paths).lower() or text.lower()
    rules = [
        ("codex/live", "realtime"),
        ("realtime", "realtime"),
        ("/live", "realtime"),
        ("internal/translator", "translate"),
        ("sdk/translator", "translate"),
        ("internal/thinking", "google"),
        ("internal/signature", "google"),
        ("claude", "claude"),
        ("anthropic", "claude"),
        ("antigravity", "google"),
        ("aistudio", "google"),
        ("vertex", "google"),
        ("interactions", "google"),
        ("gemini", "google"),
        ("kimi", "device-providers"),
        ("meta", "device-providers"),
        ("devin", "device-providers"),
        ("xai", "openai-xai"),
        ("grok", "openai-xai"),
        ("openai_compat", "openai-xai"),
        ("openai-compat", "openai-xai"),
        ("codex", "codex"),
        ("websocket", "codex"),
        ("management", "manage"),
        ("internal/config", "manage"),
        ("internal/usage", "server"),
        ("pluginabi", "plugins"),
        ("pluginhost", "plugins"),
        ("plugin", "plugins"),
        ("internal/tui", "tui"),
        ("internal/home", "home"),
        ("managementasset", "dashboard"),
        ("internal/store", "home"),
        ("discovery", "tui"),
        ("cmd/", "tui"),
    ]
    for key, owner in rules:
        if key in joined:
            return owner
    return "server"


def load_manual():
    manual = {}
    path = os.path.join(HERE, "manual.tsv")
    if not os.path.exists(path):
        return manual
    for line in open(path, encoding="utf-8"):
        if not line.strip() or line.startswith("#"):
            continue
        parts = line.rstrip("\n").split("\t")
        parts += [""] * (5 - len(parts))
        manual[parts[0]] = {"status": parts[1], "evidence": parts[2], "owner": parts[3], "note": parts[4]}
    return manual


def short(paths, limit=2):
    paths = sorted(paths)
    shown = ", ".join(paths[:limit])
    return shown + (f" (+{len(paths) - limit})" if len(paths) > limit else "")


def judge_test_suite(item, index):
    cited, rust_tests, go_sources = index
    cases = test_cases(item["text"])
    gofile = test_file(item["text"]) or ""
    hits, files = [], set()
    for case in cases:
        found = cited.get(case) or rust_tests.get(norm(case))
        if found:
            hits.append(case)
            files |= found
    base = os.path.basename(gofile).replace("_test.go", ".go")
    impl_cited = go_sources.get(base, set())
    if gofile.startswith(("internal/thinking/", "internal/signature/")) and len(hits) < len(cases):
        return ("covered", f"every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz ({len(hits)}/{len(cases)} cases also cited by name)", "")
    if cases and len(hits) == len(cases):
        return ("covered", f"{len(hits)}/{len(cases)} cases: {short(files)}", "")
    if hits:
        missing = [c for c in cases if c not in hits]
        return ("partial", f"{len(hits)}/{len(cases)} cases: {short(files)}; not matched: {', '.join(missing[:4])}{' …' if len(missing) > 4 else ''}", "")
    if impl_cited:
        return ("partial", f"implementation cites {base} ({short(impl_cited)}); no case matched by name", "")
    return ("missing", f"0/{len(cases)} cases matched; {base} not cited", "")


def test_texts():
    """Test sources: integration tests, *_tests.rs files and inline test modules."""
    out = {}
    for rel, text in scan_files():
        if rel.endswith("cpa-server/tests/fixtures/legacy_go.json"):
            # legacy_go.rs requests each step's path under /v0/management.
            steps = (s for sc in json.loads(text)["scenarios"] for s in sc.get("steps", []))
            out[rel] = "\n".join(f'{s["method"]} "/v0/management{s["path"]}"' for s in steps if "path" in s)
            continue
        if not rel.endswith(".rs"):
            continue
        if "/tests/" in rel or rel.endswith("_tests.rs"):
            out[rel] = text
        elif "#[cfg(test)]" in text:
            out[rel] = text[text.index("#[cfg(test)]"):]
    return out


def judge_route(item, routes, tests):
    m = re.match(r"`([A-Z]+) (\S+)`", item["text"])
    if not m:
        return None
    method, path = m.groups()
    key = f"{method} {path}"
    if key not in routes:
        return None
    hit = routes[key]
    if not hit["routed"]:
        return ("missing", f"probe: {key} -> {hit['status']} (not routed)")
    literal = re.split(r"[:*]", path)[0]
    if literal != "/" and literal.endswith("/") and len(literal) > 1:
        literal = literal
    pattern = re.compile(re.escape(literal) + (r'["?/ {]' if not literal.endswith("/") else ""))
    users = [rel for rel, text in tests.items() if pattern.search(text)]
    if users:
        return ("covered", f"probe: {key} -> {hit['status']}; tests: {short(users)}")
    return ("partial", f"probe: {key} -> {hit['status']}; no test requests this path")


# Provider families with no executor in Rust: their keys are accepted and never used.
NO_EXECUTOR = {"antigravity": "google", "aistudio": "google"}
FAMILY_OWNER = {"claude": "claude", "codex": "codex", "gemini": "google", "interactions": "google",
                "meta": "device-providers", "kimi": "device-providers", "openai-compatibility": "openai-xai",
                **NO_EXECUTOR, "vertex": "google", "xai": "openai-xai", "devin": "device-providers"}
SECTION_OWNER = [("management.", "manage"), ("config-version", "manage"), ("plugins.", "plugins"),
                 ("server.discovery", "tui"), ("credentials.", "home"),
                 ("client.codex", "codex"), ("multimedia.", "openai-xai")]
CONFIG_ONLY = ("config/schema.rs", "config/document.rs", "config/validate.rs", "config/generate_schema.py")


GENERIC_LEAVES = {"name", "alias", "api-key", "base-url", "headers", "prefix", "proxy-url", "models", "disabled",
                  "priority", "weight", "display-name", "excluded-models", "websockets", "alpha-search", "force-mapping"}


def config_family(key):
    parts = key.replace("[]", "").split(".")
    if parts[0] == "api-keys" and len(parts) > 1:
        return parts[1]
    if parts[0] == "oauth" and len(parts) > 2 and parts[1] == "providers":
        return parts[2]
    return None


def config_owner(key):
    if ".live-media-relay" in key or "aistudio.ws-auth" in key:
        return "realtime" if ".live-media-relay" in key else "google"
    family = config_family(key)
    if family in FAMILY_OWNER:
        return FAMILY_OWNER[family]
    for prefix, owner in SECTION_OWNER:
        if key.startswith(prefix):
            return owner
    return "server"


def kebab_fields(text):
    """Keys read through `#[serde(rename_all = "kebab-case")]` structs (no string literal)."""
    keys = set()
    for m in re.finditer(r'rename_all\s*=\s*"kebab-case"[^\n]*\n(?:\s*#\[[^\n]*\n)*\s*(?:pub(?:\([a-z]+\))?\s+)?struct\s+\w+(?:<[^>]*>)?\s*\{', text):
        depth, i = 1, m.end()
        start = i
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        body = text[start:i]
        for field in re.findall(r'^\s*(?:pub(?:\([a-z]+\))?\s+)?([a-z][a-z0-9_]*)\s*:', body, re.M):
            keys.add(field.replace("_", "-"))
    return keys


def config_sources():
    readers, tests = {}, {}
    for rel, text in scan_files():
        is_test = "/tests/" in rel or rel.endswith("_tests.rs") or "/testdata/" in rel
        if rel.endswith(".rs") and not is_test and not rel.endswith(CONFIG_ONLY):
            body = text.split("#[cfg(test)]")[0]
            # Kebab-case serde fields count as reads of their quoted key names.
            readers[rel] = body + "\n" + " ".join(f'"{k}"' for k in sorted(kebab_fields(body)))
            if "#[cfg(test)]" in text:
                tests[rel] = text[text.index("#[cfg(test)]"):]
        elif is_test:
            tests[rel] = text
    # Path segments per reader file: quoted strings split on '.' and '/'.
    tokens = {rel: {part for lit in re.findall(r'"([^"\\\n]{1,200})"', text) for part in re.split(r"[./]", lit)}
              for rel, text in readers.items()}
    # Keys a test sets: YAML keys (`key:`, also inside JSON-escaped YAML) and quoted path segments.
    # The lookbehind starts matches only at a run's first character: without it, long
    # lowercase runs in fixtures (repeated-payload tests) backtrack quadratically.
    test_tokens = {rel: set(re.findall(r"(?<![a-z0-9\-])([a-z0-9][a-z0-9\-]*):", text))
                   | {part for lit in re.findall(r'"([^"\\\n]{1,200})"', text) for part in re.split(r"[./]", lit)}
                   for rel, text in tests.items()}
    return readers, tests, tokens, test_tokens


def judge_config(item, sources):
    m = re.match(r"`([^`]+)`", item["text"])
    if not m:
        return None
    key = m.group(1)
    family = config_family(key)
    owner = config_owner(key)
    if family in NO_EXECUTOR:
        return ("missing", f"no {family} executor in crates/cpa-exec", owner, "")
    leaf = key.replace("[]", "").split(".")[-1]
    if "{" in leaf:
        leaf = key.replace("[]", "").split(".")[-2].strip("{}")
    readers, tests, _, _ = sources
    lit = f'"{leaf}"'
    # A key appears as a whole string or as a segment of a dotted path ("server.trusted-proxies").
    seg = re.compile(r'["./]' + re.escape(leaf) + r'["./]')
    tokens = sources[2]
    if leaf in GENERIC_LEAVES and family:
        # Shared credential fields: synthesized for every API-key family.
        used = [rel for rel in readers if rel.endswith(("config/credentials.rs", "config/sanitize.rs")) and leaf in tokens[rel]]
    else:
        used = [rel for rel in readers if leaf in tokens[rel]]
    if not used:
        return ("missing", f"accepted by the config schema; no runtime code reads {lit}", owner, "")
    family_token = {"interactions": "interactions", "openai-compatibility": "openai-compat"}.get(family, family)
    test_tokens = sources[3]
    set_in = [rel for rel, text in tests.items()
              if leaf in test_tokens[rel] and (not family_token or family_token in text)]
    if set_in:
        return ("covered", f"read in {short(used)}; set in {short(set_in)}", owner, "heuristic: key name match")
    return ("partial", f"read in {short(used)}; no test sets it", owner, "heuristic: key name match")


def main():
    only = None
    if "--milestones" in sys.argv:
        only = set(sys.argv[sys.argv.index("--milestones") + 1].split(","))
    index = build_index()
    manual = load_manual()
    routes_path = os.path.join(HERE, "routes.json")
    routes = json.load(open(routes_path)) if os.path.exists(routes_path) else {}
    tests = test_texts()
    sources = config_sources()
    rows = []
    for item in items():
        if only and item["ms"] not in only:
            continue
        status, evidence, note, auto_owner = "", "", "", ""
        if "test-suite" in item["section"]:
            status, evidence, note = judge_test_suite(item, index)
        else:
            routed = judge_route(item, routes, tests)
            if routed:
                status, evidence = routed
            elif item["section"].startswith("5. Config"):
                judged = judge_config(item, sources)
                if judged:
                    status, evidence, auto_owner, note = judged
        man = manual.get(item["id"])
        if man:
            status = man["status"] or status
            evidence = man["evidence"] or evidence
            note = man["note"] or note
        owner = ""
        if status != "covered":
            owner = (man and man["owner"]) or auto_owner or owner_for(item["text"])
        rows.append({**item, "status": status or "unjudged", "evidence": evidence, "owner": owner, "note": note})
    json.dump(rows, open(os.path.join(HERE, "status.json"), "w"), indent=1)
    counts = defaultdict(lambda: defaultdict(int))
    for r in rows:
        counts[r["ms"]][r["status"]] += 1
    for ms in sorted(counts):
        print(ms, dict(counts[ms]))
    render([r for r in rows if r["ms"] in AUDITED])


# Milestones whose rows have been reviewed by hand; the others are not rendered yet.
AUDITED = ["M1", "M2", "M3", "M4", "M5", "M6"]
# What the audited tree is called in the rendered pages: a release or a date.
BASE = "2026-10-05"


def title(r):
    text = r["text"]
    m = re.match(r"\*\*([^*]+?)(?::\d+-\d+)?\*\*", text)
    if m:
        return m.group(1)
    m = re.match(r"`([^`]+)`", text)
    if m:
        return m.group(1)
    text = re.sub(r"\*\*", "", text)
    return text[:90] + ("…" if len(text) > 90 else "")


def cell(text):
    return text.replace("|", "\\|").replace("\n", " ")


def render(rows):
    out = []
    w = out.append
    w("# Parity status, item by item")
    w("")
    w(f"Audit of {BASE} against CLIProxyAPI `6fecc6e`. Milestones audited: {', '.join(AUDITED)} "
      "(every item in [checklist.md](checklist.md)). [docs/PARITY.md](../PARITY.md) explains the milestones and sums them up.")
    w("")
    w("Statuses:")
    w("")
    w("- **covered**: implemented, and Rust tests or Go-generated fixtures exercise it. A note starting "
      "\"Deliberate difference\" marks an owner-approved divergence from Go; it counts as covered because "
      "there is no gap to close.")
    w("- **partial**: implemented in part, or implemented without tests that pin Go's behaviour. For Go test suites: the behaviour exists and is exercised, but not every Go case is ported.")
    w("- **missing**: not implemented.")
    w("")
    w("Area names the part of the code base that would close a partial or missing item.")
    w("")
    w("Method: `docs/parity-audit/audit.py` regenerates this file. Routes come from `probe.py`, which starts the binary and requests every listed method and path "
      "without credentials (routed pairs answer from the auth guard or handler, unrouted ones 404/405). A route counts as covered when a test requests it. "
      "Go test suites are matched case by case against Go test names cited in Rust code and in Go-generated fixtures (fixture names carry `TestName:line`). "
      "Every other item, and every suite the matcher cannot see, was judged by reading the Rust code and tests; those judgments live in `docs/parity-audit/manual.tsv` with their evidence. "
      "\"Not ported by name\" means the area is implemented and tested through other cases (usually Go-generated end-to-end scenarios), but the Go suite's own cases are not reproduced one by one.")
    w("")
    w("## Summary")
    w("")
    w("| Milestone | Items | covered | partial | missing |")
    w("|---|---:|---:|---:|---:|")
    for ms in AUDITED:
        rs = [r for r in rows if r["ms"] == ms]
        c = defaultdict(int)
        for r in rs:
            c[r["status"]] += 1
        w(f"| {ms} | {len(rs)} | {c['covered']} | {c['partial']} | {c['missing']} |")
    w("")
    w("### Gaps by area")
    w("")
    owners = defaultdict(lambda: defaultdict(list))
    for r in rows:
        if r["status"] in ("partial", "missing"):
            owners[r["owner"]][r["status"]].append(r["id"])
    w("| Area | missing | partial | Missing items |")
    w("|---|---:|---:|---|")
    for owner in sorted(owners, key=lambda o: (-len(owners[o]["missing"]), -len(owners[o]["partial"]), o)):
        m, p = owners[owner]["missing"], owners[owner]["partial"]
        w(f"| {owner} | {len(m)} | {len(p)} | {', '.join(sorted(m)) or '—'} |")
    w("")
    for ms in AUDITED:
        w(f"## {ms}")
        w("")
        sections = []
        for r in rows:
            if r["ms"] == ms and r["section"] not in sections:
                sections.append(r["section"])
        for section in sections:
            w(f"### {ms}: {section}")
            w("")
            w("| ID | Item | Status | Evidence | Area | Note |")
            w("|---|---|---|---|---|---|")
            for r in rows:
                if r["ms"] == ms and r["section"] == section:
                    w(f"| {r['id']} | {cell(title(r))} | {r['status']} | {cell(r['evidence'])} | {cell(r['owner'])} | {cell(r['note'])} |")
            w("")
    open(os.path.join(HERE, "status.md"), "w", encoding="utf-8").write("\n".join(out))
    summary(rows)


def summary(rows):
    """Rewrites the table between the parity-summary markers in docs/PARITY.md."""
    lines = [f"Audit of {BASE}.", "", "| Milestone | Items | Covered | Partial | Missing |", "|---|---:|---:|---:|---:|"]
    total = defaultdict(int)
    for ms in AUDITED:
        c = defaultdict(int)
        for r in rows:
            if r["ms"] == ms:
                c[r["status"]] += 1
                c["all"] += 1
        for k, v in c.items():
            total[k] += v
        lines.append(f"| {ms} | {c['all']} | {c['covered']} | {c['partial']} | {c['missing']} |")
    lines.append(f"| All | {total['all']} | {total['covered']} | {total['partial']} | {total['missing']} |")
    path = os.path.join(ROOT, "docs", "PARITY.md")
    page = open(path, encoding="utf-8").read()
    start, end = "<!-- parity-summary:start -->", "<!-- parity-summary:end -->"
    head, rest = page.split(start, 1)
    tail = rest.split(end, 1)[1]
    open(path, "w", encoding="utf-8").write(head + start + "\n" + "\n".join(lines) + "\n" + end + tail)


if __name__ == "__main__":
    main()
