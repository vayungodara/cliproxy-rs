// Refuses to test a server whose credentials could be real. A server without an auth-dir (or
// with a misspelled key, such as management.auth-dir) falls back to ~/.cli-proxy-api, where
// real logins live, and a test sign-in would write fake accounts there. isolated.sh isolates
// the network only, not the filesystem. The server must report an auth-dir under the
// system temp dir: v8 oauth.auth-dir, or the legacy root auth-dir.
import { tmpdir } from "node:os";
import { isAbsolute, normalize, sep } from "node:path";

/** `url` is the dashboard page or the server base; the API is looked up beside it. */
export async function assertPrivateAuthDir(url, key) {
  const r = await fetch(new URL("v8/management/config", new URL(".", url.endsWith("/") || /\.html$/.test(url) ? url : `${url}/`)), {
    headers: { Authorization: `Bearer ${key}` },
  });
  if (!r.ok) throw new Error(`refusing to run: GET /v8/management/config answered ${r.status}, so the auth-dir is unknown`);
  const config = await r.json();
  const dir = String(config.oauth?.["auth-dir"] ?? config["auth-dir"] ?? "");
  const tmp = normalize(tmpdir()) + sep;
  if (!isAbsolute(dir) || !normalize(dir).startsWith(tmp) || dir.includes(".cli-proxy-api"))
    throw new Error(`refusing to run: the server's auth-dir is "${dir}", not a private directory under ${tmp}`);
  return dir;
}
