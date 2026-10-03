// Fake provider login endpoints for end-to-end OAuth checks. Never a real provider.
// Answers the token, profile, device-code, key-mint and Claude usage routes that cliproxy-rs and Go call
// during Claude, Codex, Kimi, Meta, xAI and Devin logins, with fake tokens and @example.invalid emails.
// Device flows answer "authorization_pending" on the first poll, then succeed.
// Usage: node scripts/fake-logins.mjs [port]   (default 9102)
import http from "node:http";

const port = Number(process.argv[2] || 9102);
const b64 = (v) => Buffer.from(JSON.stringify(v)).toString("base64url");
const idToken = `${b64({ alg: "none" })}.${b64({
  email: "codex-login@example.invalid",
  "https://api.openai.com/auth": { chatgpt_plan_type: "plus", chatgpt_account_id: "acct-fake-login" },
})}.sig`;
const polls = new Map();
const pending = (key) => {
  const n = (polls.get(key) || 0) + 1;
  polls.set(key, n);
  return n < 2;
};

const routes = {
  // Claude usage, for api-call checks. Echoes the Authorization header it received so a
  // check can confirm the server substituted the credential's token for $TOKEN$.
  "/api/oauth/usage": (req) => ({
    five_hour: { utilization: 37, resets_at: new Date(Date.now() + 3 * 3600_000).toISOString() },
    seven_day: { utilization: 12.5, resets_at: new Date(Date.now() + 4 * 86400_000).toISOString() },
    echo_authorization: req.headers.authorization || "",
  }),
  // Codex
  "/oauth/token": () => ({ id_token: idToken, access_token: "fake-codex-at", refresh_token: "fake-codex-rt", expires_in: 3600 }),
  // Claude
  "/v1/oauth/token": () => ({
    access_token: "sk-ant-oat-fake-login",
    refresh_token: "fake-claude-rt",
    expires_in: 3600,
    account: { uuid: "acct-fake-claude", email_address: "claude-login@example.invalid" },
    organization: { uuid: "org-fake", name: "Fake Org" },
  }),
  "/api/oauth/profile": () => ({
    account: { uuid: "acct-fake-claude", email: "claude-login@example.invalid" },
    organization: { uuid: "org-fake", name: "Fake Org" },
  }),
  "/api/oauth/claude_cli/roles": () => ({ organization_role: "admin", workspace_role: null }),
  // Kimi (device flow)
  "/api/oauth/device_authorization": () => ({
    device_code: "fake-kimi-dc",
    user_code: "KIMI-FAKE",
    verification_uri: "https://kimi.example.invalid/device",
    verification_uri_complete: "https://kimi.example.invalid/device?code=KIMI-FAKE",
    expires_in: 600,
    interval: 1,
  }),
  "/api/oauth/token": () =>
    pending("kimi")
      ? [400, { error: "authorization_pending" }]
      : { access_token: "fake-kimi-at", refresh_token: "fake-kimi-rt", token_type: "Bearer", expires_in: 3600, scope: "s" },
  // Meta (device flow, then API-key mint)
  "/oidc/device/authorization/": () => ({
    device_code: "fake-meta-dc",
    user_code: "META-FAKE",
    verification_uri: "https://meta.example.invalid/device",
    expires_in: 600,
    interval: 1,
  }),
  "/oidc/device/token/": () =>
    pending("meta")
      ? [400, { error: "authorization_pending" }]
      : { access_token: "fake-meta-dca", token_type: "Bearer", expires_in: 3600 },
  "/muse-code/key": () => ({
    api_key: "fake-meta-minted-key",
    base_url: "http://127.0.0.1:9101/v1",
    user_email: "meta-login@example.invalid",
    user_full_name: "Fake Meta User",
  }),
  // xAI (OIDC discovery, then device flow). Discovery names auth.x.ai endpoints; the
  // server's test seam sends those requests here instead.
  "/.well-known/openid-configuration": () => ({
    device_authorization_endpoint: "https://auth.x.ai/oauth2/device/code",
    token_endpoint: "https://auth.x.ai/oauth2/token",
  }),
  "/oauth2/device/code": () => ({
    device_code: "fake-xai-dc",
    user_code: "XAI-FAKE",
    verification_uri: "https://xai.example.invalid/device",
    verification_uri_complete: "https://xai.example.invalid/device?user_code=XAI-FAKE",
    expires_in: 600,
    interval: 1,
  }),
  "/oauth2/token": () =>
    pending("xai")
      ? [400, { error: "authorization_pending" }]
      : {
          access_token: "fake-xai-at",
          refresh_token: "fake-xai-rt",
          id_token: `${b64({ alg: "none" })}.${b64({ email: "xai-login@example.invalid", sub: "xai-fake-user" })}.sig`,
          token_type: "Bearer",
          expires_in: 3600,
        },
  // Devin (PKCE code exchange and profile; the user-status RPC gets {} and the login
  // tolerates its failure)
  "/auth/cli/token": () => ({ token: "fake-devin-session" }),
  "/v3/self": () => ({ user_name: "devin-login", user_id: "u-fake-devin", org_id: "o-fake-devin" }),
};

http
  .createServer(async (req, res) => {
    for await (const _ of req);
    const path = new URL(req.url, "http://x").pathname;
    const route = routes[path];
    const out = route ? route(req) : [404, { error: "not_found" }];
    const [status, body] = Array.isArray(out) ? out : [200, out];
    if (process.env.VERBOSE) console.log(req.method, path, status);
    res.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
  })
  .listen(port, "127.0.0.1", () => console.log(`fake logins on http://127.0.0.1:${port}`));
