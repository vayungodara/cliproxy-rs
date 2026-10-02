import { serverBase, type Data } from "./core";

let base = "";
let key = "";
let revision = 0;
let onUnauthorized = () => {};
export let serverVersion = "";
export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
    public code = "",
  ) {
    super(message);
  }
}

export function connect(url: string, secret: string, unauthorized: () => void) {
  base = serverBase(url);
  key = secret;
  onUnauthorized = unauthorized;
  revision++;
}
export function disconnect() {
  key = "";
  revision++;
}

export async function api(
  path: string,
  method = "GET",
  body?: any,
  format: "json" | "text" | "blob" = "json",
): Promise<any> {
  if (!key) throw new Error("Sign in with your management key.");
  const requestRevision = revision;
  const headers: Record<string, string> = { Authorization: `Bearer ${key}` };
  const yaml = path === "/config.yaml";
  if (body !== undefined && !(body instanceof FormData))
    headers["Content-Type"] = yaml ? "application/yaml" : "application/json";
  let response: Response;
  try {
    response = await fetch(`${base}/v8/management${path}`, {
      method,
      headers,
      body:
        body === undefined
          ? undefined
          : body instanceof FormData || yaml
            ? body
            : JSON.stringify(body),
      signal: AbortSignal.timeout(path.includes("/refresh") ? 300_000 : 30_000),
      cache: "no-store",
      redirect: "error",
    });
  } catch (error) {
    if (requestRevision !== revision)
      throw new Error("Connection changed. Try again.");
    throw new Error(
      error instanceof Error && error.name === "TimeoutError"
        ? "Server timed out. Check the connection and retry."
        : "Cannot reach the server. Check the URL, CORS policy, and TLS connection.",
    );
  }
  if (requestRevision !== revision)
    throw new Error("Connection changed. Try again.");
  serverVersion =
    response.headers.get("X-CPA-VERSION") ||
    response.headers.get("X-SERVER-VERSION") ||
    response.headers.get("X-CLIProxyAPI-Version") ||
    serverVersion;
  if (!response.ok) {
    const value: Data = await response.json().catch(() => ({}));
    if (response.status === 401) onUnauthorized();
    throw new ApiError(
      String(value.message || value.error || `HTTP ${response.status}`),
      response.status,
      String(value.code || value.error || ""),
    );
  }
  if (format === "blob") return response.blob();
  if (format === "text") return response.text();
  return response.status === 204 ? {} : response.json();
}

export async function configValue(path: string, fallback: any) {
  try {
    return await api(path);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404 && e.code === "not_found")
      return fallback;
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
