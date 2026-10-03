# Benchmarks

Binary size, memory and throughput of cliproxy-rs and the Go CLIProxyAPI on the same configuration, with a local fake upstream. The numbers come from one small machine and a synthetic load, so read them as a comparison between the two servers on that machine, not as a capacity figure.

## Setup

- cliproxy-rs at commit `4abce40`, release profile (thin LTO, one codegen unit, stripped), built with Rust 1.99.0.
- CLIProxyAPI v8.0.10 (commit `6fecc6e`), the official `linux_amd64` release binary (Go 1.26.4).
- A 2-vCPU virtual machine (Intel Xeon at 2.6 GHz, 3.9 GB of memory) running Debian 12 with Linux 6.1. Other processes on the machine used under 5% of one CPU during the runs.
- Both servers run with the same `config.yaml`: one OpenAI-compatible provider that points at the fake upstream, one client key and an empty credential directory, started with `-local-model`. Each scenario starts a fresh server process.
- The server is pinned to CPU 0. The fake upstream and the load generator share CPU 1.
- The whole run happens in a network namespace with only a loopback interface. Go tries to download its management panel and an Antigravity version file at start; both fail at once there. On a machine with network access, Go's idle memory was about 10 MB higher (56 MB) after those downloads.
- Go writes one access-log line per request to standard output (redirected to a file). cliproxy-rs at this commit writes no access log, which saves it a little CPU in these tests.

The scripts are in [`bench/`](../bench): `upstream/` is the fake OpenAI-compatible upstream, `load/` the load generator, `run.sh` runs every scenario three times and `summary.sh` prints the median of the three rounds. Both helpers use only the Go standard library. The raw results of the run below are in [`bench/results/4abce40.jsonl`](../bench/results/4abce40.jsonl).

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

Median of three rounds.

### Idle

| Server | Startup (ms) | RSS after 15 s (MB) |
| --- | --- | --- |
| cliproxy-rs | 15 | 13.8 |
| Go | 82 | 44.6 |

### chat

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 1402 | 22.5 | 38.9 | 0.71 | 21.7 | 21.7 | 0 |
| Go | 1677 | 18.2 | 43.6 | 0.59 | 57.3 | 56.6 | 0 |

### chat-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 627 | 48.5 | 68.1 | 1.03 | 22.6 | 22.6 | 0 |
| Go | 989 | 32.3 | 62.4 | 1.01 | 57.6 | 56.5 | 0 |

### chat-stream-slow

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 245 | 1019.3 | 1323.4 | 1.64 | 42.4 | 42.2 | 0 |
| Go | 240 | 1025.2 | 1267 | 2.66 | 104.1 | 104.1 | 0 |

### messages-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 640 | 48.1 | 68.2 | 1.06 | 22.7 | 22.7 | 0 |
| Go | 607 | 51.2 | 121 | 1.63 | 79.4 | 78.3 | 0 |

## Binary size

Linux x86_64. The cliproxy-rs binary includes its dashboard; the Go binary does not, because Go downloads its panel separately:

| | cliproxy-rs `4abce40` | Go v8.0.10 release |
| --- | --- | --- |
| Binary | 29.6 MB | 69.1 MB |
| Binary, gzip -9 | 12.6 MB | 22.7 MB |
| Release archive | 12.6 MB | 22.9 MB |

The cliproxy-rs binary links glibc and libstdc++ dynamically; BoringSSL is linked in. The Go binary is the official release build (stripped). The Go server downloads its dashboard on first use instead of embedding it, and its archive also holds two READMEs and the example config.

## What the numbers say

- Memory: cliproxy-rs used about a third of Go's memory at idle (13.8 MB against 44.6 MB) and under load (22 to 23 MB against 57 to 79 MB). With 256 slow streams open, it peaked at 42 MB and Go at 104 MB.
- Startup: cliproxy-rs answered its first request 15 ms after launch, Go after 82 ms.
- Non-streaming requests: Go was faster, 1,677 requests per second against 1,402, and used less CPU per request (0.59 ms against 0.71 ms). Both servers were CPU-bound in this test.
- Fast streams: in chat-stream Go served 989 streams per second and cliproxy-rs 627, without using all of its CPU. cliproxy-rs at this commit does not set `TCP_NODELAY` on client connections, and Go does, so each small SSE write waits for the client's acknowledgement. In a shorter separate run of an earlier commit (`d48b9e0`) built with the option set, cliproxy-rs reached 980 streams per second in chat-stream and 912 in messages-stream, using all of its CPU.
- Translated streams: in messages-stream, where both servers translate between the Anthropic and OpenAI formats, cliproxy-rs served slightly more streams (640 against 607) with about two thirds of Go's CPU per request and a lower p99 latency (68 ms against 121 ms).
- Slow streams: with 256 streams that each last about a second, throughput is set by the upstream and both servers kept up. cliproxy-rs used less CPU per stream (1.64 ms against 2.66 ms).

No response failed in any run.
