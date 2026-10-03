# Benchmarks

Binary size, memory and throughput of cliproxy-rs and the Go CLIProxyAPI on the same configuration, with a local fake upstream. The numbers come from one small machine and a synthetic load, so read them as a comparison between the two servers on that machine, not as a capacity figure.

## Setup

- cliproxy-rs at commit `50b9e80`, release profile (thin LTO, one codegen unit, stripped), built with Rust 1.99.0. Earlier runs at `d068002` and `4abce40` are kept under History.
- CLIProxyAPI v8.0.10 (commit `6fecc6e`), the official `linux_amd64` release binary (Go 1.26.4).
- A 2-vCPU virtual machine (Intel Xeon at 2.6 GHz, 3.9 GB of memory) running Debian 12 with Linux 6.1. Other processes on the machine used under 5% of one CPU during the runs.
- Both servers run with the same `config.yaml`: one OpenAI-compatible provider that points at the fake upstream, one client key and an empty credential directory, started with `-local-model`. Each scenario starts a fresh server process.
- The server is pinned to CPU 0. The fake upstream and the load generator share CPU 1.
- The whole run happens in a network namespace with only a loopback interface. Go tries to download its management panel and an Antigravity version file at start; both fail at once there. On a machine with network access, Go's idle memory was about 10 MB higher (56 MB) after those downloads.
- Both servers write one access-log line per request to standard output (redirected to a file). cliproxy-rs writes them since `b944482`; the `d068002` run below had no access log on the Rust side.
- The machine is a shared virtual machine, and results move between runs: the same Go binary handled 1,696 non-streaming requests per second in the `d068002` run and 1,859 in this one. Compare the two servers within one run, not across runs.

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

Median of three rounds, commit `50b9e80`.

### Idle

| Server | Startup (ms) | RSS after 15 s (MB) |
| --- | --- | --- |
| cliproxy-rs | 16 | 17.4 |
| Go | 46 | 44.1 |

### chat

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 1307 | 23.6 | 41.9 | 0.76 | 24.9 | 24.9 | 0 |
| Go | 1859 | 16.4 | 37.5 | 0.53 | 58.6 | 57 | 0 |

### chat-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 944 | 32.3 | 57.1 | 1.05 | 26.1 | 26 | 0 |
| Go | 1038 | 30.8 | 54.8 | 0.96 | 58 | 57.1 | 0 |

### chat-stream-slow

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 243 | 1018.7 | 1357.9 | 1.66 | 49.5 | 49 | 0 |
| Go | 240 | 1026.5 | 1269.8 | 2.53 | 105.1 | 101.9 | 0 |

### messages-stream

| Server | Requests/s | p50 (ms) | p99 (ms) | CPU ms per request | Peak RSS (MB) | RSS 10 s later (MB) | Failed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cliproxy-rs | 830 | 36.6 | 66 | 1.2 | 26.6 | 26.5 | 0 |
| Go | 657 | 48.8 | 85.5 | 1.52 | 78.3 | 77 | 0 |

## Binary size

Linux x86_64. The cliproxy-rs binary includes its dashboard; the Go binary does not, because Go downloads its panel separately:

| | cliproxy-rs `50b9e80` | Go v8.0.10 release |
| --- | --- | --- |
| Binary | 35.9 MB | 69.1 MB |
| Binary, gzip -9 | 15.1 MB | 22.7 MB |
| Release archive | 15.2 MB | 22.9 MB |

The cliproxy-rs binary links glibc and libstdc++ dynamically; BoringSSL is linked in. The Go binary is the official release build (stripped), and its archive also holds two READMEs and the example config.

## What the numbers say

- Non-streaming requests: Go is faster. It handled 1,859 requests per second against cliproxy-rs's 1,307, about 42% more, and used less CPU per request (0.53 ms against 0.76 ms). Both servers were CPU-bound in this test.
- Fast streams: Go was also ahead on plain streams, 1,038 against 944 per second. In messages-stream, where both translate between the Anthropic and OpenAI formats, cliproxy-rs served 830 streams per second against Go's 657, with less CPU per request (1.20 ms against 1.52 ms) and a lower p99 latency (66 ms against 86 ms).
- Slow streams: with 256 streams that each last about a second, throughput is set by the upstream and both kept up; cliproxy-rs used 1.66 ms of CPU per stream against 2.53 ms.
- Memory: cliproxy-rs used about 40% of Go's memory at idle (17.4 MB against 44.1 MB) and under load (25 to 27 MB against 58 to 78 MB). With 256 slow streams open it peaked at 50 MB and Go at 105 MB.
- Startup: cliproxy-rs answered its first request 16 ms after launch, Go after 46 ms.

No response failed in any run.

## History

Three runs with the same method. The Go column gives the Go result measured in each run, which shows how much the machine itself varied.

| Scenario | `4abce40` | `d068002` | `50b9e80` | Go, per run |
| --- | --- | --- | --- | --- |
| chat, requests per second | 1,402 | 1,403 | 1,307 | 1,677 / 1,696 / 1,859 |
| chat-stream, streams per second | 627 | 956 | 944 | 989 / 940 / 1,038 |
| messages-stream, streams per second | 640 | 913 | 830 | 607 / 613 / 657 |
| Idle memory | 13.8 MB | 14.8 MB | 17.4 MB | 44.6 / 45.1 / 44.1 MB |
| Binary | 29.6 MB | 33.0 MB | 35.9 MB | 69.1 MB |

- `4abce40`: cliproxy-rs did not yet set `TCP_NODELAY` on client connections (Go sets it on every connection), so each small SSE write waited for the client's acknowledgement and the fast streaming tests were latency-bound rather than CPU-bound.
- `d068002`: `TCP_NODELAY` set; no access log on the Rust side yet.
- `50b9e80`: cliproxy-rs now writes Go's access log, request-log capture is wired in (off in this config), and usage reporting follows Go's contract. Non-streaming throughput fell about 7% and translated streams about 9% against `d068002`, while the same Go binary measured about 10% faster in this run, so the gap on non-streaming requests grew from about 20% to about 42%.

Raw results: [`bench/results/`](../bench/results), one file per commit.
