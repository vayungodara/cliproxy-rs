# The README demo: one Responses request and one Messages request through cliproxy-rs
# to a Chat Completions-only provider, with a 429 on the first key.
#
# The provider is docs/demo/mock_upstream.py on 127.0.0.1:8399, so the recording needs
# no API key and sends nothing to the internet. It answers 429 with Retry-After: 60 to
# the first key that sends a request and 200 to the other, so the recording does not
# depend on which key the proxy tries first. docs/img/demo.gif was recorded with
# cliproxy-rs 0.4.2, which tried keyA first.
#
# Record it from the repository root with `cliproxy` on PATH and ports 8317 and 8399 free:
#
#   vhs docs/demo/demo.tape

Output docs/img/demo.gif

Require cliproxy
Require curl
Require jq
Require python3

Set Shell bash
Set FontFamily "JetBrains Mono"
Set FontSize 19
Set Width 1280
Set Height 660
Set Padding 30
Set Theme "TokyoNight"
Set TypingSpeed 28ms

Hide
Type ". docs/demo/start.sh && clear" Enter
Sleep 2s
Show

Type "# Codex CLI speaks the Responses API. Send one request to the proxy:" Enter
Sleep 600ms
Type "curl -s 127.0.0.1:8317/v1/responses \" Enter
Type `  -H "Authorization: Bearer $CLIPROXY_CLIENT_KEY" \` Enter
Type `  --json '{"model":"demo","input":"Say hello"}' | jq -r '.output[].content[].text'` Sleep 400ms Enter
Sleep 2s

Type "# What the provider received: Chat Completions, and a second key after the 429" Enter
Sleep 600ms
Type "cat $UPSTREAM_LOG" Sleep 300ms Enter
Sleep 3s

Type "# Claude Code speaks Messages. Same provider, and the first key is still cooling:" Enter
Sleep 600ms
Type "curl -s 127.0.0.1:8317/v1/messages -H 'anthropic-version: 2023-06-01' \" Enter
Type `  -H "x-api-key: $CLIPROXY_CLIENT_KEY" \` Enter
Type `  --json '{"model":"demo","max_tokens":64,"messages":[{"role":"user","content":"Say hello"}]}' \` Enter
Type "  | jq -r '.content[].text'" Sleep 400ms Enter
Sleep 1500ms
Type "cat $UPSTREAM_LOG" Sleep 300ms Enter
Sleep 6s

Hide
Type "demo_stop" Enter
