import { api, connect, disconnect, unsupported, configValue } from "./api";
import { equal, fieldPath, reconcile, type Data } from "./core";

export const pages = [
  ["overview", "Overview"],
  ["credentials", "Credentials"],
  ["providers", "Providers"],
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
  constructor(private read: () => Promise<T>) {}
  async load(quiet = false) {
    if (!quiet) this.loading = true;
    try {
      this.data = await this.read();
      this.error = null;
    } catch (e) {
      this.error = e;
    } finally {
      this.loading = false;
    }
  }
}

function parse(hash: string) {
  const [page = "", ...rest] = decodeURIComponent(hash.replace(/^#/, "")).split("/");
  const name = aliases[page] || page;
  return { page: routes.has(name) ? name : "overview", arg: rest.join("/") };
}

class Store {
  logged = $state(false);
  server = $state(location.origin);
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
  #probed = new Set<string>();
  #timer: ReturnType<typeof setTimeout> | undefined;

  async login(server: string, secret: string) {
    connect(server, secret, {
      unauthorized: () => {
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
      throw e;
    }
    this.server = server;
    this.logged = true;
    this.creds.load();
  }
  logout() {
    disconnect();
    this.logged = false;
    this.caps = {};
    this.#probed.clear();
    this.config.data = this.creds.data = this.plugins.data = undefined;
    this.dirty = false;
  }
  can(method: string, path: string) {
    return this.caps[`${method} ${path}`] !== false;
  }
  /** Probe routes once per session so unsupported actions render disabled, not broken. */
  probe(...paths: string[]) {
    if (this.kind === "go") return;
    for (const path of paths) {
      if (this.#probed.has(path)) continue;
      this.#probed.add(path);
      unsupported(path).then((methods) => methods.forEach((m) => (this.caps[`${m} ${path}`] = false)));
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
  /** Replace one config list after confirming nobody changed it since it was read. */
  async replace(path: string, before: unknown, next: unknown) {
    const url = fieldPath(path);
    const latest = await configValue(url, Array.isArray(before) ? [] : {});
    if (!equal(latest, before))
      throw new Error("This setting changed on the server since you opened it. Nothing was written; review and try again.");
    await api(url, "PUT", next);
    await this.config.load(true);
  }
}

export const store = new Store();

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
