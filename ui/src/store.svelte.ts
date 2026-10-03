import { api, connect, disconnect, configValue, route, ApiError } from "./api";
import { equal, fieldPath, reconcile, type Data, type Window } from "./core";

export const pages = [
  ["overview", "Overview"],
  ["use", "Use with tools"],
  ["credentials", "Credentials"],
  ["providers", "Provider keys"],
  ["keys", "Client keys"],
  ["models", "Models"],
  ["payload", "Payload rules"],
  ["quotas", "Quotas"],
  ["", ""],
  ["usage", "Usage"],
  ["logs", "Logs"],
  ["", ""],
  ["config", "Configuration"],
  ["plugins", "Plugins"],
  ["system", "System"],
] as const;
const routes = new Set<string>([...pages.map((p) => p[0]).filter(Boolean), "connect"]);
const aliases: Record<string, string> = { oauth: "connect", configuration: "config" };

/** A server read with honest loading, error and not-available states. */
export class Res<T> {
  data = $state.raw<T>();
  error = $state.raw<unknown>(null);
  loading = $state(false);
  #gen = 0;
  constructor(private read: () => Promise<T>) {}
  /** The latest started read wins; an older one finishing later is dropped. */
  async load(quiet = false) {
    const gen = ++this.#gen;
    if (!quiet) this.loading = true;
    try {
      const data = await this.read();
      if (gen === this.#gen) (this.data = data), (this.error = null);
    } catch (e) {
      if (gen === this.#gen) this.error = e;
    } finally {
      if (gen === this.#gen) this.loading = false;
    }
  }
}

function parse(hash: string) {
  let h = hash.slice(1);
  try {
    h = decodeURIComponent(h);
  } catch {}
  const [page = "", ...rest] = h.split("/");
  const name = aliases[page] || page;
  return { page: routes.has(name) ? name : "overview", arg: rest.join("/") };
}

class Store {
  logged = $state(false);
  theme = $state(document.documentElement.dataset.theme || "dark");
  route = $state(parse(location.hash));
  meta = $state({ version: "", commit: "", built: "" });
  /**
   * Which server this is, from its version headers. cliproxy-rs sends X-CPA-VERSION
   * "cliproxy-rs/<version>". Go CLIProxyAPI sends a version ("8.0.10", or "dev" from a source
   * build) together with X-CPA-COMMIT and X-CPA-BUILD-DATE. Go implements the whole v8
   * contract, so only other servers are probed for gaps; nothing Rust-specific is assumed.
   */
  kind = $derived(
    this.meta.version.startsWith("cliproxy-rs") ? "rust" : this.meta.commit || this.meta.built ? "go" : "unknown",
  );
  online = $state(true);
  busy = $state(false);
  dirty = $state(false);
  toast = $state<{ text: string; bad: boolean } | null>(null);
  /** "METHOD /path" → false when the server does not implement it. */
  caps = $state<Record<string, false>>({});
  config = new Res<Data>(() => api("/config"));
  creds: Res<Data[]> = new Res(async (): Promise<Data[]> =>
    reconcile(this.creds.data || [], (await api("/credentials")).files || [], (a) => `${a.name}\u0000${a.auth_index}`),
  );
  plugins = new Res<Data[]>(async () => (await api("/plugins")).plugins || []);
  /** Live quota checks made in this tab, by auth index. */
  quota = $state<Record<string, { at: number; windows: Window[] } | { error: string }>>({});
  #probed = new Set<string>();
  #timer: ReturnType<typeof setTimeout> | undefined;

  async login(secret: string) {
    this.meta = { version: "", commit: "", built: "" };
    connect(secret, {
      unauthorized: () => {
        if (!this.logged) return;
        this.logout();
        this.notify("The management key was rejected. Sign in again.", true);
      },
      missing: (method, path) => (this.caps[`${method} ${path}`] = false),
      meta: (h) => {
        this.online = !!h;
        if (!h) return;
        this.meta.version = h.get("x-cpa-version") || h.get("x-server-version") || this.meta.version;
        this.meta.commit = h.get("x-cpa-commit") || this.meta.commit;
        this.meta.built = h.get("x-cpa-build-date") || this.meta.built;
      },
    });
    try {
      this.config.data = await api("/config");
    } catch (e) {
      disconnect();
      throw new Error(signInHelp(e));
    }
    this.logged = true;
    this.creds.load();
  }
  logout() {
    disconnect();
    this.logged = false;
    this.caps = {};
    this.#probed.clear();
    this.config.data = this.creds.data = this.plugins.data = undefined;
    this.quota = {};
    this.dirty = false;
  }
  can(method: string, path: string) {
    return this.caps[`${method} ${route(path)}`] !== false;
  }
  /**
   * Find unimplemented actions before they are offered. Each probe is a request Go rejects
   * with 400 during input validation, before any side effect: an empty JSON body, or a GET
   * without its required parameter. A server lacking the route answers 404 with no body (or
   * 501/405), which api() records as missing. Go itself is never probed.
   */
  probe(actions: [string, string, string][]) {
    if (this.kind === "go") return;
    for (const [method, path] of actions) {
      const id = `${method} ${path}`;
      if (this.#probed.has(id)) continue;
      this.#probed.add(id);
      api(path, method, method === "GET" ? undefined : {}).catch(() => {});
    }
  }
  go(hash: string) {
    if (location.hash.slice(1) !== hash) location.hash = hash;
  }
  sync() {
    const next = parse(location.hash);
    if (next.page === this.route.page && next.arg === this.route.arg) return;
    if (this.dirty && !confirm("Discard unsaved changes?")) {
      history.replaceState(null, "", `#${this.route.page}${this.route.arg ? `/${this.route.arg}` : ""}`);
      return;
    }
    this.dirty = false;
    if (next.page !== this.route.page) scrollTo(0, 0);
    this.route = next;
  }
  toggleTheme() {
    this.theme = this.theme === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = this.theme;
    try {
      localStorage.setItem("cliproxy-theme", this.theme);
    } catch {}
  }
  notify(text: string, bad = false) {
    this.toast = { text, bad };
    clearTimeout(this.#timer);
    if (!bad) this.#timer = setTimeout(() => (this.toast = null), 4000);
  }
  /** Run one write at a time; report success or the server's error. */
  async act(work: () => Promise<unknown>, success = "") {
    if (this.busy) return false;
    this.busy = true;
    try {
      await work();
      if (success) this.notify(success);
      return true;
    } catch (e) {
      this.notify(e instanceof Error ? e.message : String(e), true);
      return false;
    } finally {
      this.busy = false;
    }
  }
  /** One API write, optionally confirmed, followed by a reread. */
  call(method: string, path: string, body: unknown, done: string, ask = "", after = () => this.creds.load(true)) {
    if (ask && !confirm(ask)) return Promise.resolve(false);
    return this.act(async () => {
      await api(path, method, body);
      await after();
    }, done);
  }
  /**
   * Replace one config value after confirming nobody changed it since it was read. `unset`
   * is what an absent value reads as (an empty list or map unless given).
   */
  async replace(path: string, before: unknown, next: unknown, unset: unknown = Array.isArray(before) ? [] : {}) {
    const url = fieldPath(path);
    const latest = await configValue(url, unset);
    if (!equal(latest, before)) {
      await this.config.load(true);
      throw new Error("This setting changed on the server. Nothing was written; the page now shows the server copy.");
    }
    await api(url, "PUT", next);
    await this.config.load(true);
  }
}

export const store = new Store();

/** What to do about a failed sign-in, for each answer Go and cliproxy-rs give. */
function signInHelp(e: unknown): string {
  if (!(e instanceof ApiError)) return e instanceof Error ? e.message : String(e);
  const m = e.message;
  if (e.status === 404 || /key not set/.test(m))
    return "The management API is off on this server. Set management.secret-key in the server's config.yaml (or start it with MANAGEMENT_PASSWORD), then restart it.";
  if (/remote management disabled/.test(m))
    return "This server accepts the dashboard only from its own machine. Open it there, or set management.allow-remote: true in config.yaml with a strong key.";
  if (/banned/i.test(m)) return `${m}. Too many wrong keys came from this address.`;
  if (e.status === 401)
    return "That key is not right. Use the plain text you set as management.secret-key: on first start the server replaces it in config.yaml with a hash, so the file no longer shows it.";
  return m;
}

/** Repeat a read while the tab is visible; never overlap calls. Returns a cleanup. */
export function every(ms: number, work: () => Promise<unknown>) {
  let running = false;
  const timer = setInterval(async () => {
    if (running || document.hidden || !store.logged) return;
    running = true;
    try {
      await work();
    } finally {
      running = false;
    }
  }, ms);
  return () => clearInterval(timer);
}
