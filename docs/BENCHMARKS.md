# Benchmarks

Binary size, memory and throughput of cliproxy-rs and CLIProxyAPI on the same configuration, with a local fake upstream. The numbers come from one small machine and a synthetic load, so use them to compare the two servers with each other. They say little about how much traffic either one can carry on bigger hardware.

## Setup

Measured on 2026-10-03.

- cliproxy-rs 0.1.0, the launch build, release profile (thin LTO, one codegen unit, stripped), built with rustc 1.99.0.
- CLIProxyAPI v8.0.10 (commit `6fecc6e`), the official `linux_amd64` release binary, built with Go 1.26.4.
- A virtual machine with 2 vCPUs (Intel Xeon at 2.60 GHz) and 3.9 GB of memory, running Debian 12 with Linux 6.1. It is a shared machine, and the same binary measures differently from run to run, by up to about 15%; compare the two servers within one run.
- Both servers run with the same `config.yaml`: one OpenAI-compatible provider that points at the fake upstream, one client key and an empty credential folder, started with `-local-model`. Each scenario starts a fresh server process.
- The server is pinned to CPU 0. The fake upstream and the load generator share CPU 1.
- The whole run happens in a network namespace with only a loopback interface. Go tries to download its management panel and an Antigravity version file at start; both fail at once there. On a machine with network access, Go's idle memory was about 10 MB higher after those downloads.
- Both servers write one access-log line per request to standard output (redirected to a file).

The scripts are in [`bench/`](../bench): `upstream/` is the fake OpenAI-compatible upstream, `load/` the load generator, `run.sh` runs every scenario three times and `summary.sh` prints the median of the three rounds. Both helpers use only the Go standard library. The raw results are in [`bench/results/`](../bench/results), one file per run.

```sh
bench/run.sh target/release/cliproxy /path/to/cli-proxy-api /tmp/bench 3
bench/summary.sh /tmp/bench/results.jsonl
```

## Scenarios

- Idle: start, wait 15 seconds, read the resident memory. Startup is the time from launch to the first answered request.
- chat: `POST /v1/chat/completions`, not streamed, 32 concurrent clients for 20 seconds. The request is 1.7 KB; the upstream answers at once with a 0.6 KB completion.
- chat-stream: the same request with `"stream": true`. The upstream sends 22 SSE chunks with no pause between them.
- chat-stream-slow: 256 concurrent streams, with 50 ms between chunks, so each response takes about 1.1 seconds. This is closest to many coding agents waiting on a model, and it shows the memory each open stream costs.
- messages-stream: `POST /v1/messages` in Anthropic format, streamed. Both servers translate it to OpenAI chat for the upstream and translate the stream back.

A response counts only if its status is 200 and the body has the expected end marker. Latency is measured at the client. CPU time per request is the server's user and system time during the load, divided by the completed requests. Peak memory is the process's `VmHWM`; the second memory figure is `VmRSS` 10 seconds after the load stops.

## Results

Median of three rounds, 2026-10-03, cliproxy-rs 0.1.0.

### Idle

| Server | Startup (ms) | RSS after 15 s (MB) |
| --- | --- | --- |
| cliproxy-rs | 17 | 17.3 |
| Go | 204 | 44.7 |

### chat

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 1168 | 25.3 | 51.9 | 0.85 | 25.2 | 25.2 | 0 |
| Go | 1568 | 18.9 | 50.1 | 0.62 | 57.8 | 56.9 | 0 |

### chat-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 833 | 34.9 | 75.1 | 1.19 | 26.2 | 26.2 | 0 |
| Go | 916 | 34.4 | 69.6 | 1.08 | 57.8 | 56.7 | 0 |

### chat-stream-slow

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 238 | 1021.4 | 1414.5 | 1.95 | 49.7 | 49.2 | 0 |
| Go | 235 | 1039.7 | 1272.2 | 2.9 | 103.8 | 103.8 | 0 |

### messages-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 793 | 37.5 | 68.2 | 1.25 | 26.9 | 26.6 | 0 |
| Go | 564 | 54.5 | 115.4 | 1.75 | 79.3 | 77.5 | 0 |

Startup varies a lot on this machine, mostly in the first round after a binary is copied in. cliproxy-rs took 15, 17 and 46 ms; Go took 104, 204 and 453 ms in this run, and 45 to 98 ms in quieter runs earlier the same day.

## Binary size

Linux x86_64. The cliproxy-rs binary includes its dashboard; the Go binary does not, because Go downloads its panel separately.

| | cliproxy-rs 0.1.0 | CLIProxyAPI v8.0.10 release |
| --- | --- | --- |
| Binary | 35.9 MB | 69.1 MB |
| Binary, gzip -9 | 15.1 MB | 22.6 MB |
| Release archive | 15.2 MB | 22.9 MB |

The cliproxy-rs binary links glibc and libstdc++ dynamically; BoringSSL is linked in. The Go binary is the official release build (stripped), and its archive also holds two READMEs and the example config.

## What the numbers say

- Non-streaming requests: Go is faster. It handled 1,568 requests per second against cliproxy-rs's 1,168, about 34% more, and used less CPU per request (0.62 ms against 0.85 ms). Both servers were CPU-bound in this test.
- Plain streams: Go was ahead too, 916 against 833 streams per second.
- Translated streams: in messages-stream, where both servers translate between the Anthropic and OpenAI formats, cliproxy-rs served 793 streams per second against Go's 564, about 41% more, with less CPU per request (1.25 ms against 1.75 ms) and a lower p99 latency (68 ms against 115 ms).
- Slow streams: with 256 streams that each last about a second, throughput is set by the upstream and both kept up; cliproxy-rs used 1.95 ms of CPU per stream against 2.9 ms.
- Memory: cliproxy-rs used about 40% of Go's memory at idle (17.3 MB against 44.7 MB) and under load (25 to 27 MB against 58 to 79 MB). With 256 slow streams open it peaked at 50 MB and Go at 104 MB.
- Startup and size: cliproxy-rs answered its first request in 17 ms (median) and its binary is half the size of Go's.

No response failed in any run.

## Claude soak: large prompts and memory

Measured on 2026-10-04, after a field report from a Linux machine (glibc, systemd user service) that had served Amp through the Claude route for 10 hours: about 2,400 streamed `/v1/messages` requests with large prompts and a few concurrent sessions left cliproxy-rs 0.1.1 at 647 MB resident (`VmRSS`) after a peak (`VmHWM`) of 840 MB, almost all of it anonymous memory. The scenarios above use 1.7 KB requests and did not show it.

### Setup

- A virtual machine with 8 vCPUs (Intel Xeon at 2.60 GHz) and 16 GB of memory, running Debian 12 (glibc 2.36) with Linux 6.1. It is a shared machine: the same binary measured from 25 to 37 ms of CPU per request across runs, so compare servers within one session.
- cliproxy-rs master at `afe356b` (the 0.1.1 code) and the change described below, release profile, rustc 1.99.0. CLIProxyAPI at `6fecc6e`, built from source with Go 1.26.4.
- [`bench/messages.sh`](../bench/messages.sh) runs in a loopback-only network namespace. [`bench/messages/`](../bench/messages) is both the fake Claude upstream and the load generator (Go standard library only). Both servers get one Claude API key whose `base-url` is the fake upstream. An OAuth login would make both servers fetch the account profile from `api.anthropic.com`, which this setup cannot reach, so the Claude Code OAuth path (cloaking, tool-name remapping, the native TLS client) is not covered.
- The server runs on CPUs 0 to 3; the fake upstream and the load generator on CPUs 4 to 7. Each run starts a fresh server.
- Load: 8 concurrent sessions. Each session is a growing coding-agent conversation: an 18 KB system prompt, 24 tools, then pairs of an assistant turn (signed thinking, text, `tool_use`) and a user `tool_result`, growing from 100 KB to 500 KB in 25 KB steps before a new session starts. 3,000 streamed requests, 306 KB on average. The upstream reads the whole request and streams 150 delta events 2 ms apart (thinking with a signature, text, then a `tool_use` block), about 43 KB of SSE over 0.37 s.
- Large-prompt variant: the same with conversations from 1 MB to 3 MB in 100 KB steps, 600 requests, 1.9 MB on average.
- Measured per run: peak RSS (`VmHWM`), RSS 30 seconds after the load, server CPU time per request, completed requests per second, and at the client the time to the first response byte (TTFB) and to the end of the stream. A response counts only with status 200 and a `message_stop` event; none failed. RSS is sampled every second into `<label>.rss.tsv`.
- Sent straight to the fake upstream (measured in the allocator session), the same load sees a TTFB p50 of 3.5 ms and a total p50 of 373 ms; the rest of a proxy's TTFB is the time it adds.

```sh
bench/messages.sh /tmp/soak go /path/to/cli-proxy-api
bench/messages.sh /tmp/soak rust target/release/cliproxy
bench/messages.sh /tmp/soak rust-arena2 target/release/cliproxy MALLOC_ARENA_MAX=2
MIN=1000000 MAX=3000000 STEP=100000 N=600 bench/messages.sh /tmp/soak large target/release/cliproxy
```

### What was found

- CPU was the larger problem under this load. A `perf` profile of master under this load put 79% of the server's CPU in JSON scanning. The Claude executor visited the blocks that can carry `cache_control` (tools, system blocks, every message content block) by looking each one up again by path from the start of the body, so counting, normalizing and checking cache markers rescanned the conversation once per block: quadratic in its length. Go walks the blocks once with `ForEach`. The session-ID lookups also scanned long strings byte by byte. master spent 90 to 110 ms of CPU on a 306 KB request and 535 to 551 ms on a 1.9 MB one.
- No leak. A heaptrack run of master over 600 requests of this load peaked at 33.5 MB of heap (the same load without heaptrack peaks at 66 MB of RSS), and 1.8 MB was still allocated at exit (process-lifetime state). The request handler held about eight copies of a body at its peak; they were all freed when the request ended.
- Retention. After the large-prompt load, calling `malloc_trim(0)` inside the running server (through gdb) dropped its RSS from 86 MB to 42 MB and its anonymous memory from 66 MB to 21 MB. That memory was free inside glibc's per-thread arenas, which return only the top of each arena to the kernel. This is the mechanism behind a peak-then-plateau pattern like the field report; the 10-hour field number itself was not reproduced here.

### What changed

- The `cache_control` block walk reads each block from one pass over the body, in Go's order (tools, system, messages), as do the web-search domain cleanup and the 1h-TTL check. The JSON scanner in `cpa-common` jumps over string contents with `memchr`. Both keep Go's results; the Go golden tests pass unchanged.
- The final upstream body moves into the HTTP request instead of being copied, and the client's original body is no longer copied into a second `String`.
- On Linux with glibc, a background thread calls `malloc_trim(0)` once the process goes quiet (under 50 ms of CPU in five seconds) and at least once a minute. An earlier version trimmed every 10 seconds; in two A/B pairs against no trimming it added 3.5 ms to the TTFB p50 in one pair and nothing measurable in the other, within this machine's noise. Trimming when quiet keeps the page faults that follow a trim away from busy periods. macOS and Windows keep their system allocators unchanged.

### Allocators

master, default load, one session. None of the alternatives lowered the memory at the end of the run as much as trimming, and mimalloc nearly tripled it.

| master with | Peak RSS (MB) | RSS 30 s later (MB) | CPU ms per request | TTFB p50 / p99 (ms) |
| --- | --- | --- | --- | --- |
| glibc malloc | 66.3 | 51.8 | 92 | 84 / 199 |
| glibc, `MALLOC_ARENA_MAX=2` | 64.0 | 45.6 | 89 | 81 / 190 |
| jemalloc (tikv-jemallocator 0.6, defaults) | 76.3 | 61.0 | 81 | 73 / 171 |
| mimalloc 0.1 (defaults) | 180.0 | 160.9 | 84 | 77 / 176 |

With the CPU fix and the large-prompt load, `MALLOC_ARENA_MAX=2` left 106 MB after the run against 74 MB with default glibc, and a fixed `MALLOC_MMAP_THRESHOLD_=131072` left 26 MB but cost 23% more CPU per request (147 ms against 120 ms).

### Results

Default load (306 KB requests), two rounds per server in one session, run in the order Go, master, this change.

| Server | Round | Peak RSS (MB) | RSS 30 s later (MB) | CPU ms per request | Requests/s | TTFB p50 / p99 (ms) | Total p50 / p99 (ms) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Go | 1 | 115.1 | 80.4 | 63 | 18.9 | 43.7 / 94.8 | 416 / 558 |
| Go | 2 | 107.6 | 81.6 | 58 | 19.5 | 39.2 / 85.7 | 406 / 459 |
| cliproxy-rs master | 1 | 65.7 | 49.9 | 110 | 16.4 | 100.6 / 234.0 | 482 / 615 |
| cliproxy-rs master | 2 | 66.7 | 45.3 | 90 | 17.6 | 82.5 / 192.0 | 449 / 561 |
| cliproxy-rs, this change | 1 | 64.2 | 29.0 | 32 | 19.9 | 24.9 / 53.0 | 399 / 447 |
| cliproxy-rs, this change | 2 | 63.4 | 29.0 | 31 | 20.0 | 22.7 / 55.8 | 393 / 522 |

Large prompts (1.9 MB requests), one round.

| Server | Peak RSS (MB) | RSS 30 s later (MB) | CPU ms per request | Requests/s | TTFB p50 / p99 (ms) | Total p50 / p99 (ms) |
| --- | --- | --- | --- | --- | --- | --- |
| Go | 263.9 | 183.5 | 225 | 13.2 | 241 / 418 | 593 / 773 |
| cliproxy-rs master | 232.1 | 157.7 | 551 | 7.1 | 732 / 1,841 | 1,057 / 2,062 |
| cliproxy-rs, this change | 176.9 | 42.7 | 129 | 15.4 | 144 / 292 | 509 / 633 |

- On 306 KB requests this change uses about a third of master's CPU per request and half of Go's, and adds about 20 ms to the time to first byte against Go's 36 to 40 ms and master's 79 to 97 ms. Its throughput (20 requests per second) is close to the 21.4 the fake upstream allows on its own.
- 30 seconds after the load it holds 29 MB against master's 45 to 50 MB and Go's 80 to 82 MB. Peak RSS barely moved (63 to 64 MB against 66 to 67 MB): the peak is the requests in flight, and trimming only returns what they leave behind.
- With 1.9 MB prompts master was more than three times slower than Go to first byte; this change's TTFB p50 is 40% lower than Go's, and it holds 43 MB after the run against Go's 184 MB and master's 158 MB.
- Raw results, including the allocator and trim comparisons: [`bench/results/2026-10-04-messages.jsonl`](../bench/results/2026-10-04-messages.jsonl). The `session` field groups runs that were measured together.

## History

Four runs on 2026-10-03 with the same method and machine. The Go column gives the Go result measured in each run, which shows how much the machine itself varied between runs.

| Scenario | Run 1 | Run 2 | Run 3 | 0.1.0 | Go, per run |
| --- | --- | --- | --- | --- | --- |
| chat, requests per second | 1,402 | 1,403 | 1,307 | 1,168 | 1,677 / 1,696 / 1,859 / 1,568 |
| chat-stream, streams per second | 627 | 956 | 944 | 833 | 989 / 940 / 1,038 / 916 |
| messages-stream, streams per second | 640 | 913 | 830 | 793 | 607 / 613 / 657 / 564 |
| Idle memory | 13.8 MB | 14.8 MB | 17.4 MB | 17.3 MB | 44.6 / 45.1 / 44.1 / 44.7 MB |
| Binary | 29.6 MB | 33.0 MB | 35.9 MB | 35.9 MB | 69.1 MB |

- Run 1: cliproxy-rs did not yet set `TCP_NODELAY` on client connections (Go sets it on every connection), so each small SSE write waited for the client's acknowledgement and the fast streaming tests were latency-bound rather than CPU-bound.
- Run 2: `TCP_NODELAY` set; cliproxy-rs wrote no access log yet.
- Run 3 and 0.1.0: cliproxy-rs writes Go's access log, request-log capture is wired in (off in this config), and usage reporting follows Go's contract. Since run 2, Go's lead on non-streaming requests grew from about 20% to 34 to 42%.

Raw results: [`bench/results/`](../bench/results).
