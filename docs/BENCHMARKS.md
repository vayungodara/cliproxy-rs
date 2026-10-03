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
