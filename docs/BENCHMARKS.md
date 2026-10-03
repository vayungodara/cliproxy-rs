# Benchmarks

Binary size, memory and throughput of cliproxy-rs and the Go CLIProxyAPI on the same configuration, with a local fake upstream. The numbers come from one small machine and a synthetic load, so read them as a comparison between the two servers on that machine, not as a capacity figure.

## Setup

- cliproxy-rs at commit `d068002`, release profile (thin LTO, one codegen unit, stripped), built with Rust 1.99.0. An earlier run at `4abce40` is kept under History.
- CLIProxyAPI v8.0.10 (commit `6fecc6e`), the official `linux_amd64` release binary (Go 1.26.4).
- A 2-vCPU virtual machine (Intel Xeon at 2.6 GHz, 3.9 GB of memory) running Debian 12 with Linux 6.1. Other processes on the machine used under 5% of one CPU during the runs.
- Both servers run with the same `config.yaml`: one OpenAI-compatible provider that points at the fake upstream, one client key and an empty credential directory, started with `-local-model`. Each scenario starts a fresh server process.
- The server is pinned to CPU 0. The fake upstream and the load generator share CPU 1.
- The whole run happens in a network namespace with only a loopback interface. Go tries to download its management panel and an Antigravity version file at start; both fail at once there. On a machine with network access, Go's idle memory was about 10 MB higher (56 MB) after those downloads.
- Go writes one access-log line per request to standard output (redirected to a file). cliproxy-rs at this commit writes no access log yet, which saves it a little CPU in these tests.

The scripts are in [`bench/`](../bench): `upstream/` is the fake OpenAI-compatible upstream, `load/` the load generator, `run.sh` runs every scenario three times and `summary.sh` prints the median of the three rounds. Both helpers use only the Go standard library. The raw results are in [`bench/results/`](../bench/results), one file per commit.

```sh
bench/run.sh target/release/cliproxy /path/to/cli-proxy-api /tmp/bench 3
bench/summary.sh /tmp/bench/results.jsonl
```

## Scenarios

- Idle: start, wait 15 seconds, read the resident memory.
- chat: `POST /v1/chat/completions`, not streamed, 32 concurrent clients for 20 seconds. The request is 1.7 KB; the upstream answers at once with a 0.6 KB completion.
- chat-stream: the same request with `"stream": true`. The upstream sends 22 SSE chunks with no pause between them.
- chat-stream-slow: 256 concurrent streams, with 50 ms between chunks, so each response takes about 1.1 seconds. This is closest to many coding agents waiting on a model, and it shows the memory each open stream costs.
- messages-stream: `POST /v1/messages` in Anthropic format, streamed. Both servers translate it to OpenAI chat for the upstream and translate the stream back.

A response counts only if its status is 200 and the body has the expected end marker. Latency is measured at the client. CPU time per request is the server's user and system time during the load, divided by the completed requests. Peak memory is the process's `VmHWM`; the second memory figure is `VmRSS` 10 seconds after the load stops.

## Results

Median of three rounds, commit `d068002`.

### Idle

| Server | Startup (ms) | RSS after 15 s (MB) |
| --- | --- | --- |
| cliproxy-rs | 13 | 14.8 |
| Go | 98 | 45.1 |

### chat

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 1403 | 21.8 | 40.8 | 0.71 | 23.7 | 23.7 | 0 |
| Go | 1696 | 18 | 41.8 | 0.58 | 57.5 | 56.2 | 0 |

### chat-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 956 | 30 | 63.3 | 1.04 | 24.3 | 24.3 | 0 |
| Go | 940 | 33.8 | 66.9 | 1.06 | 57.3 | 56.8 | 0 |

### chat-stream-slow

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 245 | 1021 | 1313.8 | 1.58 | 43.4 | 43.2 | 0 |
| Go | 238 | 1028.8 | 1260.6 | 2.75 | 103.6 | 103.6 | 0 |

### messages-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 913 | 33.2 | 60.6 | 1.09 | 24.9 | 24.7 | 0 |
| Go | 613 | 51.7 | 98.3 | 1.62 | 78.8 | 77.3 | 0 |

## Binary size

Linux x86_64. The cliproxy-rs binary includes its dashboard; the Go binary does not, because Go downloads its panel separately:

| | cliproxy-rs `d068002` | Go v8.0.10 release |
| --- | --- | --- |
| Binary | 33.0 MB | 69.1 MB |
| Binary, gzip -9 | 13.9 MB | 22.7 MB |
| Release archive | 14.0 MB | 22.9 MB |

The cliproxy-rs binary links glibc and libstdc++ dynamically; BoringSSL is linked in. The Go binary is the official release build (stripped), and its archive also holds two READMEs and the example config.

## What the numbers say

- Memory: cliproxy-rs used about a third of Go's memory at idle (14.8 MB against 45.1 MB) and under load (24 to 25 MB against 57 to 79 MB). With 256 slow streams open it peaked at 43 MB and Go at 104 MB.
- Startup: cliproxy-rs answered its first request 13 ms after launch, Go after 98 ms.
- Non-streaming requests: Go was faster, 1,696 requests per second against 1,403, and used less CPU per request (0.58 ms against 0.71 ms). Both servers were CPU-bound in this test.
- Fast streams: the two were level in chat-stream (956 against 940 streams per second). In messages-stream, where both translate between the Anthropic and OpenAI formats, cliproxy-rs served 913 streams per second against Go's 613, with two thirds of Go's CPU per request and a lower p99 latency (61 ms against 98 ms).
- Slow streams: with 256 streams that each last about a second, throughput is set by the upstream and both kept up; cliproxy-rs used 1.58 ms of CPU per stream against 2.75 ms.

No response failed in any run.

## History

`4abce40`, the first run, three rounds with the same method. cliproxy-rs did not yet set `TCP_NODELAY` on client connections (Go sets it on every connection), so each small SSE write waited for the client's acknowledgement and the fast streaming tests were latency-bound rather than CPU-bound:

| Scenario | cliproxy-rs `4abce40` | cliproxy-rs `d068002` | Go |
| --- | --- | --- | --- |
| chat-stream, streams per second | 627 | 956 | 989 / 940 |
| messages-stream, streams per second | 640 | 913 | 607 / 613 |
| chat, requests per second | 1,402 | 1,403 | 1,677 / 1,696 |
| Idle memory | 13.8 MB | 14.8 MB | 44.6 / 45.1 MB |

The Go column gives the Go result measured next to each run. Raw results: [`bench/results/4abce40.jsonl`](../bench/results/4abce40.jsonl).
