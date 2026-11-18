# The README demo

[`docs/img/demo.gif`](../img/demo.gif) is the terminal recording in the README. It sends two requests through cliproxy-rs to the model alias `demo`: a Responses request, as Codex CLI does, and a Messages request, as Claude Code does.

The provider is a local stand-in, [`mock_upstream.py`](mock_upstream.py), on `127.0.0.1:8399`. It answers in the Chat Completions format. The first key that reaches it gets 429 with `Retry-After: 60` for the whole run, and any other key gets 200. It prints one line for each POST it receives, and the recording shows that log:

```text
POST /v1/chat/completions  keyA  ->  429
POST /v1/chat/completions  keyB  ->  200
POST /v1/chat/completions  keyB  ->  200
```

The first two lines are the Responses request: the first key answered 429, and the proxy sent the request again with the second key. The third line is the Messages request, which went to the second key because the first was still cooling.

The GIF was recorded with cliproxy-rs 0.4.2. On 18 November 2026, the same two requests through 0.4.4 gave the same provider log.

The README also has a recording of Codex CLI on a real provider, under [API keys](../../README.md#api-keys-codex-cli-and-mixed-providers).

## Files

| File | What it does |
| --- | --- |
| [`demo.tape`](demo.tape) | The [vhs](https://github.com/charmbracelet/vhs) script that types the commands and writes the GIF |
| [`start.sh`](start.sh) | Starts the mock and the proxy, with the config and both logs in a temporary directory, and waits until both answer. `demo_stop` stops them and removes the directory |
| [`config.yaml`](config.yaml) | The proxy config: the mock as one OpenAI-compatible provider, with two dummy keys behind the alias `demo` |
| [`mock_upstream.py`](mock_upstream.py) | The stand-in provider. It needs only Python's standard library and listens on `127.0.0.1` |

## Record it again

You need vhs, bash, curl, jq, python3 and `cliproxy` on your `PATH`, with ports 8317 and 8399 free. vhs itself needs `ttyd` and `ffmpeg`; see [its installation notes](https://github.com/charmbracelet/vhs#installation). No API key is needed. From the repository root:

```sh
vhs docs/demo/demo.tape
```

Record in a clean environment. `start.sh` starts the proxy inside the temporary directory, so it loads no `.env` file from yours, and it unsets the variables that would make the proxy take its config from Home or a remote store. Other settings in your shell still apply.

`start.sh` returns non-zero, with nothing left running, when a file or the binary is missing, a port is taken, or the proxy and the mock do not both answer. The tape's shell exits in that case.

## Run it without vhs

From the repository root, start the demo, send the first request and read the provider's log, which then has the first two lines above. The parentheses run it in a subshell, so a failed start stops there and the demo's client key does not stay in your shell:

```sh
(
  . docs/demo/start.sh || exit 1
  curl -s 127.0.0.1:8317/v1/responses \
    -H "Authorization: Bearer $CLIPROXY_CLIENT_KEY" \
    --json '{"model":"demo","input":"Say hello"}'
  echo
  cat "$UPSTREAM_LOG"
  demo_stop
)
```

Set `CLIPROXY_BIN` to use a binary that is not on your `PATH`.
