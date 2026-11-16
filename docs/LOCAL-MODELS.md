# Local models with Codex CLI

Codex CLI only sends Responses requests. cliproxy-rs translates them to Chat Completions, so Codex can use a model served on your own machine by llama.cpp (`llama-server`), Ollama or vLLM through an ordinary `openai-compatibility` entry.

## What was tested

Tested on 14 and 16 November 2026, on GitHub-hosted `ubuntu-24.04` runners: 4 vCPUs, 16 GB of memory, no GPU.

| | Version |
| --- | --- |
| cliproxy-rs | 0.4.2 (release binary) |
| Codex CLI | 0.169.0 (npm) |
| llama.cpp | b12377 (`llama-b12377-bin-ubuntu-x64`, version 1.0.0-dev) |
| Ollama | 0.40.16 |
| vLLM | 0.33.0 CPU wheel (`vllm-0.33.0+cpu`, torch 2.14.0+cpu) |

Every server passed four checks sent to cliproxy-rs's `/v1/responses`: a plain answer, a streamed answer with usage, a streamed tool call with the right arguments, and the turn that sends the tool result back. Then Codex CLI ran a two-step task, `codex exec` with "run `cat note.txt` and reply with the secret word", once through cliproxy-rs and once against each server's own `/v1/responses`:

| Server and model | Through cliproxy-rs | Codex straight to the server | Time through cliproxy-rs |
| --- | --- | --- | --- |
| llama.cpp, Qwen2.5-7B-Instruct Q4_K_M | Passed | Passed | 457 s |
| Ollama, `qwen2.5:7b` | Passed | Failed: the model asked for permissions instead of running the command | 430 s |
| vLLM, Qwen2.5-3B-Instruct (bf16) | Passed 2 of 4 | Passed 4 of 4 | 47 to 231 s |

With Qwen2.5-1.5B, every Codex turn completed on all three servers, with and without the proxy, but the model never ran the command. Use 7B or larger if you can. A 7B model in bf16 does not fit in 16 GB with vLLM on CPU, so vLLM was tested with 3B.

## cliproxy-rs config

Add one entry to `config.yaml`. Local servers need no API key, but the entry must have a `keys` list: write `keys: []`. An entry without `keys` stops the config from loading ("api-keys.openai-compatibility[0].keys must be a list"), as in CLIProxyAPI. With an empty list the proxy sends no `Authorization` header.

```yaml
api-keys:
  openai-compatibility:
    - name: local
      base-url: "http://127.0.0.1:8080/v1"   # llama.cpp; Ollama: :11434/v1, vLLM: :8000/v1
      keys: []
      models:
        - name: "qwen2.5-7b-instruct"         # the model name the server reports at /v1/models
          alias: "local"
```

## Codex CLI config

In `~/.codex/config.toml`:

```toml
model = "local"
model_provider = "cliproxy"

[model_providers.cliproxy]
name = "cliproxy-rs"
base_url = "http://127.0.0.1:8317/v1"
env_key = "CLIPROXY_CLIENT_KEY"
wire_api = "responses"
stream_idle_timeout_ms = 900000
```

The tests set `stream_idle_timeout_ms` to 15 minutes so that slow prompt processing on a CPU does not trip Codex's stream timeout. Codex warns that it has no model metadata for `local` and uses its defaults; the turns complete anyway.

Codex's first request is about 10,000 tokens of instructions and tool definitions, and the two-step task above usually sent about 20,000 input tokens in total. On a small model you can make it shorter by turning off Codex's multi-agent and goal tools:

```toml
[features]
multi_agent = false
goals = false
```

With that, the vLLM 3B task sent about 14,300 input tokens instead of 20,100.

## Servers

Give each server a context of at least 16,384 tokens; the tests used 16,384. Codex's first request alone does not fit in 8,192.

llama.cpp needs `--jinja` for tool calls. `-np 1` gives the one slot the whole context:

```sh
llama-server -hf Qwen/Qwen2.5-7B-Instruct-GGUF:Q4_K_M --jinja -c 16384 -np 1 \
  --host 127.0.0.1 --port 8080 --alias qwen2.5-7b-instruct
```

Ollama needs nothing for tool calls. Set the context length explicitly:

```sh
OLLAMA_CONTEXT_LENGTH=16384 ollama serve
ollama pull qwen2.5:7b      # model name "qwen2.5:7b" in the cliproxy-rs entry
```

vLLM needs a tool call parser that matches the model; Qwen2.5 uses `hermes`:

```sh
VLLM_CPU_KVCACHE_SPACE=4 vllm serve Qwen/Qwen2.5-3B-Instruct \
  --served-model-name qwen2.5-3b-instruct --max-model-len 16384 --dtype bfloat16 \
  --enable-auto-tool-choice --tool-call-parser hermes --host 127.0.0.1 --port 8000
```

On the test runners, vLLM took about 3 minutes to start, llama.cpp about a minute with the 7B download, and Ollama a few seconds before the model pull.

## Known limits

- vLLM with Qwen2.5-3B passed the Codex task in 2 of 4 attempts through cliproxy-rs and in 4 of 4 when Codex talked to vLLM's own `/v1/responses`. In the failed attempts the model called the wrong Codex tools, such as `view_image` and the multi-agent tools, or answered without running the command. Through the proxy the model sees Codex's tools as Chat Completions functions: tool namespaces become functions named like `multi_agent_v1__spawn_agent`, and the built-in `web_search` tool is not sent. With the multi-agent and goal tools turned off, the command ran in all 3 attempts and 2 of the 3 answers named the word. The 7B models on llama.cpp and Ollama passed through the proxy in the one attempt each had.
- Through cliproxy-rs, vLLM's usage showed no cached input tokens, while its own `/v1/responses` did. llama.cpp and Ollama reported cached tokens through the proxy.
- Small models often stop to ask for permissions or reply without using a tool; that happened with and without the proxy.
- On 4 CPU cores, a two-step Codex task with a 7B model takes several minutes. Most of it is processing the 10,000-token prompt.
