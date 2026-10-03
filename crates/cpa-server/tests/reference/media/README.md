# Image and video goldens

Replayed by `tests/media_go.rs`. `main.go` starts the unmodified Go server binary
(`cmd/server` at CLIProxyAPI `6fecc6e`, run with `-local-model`) against a scripted local
capture server, sends each scenario from `scenarios.go` in order, and records the status,
selected response headers, the body and every raw upstream request. All keys are fake and
every base URL points at the capture server. Run it inside a loopback-only network
namespace anyway:

```sh
# in the CLIProxyAPI tree
go build -o /tmp/cpa-go-server ./cmd/server
# in a module that replaces github.com/router-for-me/CLIProxyAPI/v8 with that tree
cp main.go scenarios.go "$gen/"
cd "$gen" && go run . /tmp/cpa-go-server ../../fixtures/media_go.json
```

Scenarios share one server, so order matters: video IDs created early are polled later
(credential pinning), and the 429 cases run last because they cool their model down.
Replies with `delay_ms` outlast the 1s keep-alive intervals in the shared config.
The `models_grok_shell*` cases cover the Grok Shell `GET /v1/models` catalog, built from
the same registry (xAI catalog, xAI image and video builtins, compat models).

Normalized on both sides: the capture address (`UPSTREAM`), Go's random multipart
boundary (`BOUNDARY`), with each group of form parts Go writes in map order sorted,
timestamps from the clock (`"<now>"`), generated `video_<id>` IDs, and the two xAI keys
as `XAI-KEY-<n>` in order of first use. Credential IDs hash the base URL, which holds the
random capture port, so which key sorts first changes between runs. Request bodies that
are not valid UTF-8 are stored in `body_b64`.
