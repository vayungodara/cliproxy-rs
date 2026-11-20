#!/usr/bin/env python3
"""Checks README.md's links into this repository (files and heading anchors), its in-page
anchors, and the relative links in the given docs. Usage: linkcheck.py <repo> [doc ...]"""
import os, re, sys
root = sys.argv[1]
def slugs(path):
    out, seen = set(), {}
    fence = False
    for line in open(path, encoding="utf-8"):
        if line.startswith("```"): fence = not fence
        m = None if fence else re.match(r"#{1,6}\s+(.*?)\s*$", line)
        if not m: continue
        t = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", m.group(1))
        t = re.sub(r"[`*]", "", t).lower()
        s = re.sub(r"[^\w\- ]", "", t).replace(" ", "-")
        n = seen.get(s, 0); seen[s] = n + 1
        out.add(s if n == 0 else f"{s}-{n}")
    return out
bad = checked = 0
def check(src, target, base):
    global bad, checked
    path, _, frag = target.partition("#"); path = path.split("?")[0]
    full = os.path.normpath(os.path.join(base, path)) if path else src
    checked += 1
    if not os.path.exists(full):
        bad += 1; print("MISSING FILE", src, target); return
    if frag and full.endswith(".md") and frag not in slugs(full):
        bad += 1; print("MISSING ANCHOR", src, target)
readme = os.path.join(root, "README.md")
text = open(readme, encoding="utf-8").read()
urls = re.findall(r'\]\(([^)\s]+)\)', text) + re.findall(r'(?:href|src|srcset)="([^"]+)"', text)
for u in urls:
    m = re.match(r"https://github\.com/vayungodara/cliproxy-rs/(?:blob|raw|tree)/master/(.+)", u)
    if m: check(readme, m.group(1), root)
    elif u.startswith("#"): check(readme, u, root)
    elif not re.match(r"https?://|mailto:", u):
        bad += 1; print("RELATIVE LINK IN README", u)
for doc in sys.argv[2:]:
    d = os.path.join(root, doc)
    t = open(d, encoding="utf-8").read()
    for u in re.findall(r'\]\(([^)\s]+)\)', t):
        if not re.match(r"https?://|mailto:", u): check(d, u, os.path.dirname(d))
print(f"checked {checked} links, {bad} problems")
