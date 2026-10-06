#!/usr/bin/env bash
# Performance gates against bench/budgets.txt, for CI on GitHub-hosted runners.
#
#   bench/gate.sh size <target> <binary> [archive]   bytes of the binary, its .text and
#                                                    .rodata (ELF), the release archive,
#                                                    and duplicate crates (Linux only)
#   bench/gate.sh image <docker image>               compressed image bytes
#   bench/gate.sh idle <os> <binary> [auth files]    idle cost over a window of no requests
#
# <os> is linux, macos or windows. Each measured value is printed and compared with the
# line of the same key in bench/budgets.txt: `<key> <value> <tolerance %>`. A key with no
# line is printed and not gated, so a new target starts reporting before it has a budget.
# Gates count events and bytes, never elapsed time: shared runners vary by about 15%.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BUDGETS=$HERE/budgets.txt
failed=0

check() { # key measured
  local key=$1 got=$2 line value tol limit
  # A failed measurement must not read as 0 (bash treats an empty value as 0).
  if [[ ! $got =~ ^[0-9]+$ ]]; then
    printf '%-44s %12s   NOT MEASURED\n' "$key" "${got:-<empty>}"
    echo "::error title=Performance budget::$key was not measured (got '${got}')."
    failed=1
    return 0
  fi
  line=$(sed 's/#.*//' "$BUDGETS" | awk -v k="$key" '$1 == k {print $2, ($3 == "" ? 0 : $3)}')
  if [[ -z $line ]]; then
    printf '%-44s %12s   (no budget)\n' "$key" "$got"
    return
  fi
  read -r value tol <<<"$line"
  value=${value//_/}
  limit=$(awk -v v="$value" -v t="$tol" 'BEGIN {printf "%d", v * (1 + t / 100)}')
  if (( got > limit )); then
    printf '%-44s %12s > %s (budget %s +%s%%)  OVER\n' "$key" "$got" "$limit" "$value" "$tol"
    echo "::error title=Performance budget::$key is $got, over $limit (bench/budgets.txt: $value +$tol%). Find the cause, or raise the budget with a dated line that names the feature and its cost."
    failed=1
  else
    printf '%-44s %12s <= %s\n' "$key" "$got" "$limit"
  fi
  [[ -n ${GITHUB_STEP_SUMMARY:-} ]] && echo "| \`$key\` | $got | $limit |" >> "$GITHUB_STEP_SUMMARY"
  return 0
}

bytes() { wc -c < "$1" | tr -d ' '; }

summary_header() {
  [[ -n ${GITHUB_STEP_SUMMARY:-} ]] && printf '\n### %s\n\n| Budget | Measured | Ceiling |\n| --- | --- | --- |\n' "$1" >> "$GITHUB_STEP_SUMMARY"
  return 0
}

size_gate() { # target binary [archive]
  local target=$1 bin=$2 archive=${3:-}
  summary_header "Size, $target"
  check "size.$target.binary_bytes" "$(bytes "$bin")"
  # Required for Linux targets: a missing section row fails instead of being skipped.
  if [[ $target == *-linux-* ]]; then
    check "size.$target.text_bytes" "$(size -A "$bin" | awk '$1 == ".text" {print $2}')"
    check "size.$target.rodata_bytes" "$(size -A "$bin" | awk '$1 == ".rodata" {print $2}')"
  fi
  [[ -n $archive ]] && check "size.$target.archive_bytes" "$(bytes "$archive")"
  if [[ $(uname -s) == Linux ]] && command -v cargo > /dev/null; then
    # Crates linked into the binary in more than one version (proc macros and build
    # scripts excluded).
    local dups
    dups=$(cargo tree -d -e normal,no-proc-macro --prefix depth --target "$target" --locked -p cliproxy |
      awk '/^0/ {sub(/^0/, ""); print $1, $2}' | sort -u |
      awk '{n[$1]++; v[$1] = v[$1] " " $2} END {for (k in n) if (n[k] > 1) print k v[k]}' | sort)
    echo "$dups" | sed 's/^/  duplicate: /'
    check "deps.$target.duplicate_crates" "$(grep -c . <<<"$dups" || true)"
  fi
}

image_gate() { # image
  summary_header "Docker image"
  check "size.docker.image_gzip_bytes" "$(docker save "$1" | gzip -c | wc -c | tr -d ' ')"
}

idle_gate() { # os binary [files]
  local os=$1 bin=$2 files=${3:-1}
  summary_header "Idle, $os, $files auth file(s)"
  case $os in
    linux)
      local json
      json=$("$HERE/idle.sh" "$bin" "$files" "${IDLE_SECONDS:-30}")
      echo "$json"
      for metric in wakeups cpu_ticks threads rss_kb; do
        check "idle.linux.$files.$metric" "$(jq -r ".$metric" <<<"$json" | tr -d '\r')"
      done
      ;;
    macos | windows)
      local json key metrics
      json=$("$HERE/idle-other.sh" "$os" "$bin" "$files" "${IDLE_SECONDS:-60}")
      echo "$json"
      # Apple silicon and Intel idle at different sizes, so each has its own budgets.
      if [[ $os == macos ]]; then
        key=macos-$(uname -m)
        metrics="cpu_ms idle_wakeups threads rss_kb"
      else
        key=windows
        metrics="cpu_ms threads rss_kb"
      fi
      for metric in $metrics; do
        # jq on Windows ends its lines with CRLF; a missing key prints "null", which
        # check() rejects.
        check "idle.$key.$files.$metric" "$(jq -r ".$metric" <<<"$json" | tr -d '\r')"
      done
      ;;
    *) echo "unknown os $os" >&2; exit 2 ;;
  esac
}

case ${1:-} in
  size) shift; size_gate "$@" ;;
  image) shift; image_gate "$@" ;;
  idle) shift; idle_gate "$@" ;;
  *) sed -n '2,13p' "$0"; exit 2 ;;
esac
exit "$failed"
