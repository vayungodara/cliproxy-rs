import { serverBase, type Data } from "./core";

// The management key lives only in this module's memory. It is never written to storage.
let base = "";
let key = "";
let revision = 0;
let hooks: {
  unauthorized(): void;
  missing(method: string, path: string): void;
  meta(h: Headers | null): void;
} = { unauthorized() {}, missing() {}, meta() {} };

export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
    public code: string,
    public method: string,
    public path: string,
  ) {
    super(message);
  }
  /** The server does not implement this route or method (Rust 501/405, Go's empty 404). */
  get missing() {
    return this.status === 501 || this.status === 405 || (this.status === 404 && !this.code);
  }
}

export function connect(url: string, secret: string, h: typeof hooks) {
  base = serverBase(url);
  key = secret;
  hooks = h;
  revision++;
}
export function disconnect() {
  key = "";
  revision++;
}
export const endpoint = () => `${base}/v8/management`;
export const route = (path: string) => path.split("?")[0];

async function send(path: string, method: string, body?: unknown) {
  if (!key) throw new Error("Sign in with your management key.");
  const at = revision;
  const headers: Record<string, string> = { Authorization: `Bearer ${key}` };
  const yaml = path === "/config.yaml";
  if (body !== undefined && !(body instanceof FormData))
    headers["Content-Type"] = yaml ? "application/yaml" : "application/json";
  let response: Response;
  try {
    response = await fetch(endpoint() + path, {
      method,
      headers,
      body: body === undefined || body instanceof FormData || yaml ? (body as BodyInit) : JSON.stringify(body),
      signal: AbortSignal.timeout(path.includes("/refresh") ? 300_000 : 30_000),
      cache: "no-store",
      redirect: "error",
    });
  } catch (e) {
    if (at === revision) hooks.meta(null);
    throw new Error(
      at !== revision
        ? "Connection changed. Try again."
        : e instanceof Error && e.name === "TimeoutError"
          ? "The server did not answer in time."
          : "Cannot reach the server. Check the URL, CORS policy and TLS.",
    );
  }
  if (at !== revision) throw new Error("Connection changed. Try again.");
  hooks.meta(response.headers);
  return response;
}

export async function api(
  path: string,
  method = "GET",
  body?: unknown,
  format: "json" | "text" | "blob" = "json",
): Promise<any> {
  const response = await send(path, method, body);
  if (!response.ok) {
    const value: Data = await response.json().catch(() => ({}));
    if (response.status === 401) hooks.unauthorized();
    const error = new ApiError(
      String(value.message || value.error || `HTTP ${response.status}`),
      response.status,
      String(value.code || value.error || ""),
      method,
      route(path),
    );
    if (error.missing) {
      error.message = `Not available on this server (${method} ${route(path)} → ${response.status}).`;
      hooks.missing(method, route(path));
    }
    throw error;
  }
  if (format === "blob") return response.blob();
  if (format === "text") return response.text();
  return response.status === 204 ? {} : response.json();
}

/**
 * Ask which methods a route supports without side effects. Go answers OPTIONS with 204 for
 * every route (all supported). Rust answers 405 with an Allow list for routes it serves and
 * 501 for routes it does not. Returns the unsupported methods; empty means unknown or all.
 */
export async function unsupported(path: string): Promise<string[]> {
  const all = ["GET", "POST", "PUT", "PATCH", "DELETE"];
  try {
    const r = await send(path, "OPTIONS");
    if (r.status === 501 || (r.status === 404 && !(await r.text()))) return all;
    const allow = r.headers.get("allow");
    return r.status === 405 && allow ? all.filter((m) => !allow.toUpperCase().includes(m)) : [];
  } catch {
    return [];
  }
}

export async function configValue(path: string, fallback: unknown) {
  try {
    return await api(path);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404 && e.code === "not_found") return fallback;
    throw e;
  }
}

export function download(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob),
    a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
