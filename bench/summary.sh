#!/usr/bin/env bash
# Prints the docs/BENCHMARKS.md tables from a bench/run.sh results.jsonl: the median of
# the rounds for each server and scenario.
#
#   bench/summary.sh <output dir>/results.jsonl
set -euo pipefail

jq -rs '
  def median: sort | if length == 0 then null else .[(length - 1) / 2 | floor] end;
  def mb: if . == null then "" else (. / 1024 * 10 | round / 10 | tostring) end;
  def r1: if . == null then "" else (. * 10 | round / 10 | tostring) end;
  def rows($s): map(select(.scenario == $s));
  def stat($rows; $server; f): $rows | map(select(.server == $server) | f) | median;
  def line($s; $server):
    rows($s) as $r
    | [$server,
       (stat($r; $server; .load.rps) | floor | tostring),
       (stat($r; $server; .load.p50_ms) | r1),
       (stat($r; $server; .load.p99_ms) | r1),
       (stat($r; $server; .cpu_s * 1000 / .load.ok) | . * 100 | round / 100 | tostring),
       (stat($r; $server; .hwm_kb) | mb),
       (stat($r; $server; .rss_settled_kb) | mb),
       (stat($r; $server; .load.bad) | tostring)]
    | "| " + join(" | ") + " |";
  . as $all
  | "Rounds: \(map(.round) | max)",
    "",
    "| Idle | Startup (ms) | RSS after 15 s (MB) |",
    "| --- | --- | --- |",
    (["rust", "go"][] as $server
      | "| \($server) | \(stat($all | rows("idle"); $server; .startup_ms)) | \(stat($all | rows("idle"); $server; .idle_rss_kb) | mb) |"),
    (["chat", "chat-stream", "chat-stream-slow", "messages-stream"][] as $s
      | select(($all | rows($s) | length) > 0)
      | "",
        "\($s)",
        "",
        "| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |",
        "| --- | --- | --- | --- | --- | --- | --- | --- |",
        ($all | line($s; "rust")),
        ($all | line($s; "go")))
' "$1"
