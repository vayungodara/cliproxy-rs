// Refuses to test a server whose credentials could be real. A server without an auth-dir (or
// with a misspelled key, such as management.auth-dir) falls back to ~/.cli-proxy-api, where
// real logins live, and a test sign-in would write fake accounts there. isolated.sh isolates
// the network only, not the filesystem. The server must report an auth-dir under the
// system temp dir: v8 oauth.auth-dir, or the legacy root auth-dir.
import { realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, relative, sep } from "node:path";

/**
 * The real path of `dir` when it is a strict descendant of the real system temp dir; throws
 * otherwise. Symlinks are resolved first, so /tmp/x pointing at ~/.cli-proxy-api is refused,
 * and a path that does not resolve is refused too.
 */
export function privateDir(dir) {
  let real, tmp;
  try {
    tmp = realpathSync(tmpdir());
    real = isAbsolute(dir) ? realpathSync(dir) : "";
  } catch (e) {
    throw new Error(`refusing to run: the auth-dir "${dir}" does not resolve (${e.code || e.message})`);
  }
  const rel = real && relative(tmp, real);
  if (!rel || rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel))
    throw new Error(`refusing to run: the auth-dir "${dir}" is ${real ? `"${real}", ` : ""}not a private directory under ${tmp}`);
  return real;
}

/** `url` is the dashboard page or the server base; the API is looked up beside it. */
export async function assertPrivateAuthDir(url, key) {
  const r = await fetch(new URL("v8/management/config", new URL(".", url.endsWith("/") || /\.html$/.test(url) ? url : `${url}/`)), {
    headers: { Authorization: `Bearer ${key}` },
  });
  if (!r.ok) throw new Error(`refusing to run: GET /v8/management/config answered ${r.status}, so the auth-dir is unknown`);
  const config = await r.json();
  return privateDir(String(config.oauth?.["auth-dir"] ?? config["auth-dir"] ?? ""));
}
