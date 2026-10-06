# Benchmarks

Binary size, memory and throughput of cliproxy-rs and CLIProxyAPI on the same configuration, with a local fake upstream. The numbers come from one small machine and a synthetic load, so use them to compare the two servers with each other. They say little about how much traffic either one can carry on bigger hardware.

## Field memory

On 2026-10-05, a personal install in daily use ran at 75 to 101 MB RSS. That reading was unscripted; sample timing, workload and the exact build were not recorded.

The 17.3 MB idle result below is from a fresh 0.1.0 process with no connected accounts and small synthetic requests. An earlier 647 MB field report prompted the [Claude soak](#claude-soak-large-prompts-and-memory), which measured retained memory after larger requests but did not reproduce that report. Keep those cases separate when comparing memory.

A later field report from 0.2.0, serving Claude and Codex with 1 to 2 MB prompts, showed a resting floor that rose from 26 to 89 MB over 7 hours and peaks of 300 to 400 MB. It prompted the [field mix](#field-mix-claude-codex-and-count_tokens) soak, in which two copies of the same tokenizer, about 93 MB, made up most of 0.2.0's resting RSS of 128 to 145 MB.

## Setup

Measured on 2026-10-03.

- cliproxy-rs 0.1.0, the launch build, release profile (thin LTO, one codegen unit, stripped), built with rustc 1.99.0.
- CLIProxyAPI v8.0.10 (commit `6fecc6e`), the official `linux_amd64` release binary, built with Go 1.26.4.
- A virtual machine with 2 vCPUs (Intel Xeon at 2.60 GHz) and 3.9 GB of memory, running Debian 12 with Linux 6.1. It is a shared machine, and the same binary measures differently from run to run, by up to about 15%; compare the two servers within one run.
- Both servers run with the same `config.yaml`: one OpenAI-compatible provider that points at the fake upstream, one client key and an empty credential folder, started with `--local-model`. Each scenario starts a fresh server process.
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

Measured on 2026-10-04, after a field report from a Linux machine (glibc, systemd user service) that had served large-prompt Claude traffic for 10 hours: about 2,400 streamed `/v1/messages` requests with a few concurrent sessions left cliproxy-rs 0.1.1 at 647 MB resident (`VmRSS`) after a peak (`VmHWM`) of 840 MB, almost all of it anonymous memory. The scenarios above use 1.7 KB requests and did not show it.

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
- On Linux with glibc, a background thread calls `malloc_trim(0)` once the process goes quiet (under 50 ms of CPU in five seconds) and at least once a minute. An earlier version trimmed every 10 seconds; in two A/B pairs against no trimming it added 3.5 ms to the TTFB p50 in one pair and nothing measurable in the other, within this machine's noise. Trimming when quiet keeps the page faults that follow a trim away from busy periods. macOS and Windows keep their system allocators unchanged. (Since 2026-10-05 this is a task that trims once after startup, then parks until a response ends, runs the trim on the blocking pool rather than a request thread, and forces a trim every minute only while busy; see Idle below.)

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

## Field mix: Claude, Codex and count_tokens

Measured on 2026-10-06, after a field report from a Linux server (glibc, 2 request threads, session affinity off) that served a coding agent's Claude and Codex traffic, with 1 to 2 MB prompts, on 0.2.0. A sampler read `/proc` once a minute; by hour after start:

| Hour | RSS min / median / max (MB) | `VmHWM` (MB) |
| --- | --- | --- |
| 0 | 26 / 31 / 54 | 148 |
| 1 | 29 / 47 / 100 | 148 |
| 2 | 39 / 64 / 82 | 148 |
| 3 | 33 / 54 / 73 | 148 |
| 4 | 52 / 77 / 315 | 395 |
| 5 | 66 / 66 / 214 | 403 |
| 6 | 89 / 91 / 202 | 438 |
| 7 | 89 / 89 / 405 | 439 |

The floor (each hour's lowest sample) rose from 26 to 89 MB and the peaks reached 300 to 400 MB. These are field observations: the traffic, the config and the sampling cannot be reproduced on demand. The soak that CI ran on release tags (`bench/soak.sh` with the Claude soak's default load: streamed Claude requests of 100 to 500 KB from 8 sessions) sent no Codex requests, no count_tokens and no body over 500 KB, so it could not show this.

### Setup

- [`bench/soak.sh`](../bench/soak.sh) with `MIX=field`, in a loopback-only network namespace. [`bench/messages/`](../bench/messages) is the fake upstream for both providers and the load generator. The server has one Claude API key and one Codex API key whose `base-url` is the fake upstream; a Claude API key on another origin has its count_tokens counted locally, as an OAuth login does.
- Load: 4 concurrent sessions, 2 Claude (streamed `/v1/messages`, shaped as in the Claude soak) and 2 Codex (streamed `/v1/responses` for `gpt-5.5` through the Codex executor: 18 KB of instructions, 24 function tools, then reasoning items with encrypted content, function calls and their outputs). Each conversation grows from 200 KB to 2 MB in 100 KB steps, then a new one starts; requests averaged 1,019 to 1,028 KB. Before every 4th Claude turn the same body goes to `/v1/messages/count_tokens`, alternately for the Claude model and for `gpt-5.5` (a Claude client counting tokens for a Codex model). The fake upstream reads the whole request and streams 150 events 2 ms apart for either provider, ending in a tool call.
- Batches of 160 turns (about 178 requests with the count_tokens calls). After each batch the server rests 20 seconds and its `VmRSS` is read: the resting floor. `VmRSS`, `VmHWM`, threads, CPU ticks and context switches are also sampled every 10 seconds. After 60 minutes of load the server idles for 300 seconds, then its context switches are counted over 30 more seconds, as `bench/idle.sh` counts them.
- Builds: the 0.1.2 and 0.2.0 release binaries (`cliproxy-<version>-x86_64-unknown-linux-gnu.tar.gz`, checked against each release's `SHA256SUMS`) and this change, built on the runner with rustc 1.99.0. Each ran on its own GitHub-hosted Ubuntu 22.04 runner (4 vCPUs, so 0.1.2 starts 4 request threads and the others 2), all three at the same time, through `soak.yml`'s `releases` input. MB here is 1,024 kB as `/proc` reports it.
- The runs in this section used an earlier version of the fake upstreams, whose tool-call fragments did not join into JSON (and, for Codex, did not join into the completed arguments); the load generator did not check them then. It does now. With the corrected fake upstreams the same binary measured the same: on the 2-vCPU machine described under Results, alternating the two versions (10 minutes each, two runs each, `vm2-mock-check-10min`), peak RSS was 159.5 and 155.4 MB against 159.2 and 163.8 MB, resting `RssAnon` 53.0 to 67.1 and 52.8 to 66.9 MB against 52.1 to 66.4 and 53.6 to 66.8 MB, and CPU 42.2 and 41.7 ms per request against 41.9 and 41.2. All four runs passed the soak's gates. The numbers below stand as measured.

```sh
MIX=field bench/soak.sh target/release/cliproxy 60 /tmp/soak-field
```

### Results

60 minutes each; no request failed.

| | 0.1.2 | 0.2.0 | This change |
| --- | --- | --- | --- |
| Requests (Claude / Codex / count_tokens) | 7,186 / 8,334 / 1,746 | 7,889 / 8,271 / 1,834 | 8,047 / 8,433 / 1,869 |
| Resting RSS over the hour, lowest / highest (MB) | 132.3 / 157.9 | 128.2 / 145.4 | 80.6 / 97.6 |
| Resting RSS in the first third, lowest / highest (MB) | 137.0 / 157.9 | 128.2 / 140.7 | 80.6 / 96.3 |
| Resting RSS in the last third, lowest / highest (MB) | 135.6 / 157.1 | 131.3 / 139.4 | 80.6 / 97.6 |
| RSS every 10 s, median / highest (MB) | 180.2 / 222.4 | 165.1 / 212.8 | 105.7 / 150.9 |
| Peak RSS (`VmHWM`, MB) | 233.2 | 220.1 | 169.0 |
| Server CPU per request (ms), own runner | 89.0 | 47.6 | 31.1 |
| Threads under load | 8 | 4 | 4 |
| After 300 s idle: RSS (MB), wakeups in 30 s, threads | 144.6, 2,913, 8 | 138.3, 4, 3 | 93.0, 3, 3 |

CPU per request depends on the machine, and each build had a runner of its own, so the CPU row does not compare the builds. On one machine (a virtual machine with 2 vCPUs, Intel Xeon at 2.60 GHz, and 3.8 GB of memory, running Debian 12 with glibc 2.36 and Linux 6.1, shared by the server and the load generator; 10 minutes of the same load per build, one after another), the tokenizer change made no difference to CPU, and the memory differences held:

| Same machine, 10 minutes | 0.1.2 | 0.2.0 | This change (2 runs) |
| --- | --- | --- | --- |
| Server CPU per request (ms) | 68.2 | 43.1 | 42.9, 44.1 |
| Resting `RssAnon` (MB) | 102.3 to 114.6 | 99.0 to 108.2 | 54.0 to 66.0, 54.4 to 62.5 |
| Peak RSS (`VmHWM`, MB) | 199.0 | 204.7 | 158.7, 158.3 |

- The resting floor stepped up once, in the first batch, and stayed flat for the hour on all three builds: on each, the last third's lowest and highest resting readings are within the soak's tolerance (10% plus 2 MB) of the first third's. The step is the tokenizers below. Over the hour this load did not reproduce a slow climb like the field report's. A plausible reading, not proven: there, Claude count_tokens, Codex count_tokens and streamed Claude requests to Codex models first ran hours apart, and each built its own tokenizer then.
- Against 0.2.0, this change's resting RSS is 47.6 MB lower at its lowest and 47.8 MB lower at its highest, and its peak 51.1 MB lower.
- 0.1.2 woke 2,913 times in 30 seconds after the load: its file watcher polled (see [Idle](#idle-wakeups-threads-and-memory)). Both later builds were back to 3 threads and 3 or 4 wakeups in 30 seconds.

### What the heap held

- Tokenizers, measured as the growth of `RssAnon` after a trim on a fresh server, one small request at a time: Claude count_tokens added 46.5 MB, Codex count_tokens for `gpt-5.5` 46.5 MB, for `gpt-4` 24.0 MB, and a Claude client's streamed request to `gpt-5.5` (whose `message_start` gets an o200k_base estimate of the request, as in Go) 47.1 MB: 164.1 MB of tokenizers on 0.2.0, and 163.8 MB on 0.1.2. Five modules kept seven lazily built `tiktoken_rs::CoreBPE` statics between them (five o200k_base, two cl100k_base), each built and kept for good. With one shared encoder per encoding the same sequence adds 46.8, 0, 24.1 and 0.2 MB: 71.0 MB.
- heaptrack, on master with line tables, after two batches of 100 field-mix turns, both count_tokens paths included: 67.1 MB was still allocated at exit, 63.8 MB of it two o200k_base encoders (31.9 MB each, one built by Claude count_tokens and one by Codex count_tokens) and 1.7 MB the tokenizer regex's caches. Everything else came to 1.7 MB, the largest part being TLS root certificates (0.7 MB). Of the 31.9 MB per encoder, the encoder map and its keys are 9.6 MB, a decoder map and its values 9.6 MB, a sorted token list 5.9 MB and the regexes 6.8 MB, 5.9 MB of that 128 per-thread copies of the main one; counting never decodes. The 600,000 short token allocations at malloc's 32-byte minimum chunk make it 47 MB resident.
- glibc held almost nothing back. `malloc_info` on master (without heaptrack) after three batches of 160 field-mix turns and 30 seconds idle: 31.5 MB free inside the arenas, already returned to the kernel by the server's heap trim, and 37.6 MB in five mmapped chunks (the encoders' hash tables). Calling `malloc_trim(0)` again through gdb changed `RssAnon` by 40 kB. The resting floor is live data.

### What was checked and ruled out, or left

- Duplicate tokenizers: confirmed, and fixed by sharing one lazily built encoder per encoding. Go tokenizes at the same places (`helps.CountClaudeInputTokens` for Claude count_tokens and the `message_start` estimate, which share one codec behind a `sync.Once`; Codex, Meta and xAI `CountTokens`; `TokenizerForModel` for OpenAI-compatible counts), so the counts need the real encoders. Nothing is built until a count needs one, and the first count waits for it (0.15 s for o200k_base, 0.06 s for cl100k_base). Without count_tokens, native Claude requests (a Claude client to a Claude model) and Codex Responses requests build none: on a fresh server the first of each added 0.3 to 0.6 MB. The exception is a streamed Claude-format request to a non-Claude model, Codex included: as in Go, its `message_start` gets an estimate of the request's input tokens, which builds o200k_base.
- Unbounded maps: every map on these routes is bounded by an entry count, a time window or the open connections, and the hour above shows no climb. Two have bounds far above a few MB. The Codex reasoning replay, used only for Claude-format clients of Codex models, keeps up to 10,240 sessions of up to 256 turns each and drops expired ones only when they are read or at that cap (Go also purges them on a timer); a Responses client never fills it. The [LCP session matcher](../crates/cpa-server/src/lcp.rs) is the other. It is used only with session affinity on, for requests that carry no session ID, and it is bounded by entry counts (Go's), not bytes. Driven directly with four interleaved sessions and a counting allocator, 150-turn conversations at one request every 3 seconds held 57.7 MB after an hour and then 60.7 to 67.0 MB (its 1-hour TTL), and 300-turn conversations at one request a second held 82.2 MB from 40 minutes on, before the TTL could expire anything, so its entry caps (4,096 groups, 262,144 prefixes) were what held it there. The field server runs with session affinity off, the default, so the matcher played no part in its report. Left as a general limit, with the numbers in a `ponytail:` note.
- Fragmentation: ruled out for the resting floor (above). For peaks, glibc's dynamic mmap threshold lets body-sized buffers come from the arenas and stay resident until the next trim. On the 2-vCPU machine above (10 minutes of the field mix per setting, this change), a fixed `MALLOC_MMAP_THRESHOLD_` lowered the peak but cost CPU, and `MALLOC_ARENA_MAX=2` changed neither (the server and the load generator shared the 2 vCPUs, so CPU per request is higher than on the runners):

  | Setting | Resting `RssAnon` (MB) | Highest `RssAnon` sample (MB) | Peak RSS (`VmHWM`, MB) | CPU per request (ms) |
  | --- | --- | --- | --- | --- |
  | glibc defaults, run 1 | 54.0 to 66.0 | 114.8 | 158.7 | 42.9 |
  | glibc defaults, run 2 | 54.4 to 62.5 | 119.5 | 158.3 | 44.1 |
  | `MALLOC_MMAP_THRESHOLD_=1048576` | 51.9 to 52.4 | 91.9 | 131.9 | 51.2 |
  | `MALLOC_MMAP_THRESHOLD_=262144` | 52.0 to 52.2 | 84.9 | 124.7 | 56.1 |
  | `MALLOC_ARENA_MAX=2` | 51.8 to 59.2 | 111.8 | 155.0 | 42.6 |

  A 1 MiB threshold took 26.6 MB (17%) off the peak for 16 to 19% more CPU per request, and 256 KiB 33.8 MB for 27 to 31% more: every large buffer then comes from fresh, zeroed pages. The server sets none of these; `MALLOC_MMAP_THRESHOLD_` in its environment remains an option for anyone who would trade that CPU for the lower peak.
- Peak per request: the Claude route peaks at 6.0x its body (11,083,177 bytes of live heap for a 1,843,825-byte request), count_tokens on the same body at 7.3x (13,530,973 bytes, in 1,630,109 allocations) and the Codex route at 8.8x (16,783,275 bytes for 1,916,052). `crates/cpa-server/tests/alloc_budget.rs` now gates all three. Codex request shaping rewrites the body pass by pass, each pass a new copy; walking it once is a larger change, left with its numbers in a `ponytail:` note in `crates/cpa-exec/src/codex_request.rs`.

### Gates

- `soak.yml` runs the field mix for an hour on every release tag, next to an hour of the Claude soak's load, and for 300 minutes every week. Either fails if any request fails or if the resting RSS climbs: the highest resting reading of the last third of the run more than 10% plus 2 MB above the highest of the first third, or the lowest of the last third that far above the lowest of the first third (a rising floor under peaks that do not rise). The field mix also fails if `VmHWM` ends above `soak.field.peak_hwm_kb` in [`bench/budgets.txt`](../bench/budgets.txt) (this change's 169.0 MB plus 20%, which 0.2.0 exceeds).
- Raw results: [`bench/results/2026-10-06-field.jsonl`](../bench/results/2026-10-06-field.jsonl) holds one `bench/soak.sh` summary per run, with the resting readings' lowest and highest over the run and in its first and last thirds, and the requests by kind. Every resting reading behind them is in [`bench/results/2026-10-06-field-batches.tsv`](../bench/results/2026-10-06-field-batches.tsv). The `session` field groups the runs: `ci-runners-60min` (the three runners), `vm2-releases-10min`, `vm2-allocator-10min` and `vm2-mock-check-10min` (the 2-vCPU machine; the allocator runs name their setting in `env`, and the two without one are this change's same-machine runs), `tokenizer-steps` (`RssAnon` in kB after each request on a fresh server), `heaptrack` (bytes still allocated at exit, by site), `malloc-info`, and `lcp-matcher` (live heap by hour, in MB of 10^6 bytes as measured).

## Claude latency: time added before the first byte

Measured on 2026-10-05: how long cliproxy-rs holds a streamed `/v1/messages` request before its first byte reaches the upstream, and before the first response byte reaches the client, for the Claude Code OAuth path (cloaking and the native TLS client) and the Claude API-key path, with the process's peak memory alongside.

### Setup

- A virtual machine with 8 vCPUs (Intel Xeon at 2.60 GHz) and 16 GB of memory, running Debian 12 (glibc 2.36) with Linux 6.1. It is shared, so compare numbers measured in the same session.
- "master" is `1849512` (0.1.2) with only the harness and its five trace marks added; "this change" is the branch described below. Both are release builds (thin LTO, one codegen unit) with line tables kept for profiling, rustc 1.99.0.
- [`bench/latency.sh`](../bench/latency.sh) runs [`claude_latency`](../crates/cpa-server/examples/claude_latency.rs) in a loopback-only network namespace, once per path. It is one process with two Tokio runtimes of 4 workers each: one runs the production router, listener and Claude executor; the other runs a mock Anthropic upstream and a keep-alive HTTP/1.1 client. Sharing one process gives every timestamp the same monotonic clock.
- OAuth path: a Claude Code OAuth credential (an `sk-ant-oat` token with its device and account IDs, so no profile fetch) and the production native TLS profile, with the executor's test hooks resolving `api.anthropic.com` to a local TLS mock that trusts a throwaway CA. Cloaking, cache_control handling, tool-name aliasing, signature sanitising and CCH signing all run. API-key path: a `claude-api-key` entry whose `base-url` is a plain HTTP mock.
- Bodies: coding-agent conversations of 5,000, 50,000, 300,000 and 2,000,000 bytes (a system prompt with a cache breakpoint, tool definitions, assistant turns with signed thinking, text and `tool_use`, user `tool_result` turns), sent with `stream: true`. The mock reads the whole request and answers at once with a short SSE stream.
- Per cell: 20 warm-up requests, then 200 measured, from 1 client or from 8 concurrent clients (25 each). The same requests are also sent straight to the mock; "added" is the proxied percentile minus the direct one. Two rounds, run master, this change, master, this change; each cell shows round 1, round 2.
- First upstream byte: from the client starting the request to the mock's request handler (the request head has arrived). First client byte: to the first response body byte at the client.
- Stages (1 client, p50): trace events (`target: "cpa_latency"`, level trace) at the route handler, the selected credential, the executor, the translated request and the finished upstream request, plus the mock's timestamps. The server's logging leaves them off unless `RUST_LOG` enables trace for that target (`RUST_LOG=cpa_latency=trace` prints them as log lines); its own debug setting stops at debug level.
- Memory: the process's `VmHWM` after each body size. It covers the proxy, the mock and the client together, so it is an upper bound on the proxy's own peak, and it only grows through a run.

```sh
bench/latency.sh /tmp/latency.jsonl
N=200 SIZES=300000 CONC=8 bench/latency.sh /tmp/latency.jsonl
```

### Results

Added to the first upstream byte, ms (round 1, round 2):

| Path | Body | Clients | master p50 | master p99 | this change p50 | this change p99 |
| --- | --- | --- | --- | --- | --- | --- |
| OAuth | 5 KB | 1 | 1.24, 1.28 | 1.76, 1.49 | 0.95, 1.05 | 1.21, 1.35 |
| OAuth | 5 KB | 8 | 1.97, 1.86 | 3.42, 2.65 | 1.38, 1.37 | 2.23, 2.23 |
| OAuth | 50 KB | 1 | 4.97, 5.03 | 5.57, 5.48 | 2.31, 2.33 | 2.68, 3.52 |
| OAuth | 50 KB | 8 | 8.22, 7.61 | 14.75, 10.66 | 3.84, 4.42 | 5.74, 7.84 |
| OAuth | 300 KB | 1 | 20.69, 20.75 | 31.80, 31.25 | 6.66, 7.03 | 7.79, 9.25 |
| OAuth | 300 KB | 8 | 29.21, 28.88 | 41.49, 41.93 | 8.05, 8.71 | 13.64, 12.86 |
| OAuth | 2 MB | 1 | 139.67, 139.14 | 197.67, 167.40 | 44.87, 46.66 | 55.76, 55.78 |
| OAuth | 2 MB | 8 | 217.47, 196.64 | 298.87, 268.67 | 55.60, 62.39 | 81.91, 82.17 |
| API key | 5 KB | 1 | 0.66, 0.67 | 0.93, 0.87 | 0.48, 0.48 | 0.62, 0.70 |
| API key | 5 KB | 8 | 0.92, 0.95 | 1.44, 1.52 | 0.66, 0.74 | 1.23, 1.21 |
| API key | 50 KB | 1 | 2.65, 2.67 | 3.14, 4.05 | 1.20, 1.24 | 1.41, 1.47 |
| API key | 50 KB | 8 | 4.36, 4.91 | 7.31, 8.68 | 1.95, 2.02 | 3.05, 3.09 |
| API key | 300 KB | 1 | 11.12, 11.07 | 14.65, 18.23 | 3.83, 3.66 | 4.95, 4.33 |
| API key | 300 KB | 8 | 15.34, 16.03 | 22.87, 23.64 | 4.68, 4.55 | 7.53, 7.35 |
| API key | 2 MB | 1 | 75.10, 72.12 | 93.26, 96.28 | 24.14, 25.10 | 32.35, 33.21 |
| API key | 2 MB | 8 | 106.64, 107.13 | 142.64, 142.68 | 31.19, 33.47 | 47.43, 44.68 |

Added to the first client byte, ms (round 1, round 2):

| Path | Body | Clients | master p50 | master p99 | this change p50 | this change p99 |
| --- | --- | --- | --- | --- | --- | --- |
| OAuth | 5 KB | 1 | 1.45, 1.53 | 1.98, 1.75 | 1.20, 1.34 | 1.50, 1.65 |
| OAuth | 5 KB | 8 | 2.72, 2.62 | 4.58, 3.75 | 2.08, 2.18 | 3.32, 3.25 |
| OAuth | 50 KB | 1 | 5.26, 5.35 | 5.82, 5.74 | 2.60, 2.67 | 2.93, 3.78 |
| OAuth | 50 KB | 8 | 10.87, 10.73 | 19.28, 15.61 | 5.34, 6.74 | 8.60, 10.99 |
| OAuth | 300 KB | 1 | 20.92, 21.07 | 31.81, 31.14 | 6.93, 7.23 | 7.73, 9.09 |
| OAuth | 300 KB | 8 | 40.32, 39.37 | 59.48, 58.48 | 11.36, 11.88 | 17.11, 17.14 |
| OAuth | 2 MB | 1 | 140.88, 140.16 | 197.81, 166.79 | 45.86, 47.55 | 55.62, 56.23 |
| OAuth | 2 MB | 8 | 266.26, 257.39 | 390.99, 388.20 | 75.57, 76.12 | 105.24, 107.34 |
| API key | 5 KB | 1 | 0.84, 0.86 | 1.16, 1.08 | 0.65, 0.66 | 0.82, 0.95 |
| API key | 5 KB | 8 | 1.38, 1.34 | 2.01, 2.20 | 1.09, 1.17 | 1.72, 1.80 |
| API key | 50 KB | 1 | 2.94, 2.95 | 3.54, 4.32 | 1.47, 1.55 | 1.71, 1.78 |
| API key | 50 KB | 8 | 6.01, 6.24 | 9.26, 10.68 | 3.00, 3.01 | 4.55, 4.62 |
| API key | 300 KB | 1 | 11.40, 11.41 | 14.81, 18.30 | 4.13, 3.93 | 5.13, 4.41 |
| API key | 300 KB | 8 | 21.31, 21.94 | 31.03, 32.60 | 6.46, 6.07 | 10.67, 9.21 |
| API key | 2 MB | 1 | 76.22, 73.23 | 94.18, 96.34 | 24.90, 26.00 | 32.77, 35.01 |
| API key | 2 MB | 8 | 138.66, 139.20 | 172.19, 203.58 | 39.86, 41.85 | 63.55, 54.68 |

Where the time goes, one client, p50 in ms, round 1 / round 2 (master → this change):

| Stage | OAuth 50 KB | OAuth 300 KB | OAuth 2 MB | API key 50 KB | API key 300 KB | API key 2 MB |
| --- | --- | --- | --- | --- | --- | --- |
| Read the client body (client write, HTTP parse, routing, client key) | 0.13 / 0.12 → 0.12 / 0.13 | 0.19 / 0.19 → 0.19 / 0.20 | 0.88 / 0.85 → 0.80 / 0.80 | 0.11 / 0.11 → 0.11 / 0.12 | 0.20 / 0.20 → 0.21 / 0.20 | 1.81 / 0.92 → 0.76 / 1.62 |
| Route and select a credential (model peek, session IDs, scheduler) | 0.67 / 0.68 → 0.21 / 0.21 | 1.79 / 1.78 → 0.44 / 0.45 | 9.64 / 9.31 → 1.95 / 1.98 | 0.69 / 0.69 → 0.22 / 0.23 | 1.80 / 1.79 → 0.47 / 0.45 | 9.45 / 9.27 → 2.02 / 2.02 |
| Translate (Claude to Claude) | 0.03 / 0.03 → 0.03 / 0.02 | 0.18 / 0.18 → 0.18 / 0.19 | 1.52 / 1.54 → 1.48 / 1.44 | 0.04 / 0.03 → 0.03 / 0.04 | 0.18 / 0.14 → 0.19 / 0.17 | 1.85 / 0.59 → 0.45 / 1.50 |
| Parse and rewrite (thinking, cloaking, cache_control, aliases, signatures, CCH, headers) | 3.96 / 4.03 → 1.79 / 1.79 | 18.39 / 18.44 → 5.69 / 6.05 | 127.36 / 127.16 → 40.50 / 42.25 | 1.69 / 1.69 → 0.72 / 0.73 | 8.80 / 8.76 → 2.85 / 2.73 | 61.95 / 60.99 → 20.07 / 20.07 |
| Connection checkout, request head on the wire | 0.22 / 0.23 → 0.21 / 0.23 | 0.27 / 0.27 → 0.27 / 0.28 | 0.56 / 0.54 → 0.43 / 0.43 | 0.17 / 0.17 → 0.16 / 0.18 | 0.24 / 0.24 → 0.23 / 0.23 | 0.32 / 0.31 → 0.28 / 0.29 |
| Request body on the wire | 0.04 / 0.04 → 0.04 / 0.04 | 0.17 / 0.18 → 0.19 / 0.18 | 1.47 / 1.46 → 1.39 / 1.39 | 0.00 / 0.00 → 0.00 / 0.00 | 0.00 / 0.00 → 0.00 / 0.00 | 0.42 / 0.43 → 0.37 / 0.38 |
| Mock answer to the first client byte | 0.36 / 0.36 → 0.35 / 0.38 | 0.55 / 0.55 → 0.54 / 0.58 | 2.00 / 1.94 → 1.88 / 1.87 | 0.32 / 0.31 → 0.31 / 0.34 | 0.51 / 0.51 → 0.53 / 0.52 | 2.07 / 1.79 → 1.78 / 1.99 |

Peak resident memory of the whole benchmark process (proxy, mock and client together) after each body size, MB, round 1, round 2:

| Path | Build | 5 KB | 50 KB | 300 KB | 2 MB |
| --- | --- | --- | --- | --- | --- |
| OAuth | master | 22.7, 23.5 | 29.8, 29.8 | 56.9, 56.7 | 223.6, 222.6 |
| OAuth | this change | 23.2, 23.2 | 29.6, 30.0 | 57.1, 57.5 | 219.4, 222.3 |
| API key | master | 21.8, 21.6 | 27.7, 27.7 | 53.1, 54.0 | 185.2, 185.0 |
| API key | this change | 21.6, 21.6 | 27.4, 28.0 | 54.2, 52.6 | 181.7, 182.2 |

There is no connect or TLS handshake row: no measured sequential request opened a connection. Of the 12,800 measured proxied requests (6,400 per build), 2 opened one, both in this change's run (API key, 50 KB, 8 clients): a request that started before the previous connection was back in the pool. In the 2 MB API-key column, reading the client body and translating move by about 1 ms between rounds in both builds. The "mock answer" row includes the mock locating its request marker in the body, which the direct baseline pays too. Peak memory differs between the builds by no more than it differs between rounds of one build, about 2%.

### Upstream connections

The Claude client for `api.anthropic.com` and Go's standard transport (API-key base URLs, OpenAI-compatible hosts) keep their connections between requests, before and after this change. The tests `native_client_reuses_its_connection` and `go_clients_reuse_connections` (HTTP/2 and HTTP/1.1) count the TCP connections a local TLS upstream accepts. Go reuses these too: its Claude Code transport is an `http.Transport` cached per proxy (`helps/utls_client.go`), keeping at most 2 idle connections per host (net/http's default).

The Codex client for `chatgpt.com` opens a new TCP and TLS connection for every request, as Go's dedicated uTLS connection per request does. Keeping those connections is a separate change.

### What changed

- gjson 0.8.1, which the Claude executor uses for most body reads, scanned JSON strings one byte at a time; almost all of a coding agent's prompt is string content. A patched copy in [`vendor/gjson`](../vendor/README.md) finds string ends with `memchr2`, and its validator does the same; results are unchanged and tested against the original functions. `cpa_common::json::valid` got the same string skip. Before this, `gjson::scan_squash` alone took 44% of the CPU in a `perf` profile of the OAuth path at 300 KB.
- Session-ID extraction (run by the server and again by the executor) asked for about twenty top-level keys, each found by scanning the body. It now walks a valid JSON object's top level once and keeps borrowed slices of the roots it queries; values are decoded on lookup, other members are skipped without decoding, and a duplicate key, an escaped key, an invalid body or any other path scans as before.
- The executor decodes UTF-8 with the fast validator before falling back to the lossy decoder, and the tool-name aliasing no longer copies its output once more.
- The bytes sent upstream are unchanged: the gjson change is checked against the original functions, the session index against plain lookups, and the Claude, Codex and server test suites (including the Go golden tests) pass.

### What is left

- At 50 KB the OAuth path adds about 2.3 ms before the first upstream byte and the API-key path 1.2 ms; at 300 KB, 6.7 to 7.0 ms and 3.7 to 3.8 ms. Most of it is "parse and rewrite": the Go-ported rules (thinking, cloaking, cache_control, tool aliases, signature sanitising, CCH signing, beta headers) each read the body again, and many return a new copy. In a `perf` profile of the OAuth path at 300 KB, the largest, signature sanitising and tool-name aliasing, take about 15% and 13% of it; the rest is spread over a dozen rules. Bringing 300 KB down to 1 to 2 ms means walking the body once for all of them.
- With 8 clients, a 2 MB request also waits for other requests' preparation on its Tokio worker: on the OAuth path the median to the first upstream byte is 56 to 62 ms against 45 to 47 ms with one client. Preparation runs on the async worker; moving it to a blocking thread would hold a thread for every large request, so the remedy is less work per request.
- Raw results: [`bench/results/2026-10-05-latency.jsonl`](../bench/results/2026-10-05-latency.jsonl).

## Idle: wakeups, threads and memory

Measured on 2026-10-06 on Linux, to see what a server costs while nobody uses it.

### Setup

- A virtual machine with 4 vCPUs (Intel Xeon at 2.60 GHz) and 7.8 GB of memory, running Debian 12 (glibc 2.36) with Linux 6.1.
- cliproxy-rs master at `1849512`, and the change that replaces the polling loops with file change notifications and deadline timers and starts two request threads by default. Both are release builds made with rustc 1.99.0 on that machine.
- [`bench/idle.sh`](../bench/idle.sh) runs each server in a loopback-only network namespace with one OpenAI-compatible API key, one client key and 1 or 2,000 Claude credential files whose tokens are valid for 30 days, so none is due for refresh. Without `-local-model`, so the catalog downloads at start are attempted (and fail at once, with no network).
- After a 30-second warm-up without requests, which outlasts the startup heap trim (5 to 10 seconds after start, on the blocking pool) and the pool's 10-second keep-alive, it sums the context switches of every thread alive across the next 30 seconds from `/proc/<pid>/task/*/status` (each one is a thread waking up), and reads the CPU ticks (1/100 s), the thread count at both ends and `VmRSS`. A thread that exits inside the window makes the run fail, since its wakeups would go uncounted.

```sh
bench/idle.sh target/release/cliproxy 1 30
bench/idle.sh target/release/cliproxy 2000 30
```

### Results

Median of three rounds.

| Server | Auth files | Wakeups in 30 s | CPU ticks in 30 s | Threads | RSS (MB) |
| --- | --- | --- | --- | --- | --- |
| master | 1 | 2,901 (97 a second) | 18 | 8 to 12 | 21.2 |
| this change | 1 | 0 | 0 | 3 | 19.7 |
| master | 2,000 | 1,109 | 3,005 (a whole core) | 9 to 10 | 71.3 |
| this change | 2,000 | 0 | 0 | 3 | 70.2 |

- master's config watcher looked at every file 20 times a second. With 2,000 auth files that kept one core busy all the time; with one file it cost about 0.6% of a core. Each look went through the blocking thread pool, so it woke several threads. Smaller loops ran too: the 5-second refresh scan, the 1-second discovery check and the heap-trim thread's 5-second tick.
- With this change no thread woke up in any 30-second window, with 1 or 2,000 files. The watcher sleeps until the kernel reports a change, the refresh loop until the next token is due (at most 10 minutes, to catch clock jumps), the discovery task until the config changes, and the heap trim, after one trim at startup, until a response or WebSocket turn ends; the 3-hour catalog refresh, and the 15-second advertisement refresh when discovery is on, still run. A blocking-pool thread exits 10 seconds after its last file or DNS task.
- Threads drop from 8 to 12 (four request threads on this 4-vCPU machine, a heap-trim thread and blocking-pool threads that the watcher kept alive) to 3: the main thread and two request threads. The count was the same at both ends of every window, and no thread exited inside one.
- Idle memory with one file is unchanged within the noise of these runs. The 2,000 credentials themselves cost about 50 MB on either build.
- The change adds 112 KB of `.text` and 4 KB of `.rodata`; the binary grows from 40,162,216 to 40,307,912 bytes (0.4%). (This build also carries current master's other changes since `1849512`, so part of the growth is theirs.)
- Raw results: [`bench/results/2026-10-06-idle.jsonl`](../bench/results/2026-10-06-idle.jsonl).

### Against Go, idle

The `go-comparison` job in [`idle.yml`](../.github/workflows/idle.yml) runs `bench/idle.sh` on a GitHub-hosted Ubuntu 22.04 runner for CLIProxyAPI v8.0.10 (the release binary) and for cliproxy-rs, on the same config: one OpenAI-compatible API key and no credential files. Each gets 30 seconds of warm-up and then 300 seconds of samples. Two runs on 2026-10-05, cliproxy-rs with the changes above:

| Server | Wakeups per minute | CPU ms per hour | Threads | RSS (MB) |
| --- | --- | --- | --- | --- |
| CLIProxyAPI v8.0.10 | 7.6 and 7.2 | 120 and 120 | 9 | 46.1 and 46.0 |
| cliproxy-rs | 0 and 0 | 0 and 0 | 3 | 21.9 and 21.5 |

CPU time is read in clock ticks of 10 ms, so Go's figure is one tick in five minutes in each run. The weekly job publishes these numbers in its summary.

### Two request threads under load

The same machine and builds, with `bench/messages.sh` (the Claude soak load: 8 sessions of 100 to 500 KB streamed requests, 1,200 requests per run). The server and the load share all 4 vCPUs, so master starts 4 request threads and this change 2. Two runs each, alternating.

```sh
N=1200 C=8 SERVER_CPUS=0-3 LOAD_CPUS=0-3 bench/messages.sh /tmp/soak rust target/release/cliproxy
```

| Server | Run | Peak RSS (MB) | RSS 30 s later (MB) | CPU s | Requests/s | TTFB p50 / p99 (ms) |
| --- | --- | --- | --- | --- | --- | --- |
| master | 1 | 63.2 | 29.5 | 27.0 | 20.3 | 15.5 / 33.3 |
| this change | 1 | 57.1 | 25.2 | 24.3 | 20.4 | 14.9 / 35.9 |
| master | 2 | 63.1 | 26.5 | 26.9 | 20.3 | 15.5 / 30.7 |
| this change | 2 | 57.9 | 27.9 | 24.1 | 20.4 | 15.0 / 40.1 |

- With two request threads the peak was about 6 MB (9%) lower and the server used about 10% less CPU for the same requests. Throughput is set by the fake upstream and did not change.
- Memory 30 seconds after the load was not measurably different in these short runs. The larger effect reported from long-running services (one glibc arena per busy thread, each holding freed memory) needs hours of varied load to show, which this test does not reproduce.
- The 99th percentile time to first byte was 3 to 9 ms higher with two threads; the median did not move. `worker-threads` or `TOKIO_WORKER_THREADS` raises the count where that matters.
- Raw results: [`bench/results/2026-10-05-workers.jsonl`](../bench/results/2026-10-05-workers.jsonl).

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
