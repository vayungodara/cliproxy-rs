# Use cliproxy-rs with your tools

The examples use `http://127.0.0.1:8317` and `your-client-key`. Replace both with the address and client key from `config.yaml`. The dashboard at `/management.html` has a Use with tools page that shows these values with copy buttons. Pick a model ID from the dashboard's Models page or from `GET /v1/models`. Available IDs depend on the accounts connected to your proxy.

## Claude Code

[Claude Code's gateway guide](https://code.claude.com/docs/en/llm-gateway-connect) uses the Anthropic Messages format. Set the base URL without `/v1`; Claude Code adds `/v1/messages`. The token is sent as bearer authorization. Optionally set `ANTHROPIC_MODEL` to an ID from the proxy.

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8317
export ANTHROPIC_AUTH_TOKEN=your-client-key
export ANTHROPIC_MODEL=<model-id>
claude
```

For a persistent personal setup, put this in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8317",
    "ANTHROPIC_AUTH_TOKEN": "your-client-key",
    "ANTHROPIC_MODEL": "<model-id>"
  }
}
```

### Claude Code versions

With a Claude account, cliproxy-rs forwards Claude Code's own identity (its `User-Agent` and SDK and Node versions) when Claude Code is 2.1.280 or newer within major version 2. A release newer than 2.1.280 must send both its `X-Stainless-Package-Version` and `X-Stainless-Runtime-Version` headers, as Claude Code does; its session-title and quota-probe requests are forwarded the same way. With `stabilize-device-profile: true`, 2.1.x releases from 2.1.280 still count as Claude Code, so they are not cloaked, but they go upstream with the stabilized 2.1.280 identity; 2.2 and later are cloaked, and the Stainless requirement does not apply. Other clients, and older Claude Code, are sent upstream as Claude Code 2.1.280. When a `claude-cli` client is not forwarded, the log says so at most once per account and version, naming the account by its auth index (the one the dashboard and the Management API show), and says when missing Stainless headers are the reason:

```text
claude: Claude Code 2.1.220 on credential 1f3a9c0b7d2e4a68 is not passed through; requests use the claude-cli/2.1.280 (external, cli) identity. ...
```

Anthropic refuses Claude Code releases that are too old for a model. The error reaches your client as a 400:

```text
Claude Code 2.1.236 does not support this model; version 2.1.251 or newer is required. Run 'claude update', or update the Claude desktop app, then try again.
```

Its `error.details.error_code` is `claude_code_version_too_old`. From Claude Code itself, run `claude update`. From another client, or from a Claude Code release that is not forwarded, the identity cliproxy-rs sends is older than the model needs. Until a cliproxy-rs release raises it, set a newer one in `config.yaml`. Use the version that `claude --version` prints, and the `X-Stainless-Package-Version` and `X-Stainless-Runtime-Version` headers that release sends:

```yaml
oauth:
  providers:
    claude:
      header-defaults:
        user-agent: "claude-cli/<version> (external, cli)"
        package-version: "<X-Stainless-Package-Version>"
        runtime-version: "<X-Stainless-Runtime-Version>"
```

The older layout spells the same block `claude-header-defaults:` at the top level. The version in `user-agent` also becomes the lowest Claude Code version that is forwarded as itself.

### GPT in Claude Code

Configure an OpenAI API key, then choose an available GPT model from `/v1/models`. You can also use a connected Codex account; read [Accounts and provider terms](../README.md#accounts-and-provider-terms) first. Claude Code sends Messages requests; the proxy translates them to the selected provider's format.

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8317
export ANTHROPIC_AUTH_TOKEN=your-client-key
export ANTHROPIC_MODEL='<gpt-model-id>'
export ANTHROPIC_DEFAULT_HAIKU_MODEL='<gpt-model-id>'
# Example for a model with at least a 200,000-token context window:
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=200000
export CLAUDE_CODE_AUTO_COMPACT_WINDOW=180000
claude
```

`ANTHROPIC_DEFAULT_HAIKU_MODEL` routes Claude Code's small-model calls too; otherwise they may still ask for Haiku. You can use a different available GPT model for that slot. If you use Claude Code's named model tiers, set `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL` and `ANTHROPIC_DEFAULT_FABLE_MODEL` to available proxy models as well.

Check the selected model's provider limit and use a compaction window below it. Claude Code's [environment reference](https://code.claude.com/docs/en/env-vars) defines both variables. `CLAUDE_CODE_MAX_CONTEXT_TOKENS` sets the window Claude Code assumes. The upstream's context limit still applies. `CLAUDE_CODE_AUTO_COMPACT_WINDOW` accepts an integer token count from 100,000 to 1,000,000. Set these variables in Claude Code's environment.

## Codex CLI

[Codex custom model providers](https://developers.openai.com/codex/config-advanced) use the OpenAI Responses format. Put this in `~/.codex/config.toml`, set the environment variable, and replace `<model-id>`. WebSocket support is documented, but it is optional, so this setup uses the normal HTTP endpoint.

```toml
model = "<model-id>"
model_provider = "cliproxy"

[model_providers.cliproxy]
name = "cliproxy-rs"
base_url = "http://127.0.0.1:8317/v1"
env_key = "CLIPROXY_CLIENT_KEY"
wire_api = "responses"
```

```bash
export CLIPROXY_CLIENT_KEY=your-client-key
codex
```

## Gemini CLI

[Gemini CLI's official repository](https://github.com/google-gemini/gemini-cli/issues/1679#issuecomment-3293504913) confirms `GOOGLE_GEMINI_BASE_URL` for a custom Gemini endpoint. `GEMINI_API_KEY` is the documented key variable. The CLI uses Gemini `/v1beta` routes.

```bash
export GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8317
export GEMINI_API_KEY=your-client-key
gemini
```

Pick `<model-id>` with Gemini CLI's `/model` command. Not verified against official docs: a model environment variable for a custom endpoint.

## Amp

[Amp Model Routing](https://ampcode.com/docs/customize/model-routing#custom-url-connections) calls Custom URL connections from Amp's servers. Expose the proxy at a public HTTPS address first. In Model Routing, choose Add, then Custom URL. Use one of these configurations and map an Amp model to `<model-id>`.

```text
Anthropic Messages
API format: anthropic-messages
Base URL: https://proxy.example.com
API key: your-client-key

OpenAI Responses
API format: responses
Base URL: https://proxy.example.com/v1
API key: your-client-key
```

The API key is sent as bearer authorization. Use Check Access, or run:

```bash
amp config model-providers check-access --provider-model <provider/model>
```

## OpenCode

[OpenCode's provider guide](https://opencode.ai/docs/providers#custom-provider) documents custom OpenAI-compatible providers. Add this to `opencode.json`, replace `<model-id>`, then select `cliproxy/<model-id>` with `/models`. This uses OpenAI Chat Completions.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "cliproxy": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "cliproxy-rs",
      "options": {
        "baseURL": "http://127.0.0.1:8317/v1",
        "apiKey": "your-client-key"
      },
      "models": {
        "<model-id>": { "name": "<model-id>" }
      }
    }
  }
}
```

## Factory Droid

[Factory's custom model guide](https://docs.factory.ai/model-independence/byok) uses `~/.factory/settings.json`. This example uses OpenAI Chat Completions. Replace `<model-id>`, then choose it with `/model`.

```json
{
  "customModels": [
    {
      "model": "<model-id>",
      "displayName": "cliproxy-rs",
      "baseUrl": "http://127.0.0.1:8317/v1",
      "apiKey": "your-client-key",
      "provider": "generic-chat-completion-api"
    }
  ]
}
```

For Anthropic Messages, use `provider: "anthropic"` and `baseUrl: "http://127.0.0.1:8317"` instead.

## Cline, Roo Code and Kilo Code

[Cline](https://docs.cline.bot/provider-config/openai-compatible), [Roo Code](https://docs.roocode.com/providers/openai-compatible), and [Kilo Code](https://kilo.ai/docs/ai-providers/openai-compatible) have the same OpenAI Chat Completions fields. Open the extension's provider settings and enter:

```text
Provider or provider API: OpenAI Compatible
Base URL: http://127.0.0.1:8317/v1
API key: your-client-key
Model or model ID: <model-id>
```

Kilo also offers OpenAI Responses and Anthropic Messages custom provider formats. Cline documents an Anthropic Use custom base URL option. Roo Code's cited page verifies only its OpenAI-compatible setup.

## Cursor

[Cursor's current key guide](https://cursor.com/help/models-and-usage/api-keys) verifies Cursor Settings, Models, and the OpenAI key field. It also says every request goes through Cursor's servers, so `127.0.0.1` cannot work. Expose the proxy over HTTPS first.

```text
Cursor Settings > Models
OpenAI API key: your-client-key
Override OpenAI Base URL: https://proxy.example.com/v1
Custom model: <model-id>
```

Not verified against official docs: the current official guide does not document Override OpenAI Base URL or Custom model. Confirm that these fields exist in your installed Cursor version before using this setup.

## Zed

[Zed's API access guide](https://zed.dev/docs/ai/use-api-access#openai-compatible) supports custom OpenAI-compatible providers. Put this in Zed's `settings.json`, replace `<model-id>`, and set the generated key variable. Zed uses Chat Completions by default.

```json
{
  "language_models": {
    "openai_compatible": {
      "cliproxy": {
        "api_url": "http://127.0.0.1:8317/v1",
        "available_models": [
          {
            "name": "<model-id>",
            "display_name": "cliproxy-rs",
            "max_tokens": 128000
          }
        ]
      }
    }
  }
}
```

```bash
export CLIPROXY_API_KEY=your-client-key
```

Set `capabilities.chat_completions` to `false` in the model entry to use OpenAI Responses. Zed also documents `language_models.anthropic_compatible` with `api_url: "http://127.0.0.1:8317"` for Anthropic Messages.

## Continue

[Continue's OpenAI provider guide](https://docs.continue.dev/customize/model-providers/top-level/openai#openai-api-compatible-providers) uses `apiBase`, `apiKey`, and `model` in `config.yaml`. This setup uses OpenAI Chat Completions.

```yaml
name: cliproxy-rs
version: 1.0.0
schema: v1
models:
  - name: cliproxy-rs
    provider: openai
    model: <model-id>
    apiBase: http://127.0.0.1:8317/v1
    apiKey: your-client-key
```

## Aider

[Aider's OpenAI-compatible guide](https://aider.chat/docs/llms/openai-compat.html) documents these variables and the `openai/` model prefix. It uses OpenAI-compatible requests.

```bash
export OPENAI_API_BASE=http://127.0.0.1:8317/v1
export OPENAI_API_KEY=your-client-key
aider --model openai/<model-id>
```

Not verified against official docs: an Anthropic custom-base variable. Aider documents `ANTHROPIC_API_KEY`, but its current options reference does not document `ANTHROPIC_API_BASE`.

## SDKs

All examples replace `<model-id>` with an ID from the proxy.

### OpenAI Python

The [official Python SDK](https://github.com/openai/openai-python) uses OpenAI Responses here.

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8317/v1", api_key="your-client-key")
response = client.responses.create(model="<model-id>", input="Hello")
print(response.output_text)
```

### OpenAI JavaScript

The [official JavaScript SDK](https://github.com/openai/openai-node/blob/main/docs/authentication.md) uses camel-case `baseURL` and `apiKey`.

```javascript
import OpenAI from "openai";

const client = new OpenAI({
  baseURL: "http://127.0.0.1:8317/v1",
  apiKey: "your-client-key",
});
const response = await client.responses.create({ model: "<model-id>", input: "Hello" });
console.log(response.output_text);
```

### Anthropic Python

The [official Python SDK](https://github.com/anthropics/anthropic-sdk-python) uses Anthropic Messages and a base URL without `/v1`.

```python
import anthropic

client = anthropic.Anthropic(base_url="http://127.0.0.1:8317", api_key="your-client-key")
message = client.messages.create(
    model="<model-id>", max_tokens=256,
    messages=[{"role": "user", "content": "Hello"}],
)
print(message.content[0].text)
```

### Anthropic JavaScript

The [official JavaScript SDK](https://github.com/anthropics/anthropic-sdk-typescript) uses `baseURL` and `apiKey`.

```javascript
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic({
  baseURL: "http://127.0.0.1:8317",
  apiKey: "your-client-key",
});
const message = await client.messages.create({
  model: "<model-id>", max_tokens: 256,
  messages: [{ role: "user", content: "Hello" }],
});
console.log(message.content[0].text);
```

### Google Gen AI Python

The [official Python SDK source](https://github.com/googleapis/python-genai/blob/main/google/genai/client.py) accepts `api_key` and `http_options`. It uses Gemini `/v1beta` routes.

```python
from google import genai
from google.genai import types

client = genai.Client(
    api_key="your-client-key",
    http_options=types.HttpOptions(base_url="http://127.0.0.1:8317"),
)
response = client.models.generate_content(model="<model-id>", contents="Hello")
print(response.text)
```

The published Python guide describes custom base URLs only for its enterprise mode. The SDK's current source accepts the combination above for the Gemini Developer API too.

### Google Gen AI JavaScript

The [official JavaScript SDK source](https://github.com/googleapis/js-genai/blob/main/src/client.ts) accepts `apiKey` and `httpOptions.baseUrl`.

```javascript
import { GoogleGenAI } from "@google/genai";

const client = new GoogleGenAI({
  apiKey: "your-client-key",
  httpOptions: { baseUrl: "http://127.0.0.1:8317" },
});
const response = await client.models.generateContent({
  model: "<model-id>", contents: "Hello",
});
console.log(response.text);
```

## Check it works

```bash
curl -sS http://127.0.0.1:8317/v1/models \
  -H 'Authorization: Bearer your-client-key'
```

A `401` response means the client key is wrong. If a cloud-hosted tool cannot connect, use a public HTTPS address such as `https://proxy.example.com` instead of `127.0.0.1`.
