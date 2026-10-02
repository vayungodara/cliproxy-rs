<script lang="ts">
  import { onMount, tick } from "svelte";
  import {
    api,
    connect,
    disconnect,
    configValue,
    download,
    serverVersion,
  } from "./api";
  import {
    cleanEvent,
    equal,
    fieldPath,
    metrics,
    quotaWindows,
    readPath,
    type Data,
    type Event,
  } from "./core";
  import Icon from "./Icon.svelte";
  import Editor from "./Editor.svelte";

  const pages = [
    ["overview", "Overview", "Traffic, accounts, and the pulse of your proxy."],
    [
      "credentials",
      "Credentials",
      "Your accounts. One place to keep them healthy.",
    ],
    [
      "oauth",
      "Connect an account",
      "Sign in with a provider, without handling tokens.",
    ],
    [
      "providers",
      "Providers",
      "Upstream API keys, endpoints, and routing policies.",
    ],
    ["keys", "Client keys", "Control which applications can use your proxy."],
    [
      "models",
      "Models",
      "Give models a familiar name. Keep routing deliberate.",
    ],
    [
      "payload",
      "Payload rules",
      "Shape requests before they reach the provider.",
    ],
    ["quotas", "Quotas", "Know your headroom before you hit a limit."],
    [
      "configuration",
      "Configuration",
      "Fine-tune the proxy. Review every change.",
    ],
    ["logs", "Logs", "Follow the server, from first request to last byte."],
    [
      "plugins",
      "Plugins",
      "Extend your proxy with trusted, native integrations.",
    ],
    [
      "system",
      "System",
      "Connection details, capabilities, and release information.",
    ],
  ];
  const families = [
    "claude",
    "codex",
    "gemini",
    "vertex",
    "openai-compatibility",
    "interactions",
    "xai",
    "meta",
  ];
  const oauthProviders = [
    "claude",
    "codex",
    "antigravity",
    "kimi",
    "kimi-ai",
    "xai",
    "devin",
    "meta",
  ];
  let route = $state(location.hash.slice(1) || "overview");
  let page = $derived(pages.find((p) => p[0] === route) || pages[0]);
  let logged = $state(false),
    busy = $state(false),
    loading = $state(false),
    online = $state(false);
  let server = $state(location.origin),
    secret = $state(""),
    loginError = $state("");
  let config = $state<Data>({}),
    accounts = $state<Data[]>([]),
    plugins = $state<Data[]>([]),
    store = $state<Data[]>([]);
  let providerUsage = $state<
    Record<string, { success: number; failed: number }>
  >({});
  let error = $state(""),
    toast = $state(""),
    checked = $state(""),
    version = $state("");
  let theme = $state(document.documentElement.dataset.theme || "dark");
  let editor = $state<{
    path: string;
    value: any;
    title: string;
    yaml?: boolean;
  } | null>(null);
  let search = $state(""),
    family = $state("claude"),
    channel = $state("claude");
  let groupName = $state(""),
    upstreamKey = $state(""),
    upstreamUrl = $state("");
  let clientKey = $state(""),
    revealKeys = $state(false);
  let aliasName = $state(""),
    aliasTarget = $state(""),
    exclusion = $state(""),
    fork = $state(false),
    definitions = $state<Data[]>([]);
  let ruleKind = $state("default"),
    ruleModel = $state("*"),
    ruleProtocol = $state("openai"),
    ruleParams = $state('{\n  "temperature": 0.7\n}');
  let oauthProvider = $state("codex"),
    oauth = $state<Data | null>(null),
    callback = $state(""),
    oauthState = $state("");
  let live = $state(false),
    tail = $state(true),
    events = $state<Event[]>([]),
    clock = $state(Date.now());
  let lines = $state<string[]>([]),
    cursor = "",
    logFilter = $state(""),
    logFiles = $state<Data[]>([]),
    requestId = $state(""),
    requestLog = $state("");
  let quotas = $state<Record<string, Data>>({}),
    quotaErrors = $state<Record<string, string>>({});
  let latest = $state<Data | null>(null),
    details = $state<Data | null>(null),
    credModels = $state<Data[]>([]);
  let logPane = $state<HTMLDivElement>();
  let credentialNote = $state(""),
    credentialPriority = $state(0),
    credentialProxy = $state("");
  let fixtures = $state(false),
    fixtureAccounts: Data[] = [],
    fixtureEvents: Event[] = [];
  let pollBusy = false,
    toastTimer: ReturnType<typeof setTimeout>;
  let allAccounts = $derived([...accounts, ...fixtureAccounts]);
  let shownAccounts = $derived(
    allAccounts.filter((a) =>
      `${a.name} ${a.email || ""} ${a.provider || a.type}`
        .toLowerCase()
        .includes(search.toLowerCase()),
    ),
  );
  let stats = $derived(metrics([...events, ...fixtureEvents], clock));
  let groups = $derived(readPath(config, `api-keys/${family}`, []) as Data[]);
  let clientKeys = $derived(
    readPath(config, "access/api-keys", []) as string[],
  );
  let aliases = $derived(
    readPath(config, `oauth/model-alias/${channel}`, []) as Data[],
  );
  let excluded = $derived(
    readPath(config, `oauth/excluded-models/${channel}`, []) as string[],
  );
  let payload = $derived(readPath(config, "requests/payload", {}) as Data);
  let visibleLines = $derived(
    lines.filter((line) =>
      line.toLowerCase().includes(logFilter.toLowerCase()),
    ),
  );
  let providerMix = $derived(
    [...new Set(allAccounts.map((a) => provider(a)))].map((name) => ({
      name,
      count: [...events, ...fixtureEvents].filter((e) => e.provider === name)
        .length,
      accounts: allAccounts.filter((a) => provider(a) === name).length,
    })),
  );
  let chartMax = $derived(Math.max(10, ...stats.bins));
  let chartPoints = $derived(
    stats.bins
      .map((n, i) => `${48 + i * (730 / 29)},${210 - (n / chartMax) * 160}`)
      .join(" "),
  );

  function provider(a: Data): string {
    return String(a.provider || a.type || "unknown").replace(
      "anthropic",
      "claude",
    );
  }
  function health(a: Data): string {
    return a.disabled
      ? "Disabled"
      : a.unavailable || a.status === "error"
        ? "Needs attention"
        : a.status === "active" || a.status === "ok"
          ? "Healthy"
          : a.status || "Unknown";
  }
  function stamp(value: string | number) {
    const date = new Date(value);
    return Number.isNaN(date.valueOf())
      ? "Not reported"
      : new Intl.DateTimeFormat(undefined, {
          month: "short",
          day: "numeric",
          hour: "2-digit",
          minute: "2-digit",
        }).format(date);
  }
  function message(e: unknown) {
    return e instanceof Error ? e.message : String(e);
  }
  function storeText(value: string) {
    // Decode upstream store descriptions into text only, never insert remote HTML.
    return (
      new DOMParser().parseFromString(value || "", "text/html").body
        .textContent || ""
    );
  }
  function notify(text: string) {
    toast = text;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = ""), 4500);
  }
  function toggleTheme() {
    theme = theme === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = theme;
    try {
      localStorage.setItem("cliproxy-theme", theme);
    } catch {}
  }
  function navigate(next: string) {
    if (editor && !confirm("Close the editor and discard any unsaved changes?"))
      return;
    editor = null;
    details = null;
    search = "";
    error = "";
    toast = "";
    clearTimeout(toastTimer);
    route = next;
    location.hash = next;
    window.scrollTo(0, 0);
    load();
  }
  function logout() {
    disconnect();
    logged = false;
    online = false;
    live = false;
    config = {};
    accounts = [];
    plugins = [];
    providerUsage = {};
    store = [];
    definitions = [];
    upstreamKey = "";
    clientKey = "";
    revealKeys = false;
    callback = "";
    events = [];
    lines = [];
    quotas = {};
    editor = null;
    oauth = null;
    latest = null;
    requestLog = "";
    details = null;
  }
  async function login() {
    busy = true;
    loginError = "";
    try {
      connect(server, secret, () => {
        logout();
        loginError = "Management key expired or was rejected. Sign in again.";
      });
      config = await api("/config");
      secret = "";
      logged = true;
      online = true;
      version = serverVersion;
      await load();
    } catch (e) {
      loginError = message(e);
      disconnect();
    } finally {
      busy = false;
    }
  }
  async function action(work: () => Promise<void>, success = "") {
    if (busy) return;
    const actionRoute = route;
    busy = true;
    error = "";
    try {
      await work();
      if (success && route === actionRoute) notify(success);
    } catch (e) {
      if (route === actionRoute) error = message(e);
    } finally {
      busy = false;
    }
  }
  async function load() {
    if (!logged) return;
    loading = true;
    const currentRoute = route;
    try {
      const [c, a] = await Promise.all([api("/config"), api("/credentials")]);
      if (!logged) return;
      config = c;
      accounts = a.files || [];
      online = true;
      version = serverVersion;
      checked = new Date().toISOString();
      if (["plugins", "oauth", "quotas"].includes(currentRoute))
        plugins = (await api("/plugins")).plugins || [];
      if (currentRoute === "providers") {
        const usage = await api("/observability/usage/api-keys");
        // Composite response keys contain API keys. Retain counts only.
        providerUsage = Object.fromEntries(
          Object.entries<Data>(usage).map(([name, entries]) => [
            name,
            Object.values<Data>(entries).reduce<{
              success: number;
              failed: number;
            }>(
              (total, entry) => ({
                success: total.success + Number(entry.success || 0),
                failed: total.failed + Number(entry.failed || 0),
              }),
              { success: 0, failed: 0 },
            ),
          ]),
        );
      }
      if (currentRoute === "logs") {
        cursor = "";
        await fetchLogs(false);
        logFiles = (await api("/observability/logs/errors")).files || [];
      }
      if (currentRoute === "models") await loadDefinitions();
    } catch (e) {
      error = message(e);
      online = false;
    } finally {
      loading = false;
    }
  }
  async function edit(
    path: string,
    title: string,
    fallback: any = {},
    yaml = false,
  ) {
    await action(async () => {
      const url = yaml ? "/config.yaml" : path ? fieldPath(path) : "/config";
      const value = yaml
        ? await api(url, "GET", undefined, "text")
        : await configValue(url, fallback);
      editor = { path: url, value, title, yaml };
    });
  }
  async function replace(path: string, before: any, next: any) {
    const url = fieldPath(path),
      latestValue = await configValue(url, Array.isArray(before) ? [] : {});
    if (!equal(latestValue, before))
      throw new Error(
        "This list changed on the server. Refresh and try again. Nothing was written.",
      );
    await api(url, "PUT", next);
    await load();
  }
  async function upload(input: HTMLInputElement, vertex = false) {
    const files = [...(input.files || [])];
    if (!files.length) return;
    await action(async () => {
      const form = new FormData();
      files.forEach((f) => form.append("file", f));
      const result = await api(
        vertex ? "/oauth/import?provider=vertex" : "/credentials",
        "POST",
        form,
      );
      await load();
      if (result.failed?.length)
        throw new Error(
          `${result.uploaded || 0} uploaded; ${result.failed.map((f: Data) => `${f.name}: ${f.error}`).join("; ")}`,
        );
    }, "Credential upload complete.");
    input.value = "";
  }
  async function credentialAction(a: Data, kind: string) {
    if (a.fixture) {
      error =
        "Sample account is read-only. Add a real credential to perform this action.";
      return;
    }
    await action(async () => {
      const lookup = {
        name: a.name,
        ...(a.auth_index ? { auth_index: a.auth_index } : {}),
      };
      if (kind === "delete") {
        if (!confirm(`Permanently delete ${a.name}?`)) return;
        await api("/credentials", "DELETE", { names: [a.name] });
      }
      if (kind === "status")
        await api("/credentials/status", "PATCH", {
          ...lookup,
          disabled: !a.disabled,
        });
      if (kind === "refresh") await api("/credentials/refresh", "POST", lookup);
      if (kind === "reset") {
        if (
          !confirm(
            `Clear cooldown for ${a.name}? This does not restore provider quota.`,
          )
        )
          return;
        await api("/routing/cooldown/reset", "POST", {
          auth_index: a.auth_index,
        });
      }
      if (kind === "download")
        download(
          await api(
            `/credentials/download?name=${encodeURIComponent(a.name)}`,
            "GET",
            undefined,
            "blob",
          ),
          a.name,
        );
      if (kind === "details") {
        details = a;
        credentialNote = a.note || "";
        credentialPriority = a.priority || 0;
        credentialProxy = a.proxy_url || "";
        credModels =
          (await api(`/credentials/models?name=${encodeURIComponent(a.name)}`))
            .models || [];
      } else await load();
    });
  }
  async function addProvider() {
    await action(async () => {
      if (!upstreamKey.trim() || !groupName.trim())
        throw new Error("Enter a group name and an API key.");
      const group: Data = {
        name: groupName.trim(),
        keys: [{ "api-key": upstreamKey.trim() }],
      };
      if (upstreamUrl.trim())
        group["base-url"] = new URL(upstreamUrl.trim()).href;
      if (family === "openai-compatibility" && !upstreamUrl.trim())
        throw new Error("OpenAI-compatible providers require a base URL.");
      await replace(`api-keys/${family}`, groups, [...groups, group]);
      groupName = "";
      upstreamKey = "";
      upstreamUrl = "";
    }, "Provider group added.");
  }
  async function loadDefinitions() {
    definitions =
      (await api(`/routing/model-definitions/${encodeURIComponent(channel)}`))
        .models || [];
  }
  async function addAlias() {
    await action(async () => {
      if (!aliasTarget.trim() || !aliasName.trim())
        throw new Error("Enter an upstream model and client alias.");
      if (aliases.some((a) => a.alias === aliasName.trim()))
        throw new Error("That alias already exists.");
      await replace(`oauth/model-alias/${channel}`, aliases, [
        ...aliases,
        { name: aliasTarget.trim(), alias: aliasName.trim(), fork },
      ]);
      aliasTarget = "";
      aliasName = "";
    }, "Alias added.");
  }
  async function addRule() {
    await action(async () => {
      const params = JSON.parse(ruleParams);
      if (
        ruleKind === "filter"
          ? !Array.isArray(params)
          : !params || Array.isArray(params) || typeof params !== "object"
      )
        throw new Error(
          ruleKind === "filter"
            ? "Filter params must be a JSON array of paths."
            : "Params must be a JSON object.",
        );
      const rules = payload[ruleKind] || [];
      await replace(`requests/payload/${ruleKind}`, rules, [
        ...rules,
        { models: [{ name: ruleModel, protocol: ruleProtocol }], params },
      ]);
    }, "Payload rule added.");
  }
  async function startOAuth() {
    await action(async () => {
      if (oauth?.state && oauthState === "wait")
        await api(
          `/oauth/session?state=${encodeURIComponent(oauth.state)}`,
          "DELETE",
        );
      const result = await api(
        `/oauth/auth-url?provider=${encodeURIComponent(oauthProvider)}${["claude", "codex", "antigravity", "xai", "devin"].includes(oauthProvider) ? "&is_webui=true" : ""}`,
      );
      const url = new URL(result.url);
      if (!["https:", "http:"].includes(url.protocol))
        throw new Error("Server returned an unsafe login URL.");
      oauth = { ...result, provider: oauthProvider };
      oauthState = "wait";
      callback = "";
    });
  }
  async function fetchLogs(append = true) {
    const follow =
      !logPane ||
      logPane.scrollHeight - logPane.scrollTop - logPane.clientHeight < 40;
    const result = await api(
      `/observability/logs?limit=250${append && cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`,
    );
    lines = (
      append && !result["cursor-reset"]
        ? [...lines, ...(result.lines || [])]
        : result.lines || []
    ).slice(-1500);
    cursor = result["next-cursor"] || "";
    await tick();
    if (logPane && follow) logPane.scrollTop = logPane.scrollHeight;
  }
  async function fetchQuota(a: Data) {
    if (a.fixture) return;
    const id = a.auth_index || a.name;
    quotaErrors = { ...quotaErrors, [id]: "" };
    const p = provider(a);
    try {
      const urls: Record<string, string> = {
        claude: "https://api.anthropic.com/api/oauth/usage",
        codex: "https://chatgpt.com/backend-api/wham/usage",
        kimi: "https://api.kimi.com/coding/v1/usages",
        "kimi-ai": "https://api.kimi.ai/coding/v1/usages",
      };
      let result: Data;
      const plugin = plugins.find(
        (plugin) =>
          plugin.supports_quota && (plugin.quota_provider || plugin.id) === p,
      );
      if (plugin)
        result = await api(
          `/plugins/${encodeURIComponent(plugin.id)}/quota`,
          "POST",
          { name: a.name, auth_index: a.auth_index },
        );
      else {
        if (!urls[p])
          throw new Error(
            "No built-in quota adapter for this provider. Plugin quota is supported when advertised.",
          );
        const header: Data = {
          Authorization: "Bearer $TOKEN$",
          "Content-Type": "application/json",
        };
        if (p === "claude") {
          header["anthropic-beta"] = "oauth-2025-04-20";
          header["User-Agent"] = "claude-cli/2.1.280 (external, cli)";
        }
        if (p === "codex") {
          header["User-Agent"] = "codex-tui/0.149.1";
          const accountId = a.account_id || a.id_token?.chatgpt_account_id;
          if (accountId) header["Chatgpt-Account-Id"] = accountId;
        }
        const response = await api("/requests/api-call", "POST", {
          authIndex: a.auth_index,
          method: "GET",
          url: urls[p],
          header,
        });
        if (response.status_code < 200 || response.status_code >= 300)
          throw new Error(
            `Provider returned HTTP ${response.status_code}. Try refreshing the credential.`,
          );
        result =
          typeof response.body === "string"
            ? JSON.parse(response.body)
            : response.body;
      }
      quotas = {
        ...quotas,
        [id]: { ...result, fetched: new Date().toISOString() },
      };
    } catch (e) {
      quotaErrors = { ...quotaErrors, [id]: message(e) };
    }
  }
  async function poll() {
    if (!logged || pollBusy || document.hidden) return;
    pollBusy = true;
    clock = Date.now();
    try {
      if (oauth?.state && oauthState === "wait") {
        const result = await api(
          `/oauth/status?state=${encodeURIComponent(oauth.state)}`,
        );
        oauthState = result.status;
        if (result.status === "error")
          error = result.error || "Provider login failed. Start again.";
        if (result.status === "ok") {
          notify("Account connected.");
          await load();
        }
      }
      if (route === "logs" && tail) await fetchLogs();
      if (live) {
        const rows = await api("/observability/usage/queue?count=500");
        if (Array.isArray(rows))
          events = [
            ...events,
            ...rows
              .filter((row) => row && typeof row === "object")
              .map(cleanEvent),
          ]
            .filter((e) => clock - Date.parse(e.timestamp) < 15 * 60_000)
            .slice(-10000);
      }
      if (route === "overview" || route === "credentials") {
        accounts = (await api("/credentials")).files || [];
        online = true;
        checked = new Date().toISOString();
      }
    } catch (e) {
      error = message(e);
      online = false;
      live = false;
      tail = false;
    } finally {
      pollBusy = false;
    }
  }
  onMount(() => {
    if (import.meta.env.DEV && import.meta.env.VITE_DEV_FIXTURES === "true")
      import("./dev").then((data) => {
        fixtures = true;
        fixtureAccounts = data.accounts;
        fixtureEvents = data.events;
        quotas = data.quotas;
      });
    const timer = setInterval(poll, 3000);
    const hash = () => {
      const next = location.hash.slice(1) || "overview";
      if (next !== route)
        navigate(pages.some((p) => p[0] === next) ? next : "overview");
    };
    window.addEventListener("hashchange", hash);
    return () => {
      clearInterval(timer);
      clearTimeout(toastTimer);
      window.removeEventListener("hashchange", hash);
    };
  });
</script>

<svelte:head
  ><title>{logged ? page[1] : "Sign in"} · cliproxy-rs</title></svelte:head
>

{#snippet badge(a: Data)}<span
    class:warn={health(a) === "Needs attention"}
    class:neutral={a.disabled}
    class="status"><i></i>{health(a)}</span
  >{/snippet}
{#snippet empty(title: string, body: string, target = "")}<div class="empty">
    <Icon name={route} size={30} />
    <h3>{title}</h3>
    <p>{body}</p>
    {#if target}<button onclick={() => navigate(target)}
        >Connect an account <Icon name="arrow" size={16} /></button
      >{/if}
  </div>{/snippet}
{#snippet accountTable(compact = false)}
  <div class="table-scroll">
    <table>
      <thead
        ><tr
          ><th>Account</th><th>Provider</th><th>Health</th><th class="numeric"
            >Requests</th
          ><th>{compact ? "Quota used" : "Actions"}</th></tr
        ></thead
      ><tbody>
        {#each compact ? allAccounts : shownAccounts as a (`${a.name}:${a.auth_index || ""}`)}<tr
            ><td
              ><div class="account-cell">
                <span class="provider-symbol"
                  >{provider(a).slice(0, 1).toUpperCase()}</span
                >
                <div>
                  <strong>{a.email || a.label || a.name}</strong><small
                    >{a.note || a.name}{a.fixture ? " · sample" : ""}</small
                  >
                </div>
              </div></td
            ><td><span class="provider-label">{provider(a)}</span></td><td
              >{@render badge(a)}</td
            ><td class="numeric"
              >{Number(a.success || 0).toLocaleString()}<span
                class="mobile-label"
              >
                requests</span
              ><small class="muted">{a.failed || 0} failed</small></td
            ><td>
              {#if compact}{@const windows = quotaWindows(
                  provider(a),
                  quotas[a.auth_index || a.name] || {},
                )}{#if windows.length}<div class="mini-quota">
                    <meter
                      min="0"
                      max="100"
                      value={windows[0].used}
                      aria-label={`${a.name} quota used`}
                    ></meter><span>{windows[0].used}%</span>
                  </div>{:else}<span class="muted">Not fetched</span>{/if}
              {:else}<div class="row-actions">
                  <button
                    disabled={busy || a.fixture}
                    onclick={() => credentialAction(a, "details")}
                    >Details</button
                  >
                  <details class="action-menu">
                    <summary aria-label={`Actions for ${a.name}`}>More</summary>
                    <div>
                      <button
                        disabled={busy || a.fixture}
                        onclick={() => credentialAction(a, "status")}
                        >{a.disabled ? "Enable" : "Disable"}</button
                      ><button
                        disabled={busy || a.fixture}
                        onclick={() => credentialAction(a, "refresh")}
                        >Refresh token</button
                      ><button
                        disabled={busy || a.fixture || !a.auth_index}
                        onclick={() => credentialAction(a, "reset")}
                        >Reset cooldown</button
                      ><button
                        disabled={busy || a.fixture || a.runtime_only}
                        onclick={() => credentialAction(a, "download")}
                        >Download JSON</button
                      ><button
                        class="danger-text"
                        disabled={busy || a.fixture || a.runtime_only}
                        onclick={() => credentialAction(a, "delete")}
                        >Delete credential</button
                      >
                    </div>
                  </details>
                </div>{/if}
            </td></tr
          >{/each}
      </tbody>
    </table>
  </div>
{/snippet}

{#if !logged}
  <div class="login-shell">
    <div class="login-story">
      <a class="brand" href="#overview"
        ><span class="brand-mark"><Icon name="terminal" size={25} /></span><span
          >cliproxy<span class="brand-rs">-rs</span></span
        ></a
      >
      <div>
        <h1>Your models.<br />One clear view.</h1>
        <p>
          A faster proxy deserves a better control room.<br />Manage your
          accounts, routing, and requests.
        </p>
        <div class="login-diagram">
          <span>Claude</span><span>Codex</span><span>Gemini</span>
          <div class="diagram-line"></div>
          <strong><Icon name="terminal" /> cliproxy-rs</strong>
          <div class="diagram-line"></div>
          <span class="diagram-client">Your tools, connected.</span>
        </div>
      </div>
      <span class="muted">Open source. Built for your workflow.</span>
    </div>
    <div class="login-form-wrap">
      <button
        class="theme-toggle"
        onclick={toggleTheme}
        aria-label="Toggle theme"
        ><Icon name={theme === "dark" ? "sun" : "moon"} /></button
      >
      <form
        class="login-form"
        onsubmit={(e) => {
          e.preventDefault();
          login();
        }}
      >
        <h2>Welcome to your proxy.</h2>
        <p class="muted">Connect with your server’s management key.</p>
        <label
          >Server URL<input
            type="url"
            required
            bind:value={server}
            autocomplete="url"
            placeholder="https://proxy.example.com"
          /></label
        ><label
          >Management key<input
            type="password"
            required
            bind:value={secret}
            autocomplete="off"
            placeholder="Enter your management key"
          /></label
        >{#if loginError}<div class="notice danger" role="alert">
            {loginError}
          </div>{/if}<button class="primary" disabled={busy}
          >{busy ? "Connecting…" : "Connect to server"}<Icon
            name="arrow"
            size={18}
          /></button
        ><small
          >Key stays in memory, never in browser storage. Use HTTPS when
          connecting remotely.</small
        >{#if fixtures}<div class="fixture-notice">
            Development fixtures enabled. Real management key still required.
          </div>{/if}
      </form>
    </div>
  </div>
{:else}
  <div class="app-shell">
    <aside>
      <a
        class="brand"
        href="#overview"
        onclick={(e) => {
          e.preventDefault();
          navigate("overview");
        }}
        ><span class="brand-mark"><Icon name="terminal" size={22} /></span><span
          >cliproxy<span class="brand-rs">-rs</span></span
        ></a
      >
      <div class="workspace-label">
        <span class="connection-dot" class:offline={!online}></span>Management
        console
      </div>
      <nav aria-label="Management pages">
        {#each pages as p, i}{#if i === 1 || i === 5 || i === 8}<div
              class="nav-divider"
            ></div>{/if}<a
            href={`#${p[0]}`}
            class:active={route === p[0]}
            aria-current={route === p[0] ? "page" : undefined}
            onclick={(e) => {
              e.preventDefault();
              navigate(p[0]);
            }}
            ><Icon
              name={p[0]}
              size={18}
            />{p[1]}{#if p[0] === "credentials" && allAccounts.length}<span
                class="nav-count">{allAccounts.length}</span
              >{/if}</a
          >{/each}
      </nav>
      <div class="sidebar-bottom">
        <div>
          <span class="connection-dot" class:offline={!online}></span>{online
            ? "Server connected"
            : "Connection interrupted"}<small>Management API v8</small>
        </div>
        <button class="icon-button" onclick={logout} aria-label="Sign out"
          ><Icon name="logout" size={18} /></button
        >
      </div>
    </aside>
    <div class="main-shell">
      <header class="topbar">
        <div>
          <span class="server-label">{server.replace(/^https?:\/\//, "")}</span
          ><span class="topbar-separator"></span><span
            class="status"
            class:warn={!online}><i></i>{online ? "Connected" : "Offline"}</span
          >
        </div>
        <div>
          <span class="topbar-hint"
            >{fixtures
              ? "DEVELOPMENT · SAMPLE DATA"
              : "Management API v8"}</span
          ><button
            class="icon-button"
            onclick={toggleTheme}
            aria-label="Toggle theme"
            ><Icon name={theme === "dark" ? "sun" : "moon"} size={18} /></button
          >
        </div>
      </header>
      <main id="main" aria-busy={loading}>
        <div class="page-head">
          <div>
            <h1>{page[1]}</h1>
            <p>{page[2]}</p>
          </div>
          <div class="head-actions">
            <button
              onclick={() => action(load)}
              disabled={busy || loading || !!editor}
              ><Icon name="refresh" size={16} />{loading
                ? "Refreshing…"
                : "Refresh"}</button
            >{#if route === "credentials"}<button
                class="primary"
                onclick={() => navigate("oauth")}
                ><Icon name="plus" size={16} />Connect account</button
              >{/if}
          </div>
        </div>
        {#if error}<div class="notice danger" role="alert">
            <span>{error}</span><button
              class="icon-button"
              aria-label="Dismiss error"
              onclick={() => (error = "")}
              ><Icon name="close" size={16} /></button
            >
          </div>{/if}
        {#if fixtures && ["overview", "credentials", "quotas"].includes(route)}<div
            class="fixture-notice"
          >
            Sample accounts and telemetry supplement a real server connection.
            Sample actions are disabled.
          </div>{/if}
        {#if editor}
          {#key editor.path}<Editor
              {...editor}
              onsaved={() => {
                editor = null;
                notify("Configuration saved.");
                load();
              }}
              oncancel={() => (editor = null)}
            />{/key}
        {:else if route === "overview"}
          <div class="overview-toolbar">
            <span class="muted">Live session · Last 15 minutes</span><button
              class:recording={live}
              onclick={() => {
                if (
                  live ||
                  confirm(
                    "Live telemetry consumes the server usage queue. Enable only if no other collector needs these events. Events stay in this tab and disappear on sign-out.",
                  )
                ) {
                  live = !live;
                  if (live) poll();
                }
              }}
              ><span class="connection-dot" class:offline={!live}></span>{live
                ? "Stop live telemetry"
                : "Enable live telemetry"}</button
            >
          </div>
          <div class="measurements">
            <div>
              <span>Requests / minute</span><strong
                >{live || fixtures ? stats.rpm : "—"}<small>rpm</small></strong
              >
              <p>
                {live || fixtures
                  ? "Observed in the last 60 seconds"
                  : "Enable telemetry to measure"}
              </p>
            </div>
            <div>
              <span>Median latency</span><strong
                >{stats.p50 ?? "—"}<small>ms</small></strong
              >
              <p>End-to-end · p50</p>
            </div>
            <div>
              <span>Error rate</span><strong
                >{stats.error === null ? "—" : stats.error.toFixed(1)}<small
                  >%</small
                ></strong
              >
              <p>Failed / observed requests</p>
            </div>
            <div>
              <span>Healthy accounts</span><strong
                >{allAccounts.filter((a) => health(a) === "Healthy")
                  .length}<small>/ {allAccounts.length}</small></strong
              >
              <p>
                {allAccounts.filter((a) => health(a) === "Needs attention")
                  .length} need attention
              </p>
            </div>
          </div>
          <div class="telemetry-grid">
            <section class="traffic panel">
              <div class="section-head">
                <h2>Request traffic</h2>
                <span class="legend"><i></i>Requests / min</span>
              </div>
              <div class="chart-wrap">
                <svg
                  class="traffic-chart"
                  viewBox="0 0 800 250"
                  role="img"
                  aria-label="Requests per minute over the last 15 minutes"
                  ><title>Requests per minute, 30-second buckets</title
                  >{#each [0, 1, 2, 3] as n}<line
                      x1="48"
                      y1={50 + n * 53.33}
                      x2="778"
                      y2={50 + n * 53.33}
                      class="chart-grid"
                    /><text x="34" y={55 + n * 53.33} text-anchor="end"
                      >{Math.round(chartMax * (1 - n / 3))}</text
                    >{/each}<polygon
                    points={`48,210 ${chartPoints} 778,210`}
                    class="chart-area"
                  /><polyline
                    points={chartPoints}
                    class="chart-line"
                  />{#if live || fixtures}<circle
                      cx="778"
                      cy={210 - (stats.bins[29] / chartMax) * 160}
                      r="4"
                      class="chart-dot"
                    />{/if}<text x="48" y="240">15 min ago</text><text
                    x="400"
                    y="240"
                    text-anchor="middle">7.5 min ago</text
                  ><text x="778" y="240" text-anchor="end">Now</text></svg
                >{#if !events.length && !fixtures}<div class="chart-empty">
                    {live
                      ? "Listening for requests…"
                      : "Your traffic will appear here."}
                  </div>{/if}
              </div>
              <div class="chart-footer">
                <span>{stats.tokens.toLocaleString()} tokens observed</span
                ><span>Refreshes every 3 seconds</span>
              </div>
            </section>
            <section class="mix panel">
              <div class="section-head">
                <h2>Provider mix</h2>
                <span class="muted">This session</span>
              </div>
              {#each providerMix as p}<div class="mix-row">
                  <div><span>{p.name}</span><strong>{p.count}</strong></div>
                  <progress
                    max={Math.max(1, ...providerMix.map((p) => p.count))}
                    value={p.count}
                    aria-label={`${p.name} requests`}
                  ></progress><small
                    >{p.accounts}
                    {p.accounts === 1 ? "account" : "accounts"}</small
                  >
                </div>{/each}{#if !providerMix.length}<p
                  class="muted mix-empty"
                >
                  No accounts connected yet.
                </p>{/if}<button
                class="text-button"
                onclick={() => navigate("providers")}
                >Manage providers <Icon name="arrow" size={16} /></button
              >
            </section>
          </div>
          <section class="panel account-panel">
            <div class="section-head">
              <div>
                <h2>Account health</h2>
                <p class="muted">
                  A clear path from every account to every request.
                </p>
              </div>
              <button
                class="text-button"
                onclick={() => navigate("credentials")}
                >View credentials <Icon name="arrow" size={16} /></button
              >
            </div>
            {#if allAccounts.length}{@render accountTable(
                true,
              )}{:else}{@render empty(
                "Ready for your first account",
                "Connect a provider or upload credentials to start routing requests.",
                "oauth",
              )}{/if}
          </section>
          <div class="bottom-note">
            <span class="connection-dot" class:offline={!online}></span>{checked
              ? `Last checked ${stamp(checked)}`
              : "Checking server…"}<span
              >Telemetry is session-only. No synthetic production metrics.</span
            >
          </div>
        {:else if route === "credentials"}
          <div class="section-toolbar">
            <label class="search"
              ><Icon name="search" size={18} /><input
                bind:value={search}
                placeholder="Search accounts or providers"
                aria-label="Search credentials"
              /></label
            >
            <div class="head-actions">
              <label class="upload-button"
                >Upload JSON<input
                  type="file"
                  accept=".json,application/json"
                  multiple
                  onchange={(e) => upload(e.currentTarget)}
                  disabled={busy}
                /></label
              ><button
                disabled={busy}
                onclick={() =>
                  action(async () => {
                    const result = await api("/credentials/refresh", "POST", {
                      all: true,
                    });
                    const failed = (result.results || []).filter(
                      (r: Data) => !r.success,
                    );
                    if (failed.length)
                      throw new Error(
                        `${failed.length} credentials could not refresh: ${failed.map((r: Data) => `${r.id}: ${r.error}`).join("; ")}`,
                      );
                    await load();
                    notify(
                      result.results?.length
                        ? `${result.results.length} credentials refreshed.`
                        : "No eligible refreshable credentials.",
                    );
                  })}>Refresh all tokens</button
              >
            </div>
          </div>
          <section class="panel account-panel">
            <div class="section-head">
              <h2>{shownAccounts.length} credentials</h2>
              <span class="muted">Token refresh and status sync</span>
            </div>
            {#if shownAccounts.length}{@render accountTable()}{:else}{@render empty(
                search ? "No matching credentials" : "No credentials yet",
                search
                  ? "Try another account name or provider."
                  : "Upload a JSON auth file or use OAuth to connect an account.",
                "oauth",
              )}{/if}
          </section>
          {#if details}<section class="panel">
              <div class="section-head">
                <h2>{details.name}</h2>
                <button onclick={() => (details = null)}>Close details</button>
              </div>
              {#if details.status_message}<div class="notice">
                  {details.status_message}
                </div>{/if}
              <div class="info-grid">
                <div>
                  <span>Last refresh</span><strong
                    >{stamp(details.last_refresh)}</strong
                  >
                </div>
                <div>
                  <span>Auth index</span><code
                    >{details.auth_index || "Not reported"}</code
                  >
                </div>
              </div>
              <form
                class="inline-form"
                onsubmit={(e) => {
                  e.preventDefault();
                  action(async () => {
                    await api("/credentials/fields", "PATCH", {
                      name: details!.name,
                      note: credentialNote,
                      priority: credentialPriority,
                      proxy_url: credentialProxy,
                    });
                    details = null;
                    await load();
                  }, "Credential fields saved.");
                }}
              >
                <label>Note<input bind:value={credentialNote} /></label><label
                  >Priority<input
                    type="number"
                    bind:value={credentialPriority}
                  /></label
                ><label
                  >Proxy URL<input
                    bind:value={credentialProxy}
                    placeholder="direct or socks5://…"
                  /></label
                ><button class="primary" disabled={busy}>Save fields</button>
              </form>
              <h3>Available models</h3>
              <div class="model-tags">
                {#each credModels as model}<code>{model.id}</code
                  >{/each}{#if !credModels.length}<span class="muted"
                    >No models reported.</span
                  >{/if}
              </div>
            </section>{/if}
        {:else if route === "oauth"}
          <div class="split-page">
            <section class="panel oauth-panel">
              <h2>A secure handshake.</h2>
              <p class="muted">
                Your server exchanges the authorization code and stores the
                credential. Tokens never pass through this dashboard.
              </p>
              <form
                onsubmit={(e) => {
                  e.preventDefault();
                  startOAuth();
                }}
              >
                <label
                  >Provider<select bind:value={oauthProvider}
                    >{#each [...oauthProviders, ...plugins
                        .filter((p) => p.supports_oauth)
                        .map((p) => p.oauth_provider || p.id)] as p}<option
                        value={p}>{p}</option
                      >{/each}</select
                  ></label
                ><button class="primary" disabled={busy}
                  >{busy ? "Starting…" : "Start provider login"}<Icon
                    name="oauth"
                    size={17}
                  /></button
                >
              </form>
              {#if oauth}<div class="oauth-session">
                  <div class="section-head">
                    <h3>{oauth.provider} login</h3>
                    <span class="status" class:warn={oauthState === "error"}
                      ><i></i>{oauthState === "wait"
                        ? "Waiting for authorization"
                        : oauthState}</span
                    >
                  </div>
                  {#if oauthState === "wait"}{#if oauth.user_code}<div>
                        Device code<code class="device-code"
                          >{oauth.user_code}</code
                        >
                      </div>{/if}<a
                      class="button primary"
                      href={oauth.url}
                      target="_blank"
                      rel="noopener noreferrer"
                      >Open authorization page <Icon
                        name="oauth"
                        size={16}
                      /></a
                    ><label
                      >Authorization URL<input
                        readonly
                        value={oauth.url}
                      /></label
                    ><label
                      >Remote server? Paste the full callback URL<textarea
                        rows="3"
                        bind:value={callback}
                        placeholder="http://localhost:…/callback?code=…&state=…"
                      ></textarea></label
                    >
                    <div class="head-actions">
                      <button
                        disabled={busy || !callback.trim()}
                        onclick={() =>
                          action(async () => {
                            await api("/oauth/callback", "POST", {
                              provider: oauth!.provider,
                              redirect_url: callback.trim(),
                            });
                            await poll();
                          }, "Callback accepted. Waiting for credential exchange.")}
                        >Submit callback</button
                      ><button
                        onclick={() =>
                          action(async () => {
                            await api(
                              `/oauth/session?state=${encodeURIComponent(oauth!.state)}`,
                              "DELETE",
                            );
                            oauthState = "cancelled";
                          }, "Login cancelled.")}
                        disabled={busy}>Cancel login</button
                      >
                    </div>{:else if oauthState === "ok"}<p class="success-text">
                      Account connected. Find it in Credentials.
                    </p>{/if}
                </div>{/if}
            </section>
            <section class="oauth-guide">
              <div class="guide-line">
                <span class="guide-step">1</span>
                <div>
                  <h3>Choose your provider</h3>
                  <p>
                    Built-in OAuth plus providers advertised by installed
                    plugins.
                  </p>
                </div>
              </div>
              <div class="guide-line">
                <span class="guide-step">2</span>
                <div>
                  <h3>Authorize in your browser</h3>
                  <p>
                    Use the provider’s sign-in page. This console polls the
                    session every 3 seconds.
                  </p>
                </div>
              </div>
              <div class="guide-line">
                <span class="guide-step">3</span>
                <div>
                  <h3>Bring the callback home</h3>
                  <p>
                    On a remote server, paste the entire callback URL.
                    Acceptance is not completion; wait for “ok”.
                  </p>
                </div>
              </div>
              <div class="vertex-import">
                <h3>Using Vertex AI?</h3>
                <p>Import a Google service-account JSON file instead.</p>
                <label class="upload-button"
                  >Import service account<input
                    type="file"
                    accept=".json"
                    onchange={(e) => upload(e.currentTarget, true)}
                    disabled={busy}
                  /></label
                >
              </div>
            </section>
          </div>
        {:else if route === "providers"}
          <div class="section-toolbar">
            <span class="muted"
              >{(providerUsage[family]?.success || 0).toLocaleString()} successful
              requests · {(providerUsage[family]?.failed || 0).toLocaleString()} failed</span
            ><button class="text-button" onclick={() => navigate("credentials")}
              >Manage individual key health <Icon
                name="arrow"
                size={16}
              /></button
            >
          </div>
          <div class="tabs provider-tabs">
            {#each families as f}<button
                class:active={family === f}
                onclick={() => (family = f)}
                >{f}<span>{readPath(config, `api-keys/${f}`, []).length}</span
                ></button
              >{/each}
          </div>
          <section class="panel">
            <div class="section-head">
              <div>
                <h2>{family} groups</h2>
                <p class="muted">
                  Group policy is inherited by its keys. Explicit overrides stay
                  intact.
                </p>
              </div>
              <button
                onclick={() =>
                  edit(`api-keys/${family}`, `${family} groups`, [])}
                >Edit all fields</button
              >
            </div>
            {#each groups as group, i}<div class="provider-group">
                <div class="section-head">
                  <h3>{group.name || `Group ${i + 1}`}</h3>
                  <div class="head-actions">
                    <span class="status" class:neutral={group.disabled}
                      ><i></i>{family === "openai-compatibility"
                        ? group.disabled
                          ? "Disabled"
                          : "Enabled"
                        : "Configured"}</span
                    >{#if family === "openai-compatibility"}<button
                        disabled={busy}
                        onclick={() =>
                          action(async () => {
                            await replace(
                              `api-keys/${family}`,
                              groups,
                              groups.map((g, index) =>
                                index === i
                                  ? { ...g, disabled: !g.disabled }
                                  : g,
                              ),
                            );
                          })}>{group.disabled ? "Enable" : "Disable"}</button
                      >{/if}<button
                      class="danger-text"
                      disabled={busy}
                      onclick={() => {
                        if (
                          confirm(
                            `Delete provider group ${group.name || i + 1} and its keys?`,
                          )
                        )
                          action(() =>
                            replace(
                              `api-keys/${family}`,
                              groups,
                              groups.filter((_, n) => n !== i),
                            ),
                          );
                      }}>Delete</button
                    >
                  </div>
                </div>
                <p class="code muted">
                  {group["base-url"] || "Default provider endpoint"}
                </p>
                <div class="group-keys">
                  {#each group.keys || [] as key, k}<div>
                      <span>Key {k + 1}</span><code
                        >{revealKeys ? key["api-key"] : "••••••••••••"}</code
                      ><span class="muted"
                        >Weight {key.weight ?? group.weight ?? 1}</span
                      >
                    </div>{/each}
                </div>
                <div class="model-tags">
                  {#each group.models || [] as model}<code
                      >{model.name}{model.alias
                        ? ` → ${model.alias}`
                        : ""}</code
                    >{/each}
                </div>
              </div>{/each}
            {#if !groups.length}{@render empty(
                `No ${family} groups`,
                "Add an API key below. Use Edit all fields for headers, model mappings, exclusions, proxy, priority, and per-key overrides.",
              )}{/if}
            <form
              class="inline-form"
              onsubmit={(e) => {
                e.preventDefault();
                addProvider();
              }}
            >
              <label
                >Group name<input
                  bind:value={groupName}
                  placeholder="production"
                  required
                /></label
              ><label
                >API key<input
                  type="password"
                  bind:value={upstreamKey}
                  required
                  autocomplete="off"
                  placeholder="Provider API key"
                /></label
              ><label
                >Base URL<input
                  type="url"
                  bind:value={upstreamUrl}
                  placeholder="Default endpoint"
                  required={family === "openai-compatibility"}
                /></label
              ><button class="primary" disabled={busy}
                ><Icon name="plus" size={16} />Add group</button
              >
            </form>
          </section>
          <div class="bottom-note">
            <button
              class="text-button"
              onclick={() => (revealKeys = !revealKeys)}
              >{revealKeys ? "Hide" : "Reveal"} API keys</button
            ><span>Provider keys are not client access keys.</span>
          </div>
        {:else if route === "keys"}
          <section class="panel">
            <div class="section-head">
              <div>
                <h2>Client access</h2>
                <p class="muted">
                  Applications send these keys as Bearer tokens to your proxy.
                </p>
              </div>
              <button
                onclick={() => edit("access/api-keys", "Client API keys", [])}
                >Edit list</button
              >
            </div>
            {#if !clientKeys.length}<div class="notice warn">
                No client keys configured. The proxy may accept unauthenticated
                client traffic.
              </div>{/if}
            <div class="key-list">
              {#each clientKeys as key, i}<div>
                  <span class="key-index">{i + 1}</span><code
                    >{revealKeys ? key : "••••••••••••••••••••"}</code
                  ><button
                    disabled={busy}
                    onclick={() =>
                      action(async () => {
                        await navigator.clipboard.writeText(key);
                      }, "Key copied.")}>Copy</button
                  ><button
                    class="danger-text"
                    disabled={busy}
                    onclick={() => {
                      if (
                        confirm(
                          "Remove this client key? Applications using it will lose access.",
                        )
                      )
                        action(() =>
                          replace(
                            "access/api-keys",
                            clientKeys,
                            clientKeys.filter((_, n) => n !== i),
                          ),
                        );
                    }}>Remove</button
                  >
                </div>{/each}
            </div>
            <form
              class="inline-form key-form"
              onsubmit={(e) => {
                e.preventDefault();
                action(async () => {
                  if (
                    !clientKey.trim() ||
                    clientKeys.includes(clientKey.trim())
                  )
                    throw new Error("Enter a unique, non-empty key.");
                  await replace("access/api-keys", clientKeys, [
                    ...clientKeys,
                    clientKey.trim(),
                  ]);
                  clientKey = "";
                }, "Client key added.");
              }}
            >
              <label
                >New client key<input
                  type="password"
                  bind:value={clientKey}
                  required
                  autocomplete="off"
                  placeholder="Paste or generate a strong key"
                /></label
              ><button
                type="button"
                onclick={() =>
                  (clientKey = `sk-${Array.from(crypto.getRandomValues(new Uint8Array(24)), (n) => n.toString(16).padStart(2, "0")).join("")}`)}
                >Generate key</button
              ><button class="primary" disabled={busy}>Add key</button>
            </form>
            <button
              class="text-button"
              onclick={() => (revealKeys = !revealKeys)}
              >{revealKeys ? "Hide" : "Reveal"} keys</button
            >
          </section>
          <div class="notice">
            Client access keys and the management key are separate. Never share
            your management key with client applications.
          </div>
        {:else if route === "models"}
          <div class="section-toolbar">
            <label class="select-inline"
              >OAuth provider<select
                bind:value={channel}
                onchange={() => action(loadDefinitions)}
                >{#each [...new Set( [...oauthProviders, "gemini", ...plugins.map((p) => p.id)] )] as p}<option
                    value={p}>{p}</option
                  >{/each}</select
              ></label
            ><button
              onclick={() => edit("oauth", "OAuth aliases and exclusions")}
              >Edit all mappings</button
            >
          </div>
          <section class="panel">
            <div class="section-head">
              <h2>Model aliases</h2>
              <span class="muted">{aliases.length} mappings</span>
            </div>
            {#if aliases.length}<div class="alias-list">
                {#each aliases as alias, i}<div>
                    <code>{alias.name}</code><Icon
                      name="arrow"
                      size={17}
                    /><code class="accent">{alias.alias}</code><span
                      class="muted"
                      >{alias.fork ? "Keep original" : "Rename"}</span
                    ><button
                      class="danger-text"
                      disabled={busy}
                      onclick={() =>
                        action(() =>
                          replace(
                            `oauth/model-alias/${channel}`,
                            aliases,
                            aliases.filter((_, n) => n !== i),
                          ),
                        )}>Remove</button
                    >
                  </div>{/each}
              </div>{:else}<p class="muted spacious">
                No aliases. Clients use the provider’s original model names.
              </p>{/if}
            <form
              class="inline-form"
              onsubmit={(e) => {
                e.preventDefault();
                addAlias();
              }}
            >
              <label
                >Upstream model<input
                  list="model-definitions"
                  bind:value={aliasTarget}
                  required
                  placeholder="claude-sonnet-4-5"
                /></label
              ><label
                >Client alias<input
                  bind:value={aliasName}
                  required
                  placeholder="daily-driver"
                /></label
              ><label class="checkbox-label"
                ><input type="checkbox" bind:checked={fork} />Keep original</label
              ><button class="primary" disabled={busy}>Add alias</button>
            </form>
          </section>
          <section class="panel">
            <div class="section-head">
              <h2>Excluded models</h2>
              <span class="muted">Wildcards supported</span>
            </div>
            <div class="model-tags">
              {#each excluded as model, i}<span class="tag"
                  ><code>{model}</code><button
                    aria-label={`Remove exclusion ${model}`}
                    disabled={busy}
                    onclick={() =>
                      action(() =>
                        replace(
                          `oauth/excluded-models/${channel}`,
                          excluded,
                          excluded.filter((_, n) => n !== i),
                        ),
                      )}><Icon name="close" size={14} /></button
                  ></span
                >{/each}{#if !excluded.length}<span class="muted"
                  >All provider models are eligible.</span
                >{/if}
            </div>
            <form
              class="inline-form key-form"
              onsubmit={(e) => {
                e.preventDefault();
                action(async () => {
                  if (!exclusion.trim()) return;
                  await replace(`oauth/excluded-models/${channel}`, excluded, [
                    ...new Set([...excluded, exclusion.trim()]),
                  ]);
                  exclusion = "";
                });
              }}
            >
              <label
                >Model pattern<input
                  bind:value={exclusion}
                  placeholder="*-preview"
                  required
                /></label
              ><button disabled={busy}>Exclude model</button>
            </form>
          </section>
          <section class="panel">
            <div class="section-head">
              <h2>Provider model catalog</h2>
              <span class="muted">{definitions.length} definitions</span>
            </div>
            <div class="model-tags">
              {#each definitions as model}<code>{model.id}</code
                >{/each}{#if !definitions.length}<span class="muted"
                  >No definitions available for this provider.</span
                >{/if}
            </div>
          </section>
          <datalist id="model-definitions"
            >{#each definitions as model}<option value={model.id}
              ></option>{/each}</datalist
          >
        {:else if route === "payload"}
          <section class="panel">
            <div class="section-head">
              <div>
                <h2>Request transformations</h2>
                <p class="muted">
                  Defaults fill gaps. Overrides replace values. Filters remove
                  paths.
                </p>
              </div>
              <button onclick={() => edit("requests/payload", "Payload rules")}
                >Edit all rules</button
              >
            </div>
            {#each ["default", "default-raw", "override", "override-raw", "filter"] as kind}<div
                class="rule-section"
              >
                <div class="section-head">
                  <h3>{kind}</h3>
                  <span class="muted">{(payload[kind] || []).length} rules</span
                  >
                </div>
                {#each payload[kind] || [] as rule, i}<div class="rule-row">
                    <div>
                      <div class="model-tags">
                        {#each rule.models || [] as model}<code
                            >{model.name} · {model.protocol || "all"}</code
                          >{/each}
                      </div>
                      <pre class="code">{JSON.stringify(
                          rule.params,
                          null,
                          2,
                        )}</pre>
                    </div>
                    <button
                      class="danger-text"
                      disabled={busy}
                      onclick={() =>
                        action(() =>
                          replace(
                            `requests/payload/${kind}`,
                            payload[kind],
                            payload[kind].filter(
                              (_: Data, n: number) => n !== i,
                            ),
                          ),
                        )}>Remove</button
                    >
                  </div>{/each}{#if !(payload[kind] || []).length}<span
                    class="muted">No {kind} rules.</span
                  >{/if}
              </div>{/each}
          </section>
          <section class="panel">
            <h2>Add a rule</h2>
            <form
              onsubmit={(e) => {
                e.preventDefault();
                addRule();
              }}
            >
              <div class="inline-form">
                <label
                  >Rule type<select bind:value={ruleKind}
                    >{#each ["default", "default-raw", "override", "override-raw", "filter"] as kind}<option
                        value={kind}>{kind}</option
                      >{/each}</select
                  ></label
                ><label
                  >Model pattern<input bind:value={ruleModel} required /></label
                ><label
                  >Protocol<select bind:value={ruleProtocol}
                    ><option>openai</option><option>claude</option><option
                      >gemini</option
                    ><option>codex</option></select
                  ></label
                >
              </div>
              <label
                >Parameters · {ruleKind === "filter"
                  ? "JSON array of paths"
                  : "JSON path-to-value object"}<textarea
                  class="code"
                  rows="5"
                  bind:value={ruleParams}
                  spellcheck="false"></textarea></label
              >
              <div class="editor-actions">
                <span class="muted"
                  >Raw rules expect JSON strings as parameter values.</span
                ><button class="primary" disabled={busy}>Add rule</button>
              </div>
            </form>
          </section>
        {:else if route === "quotas"}
          <div class="section-toolbar">
            <span class="muted"
              >Quota is fetched on demand using the server’s credentials.</span
            ><button
              disabled={busy || !accounts.length}
              onclick={() =>
                action(async () => {
                  for (const a of accounts) await fetchQuota(a);
                })}>Fetch all quotas</button
            >
          </div>
          <div class="quota-grid">
            {#each allAccounts as a}{@const id =
                a.auth_index || a.name}{@const windows = quotaWindows(
                provider(a),
                quotas[id] || {},
              )}{@const quotaPlugin = plugins.find(
                (p) =>
                  p.supports_quota &&
                  (p.quota_provider || p.id) === provider(a),
              )}
              <section class="panel quota-panel">
                <div class="section-head">
                  <div>
                    <span class="provider-label">{provider(a)}</span>
                    <h2>{a.email || a.label || a.name}</h2>
                  </div>
                  {@render badge(a)}
                </div>
                {#if windows.length}{#each windows as window}<div
                      class="quota-window"
                    >
                      <div>
                        <span>{window.label}</span><strong
                          class:danger-text={window.used >= 95}
                          >{window.used}% used</strong
                        >
                      </div>
                      <meter
                        min="0"
                        max="100"
                        value={window.used}
                        class:critical={window.used >= 95}
                        aria-label={`${window.label} quota used`}
                      ></meter><small
                        >Resets {window.reset
                          ? stamp(window.reset)
                          : "not reported"}</small
                      >
                    </div>{/each}{:else}<div class="quota-unfetched">
                    <strong>Not measured yet</strong>
                    <p>
                      {["claude", "codex", "kimi", "kimi-ai"].includes(
                        provider(a),
                      )
                        ? "Fetch the account’s current provider quota."
                        : "Quota needs an adapter advertised by a plugin."}
                    </p>
                  </div>{/if}{#if quotaErrors[id]}<div class="notice danger">
                    {quotaErrors[id]}
                  </div>{/if}{#if quotas[id] && !windows.length}<details>
                    <summary>Provider quota response</summary>
                    <pre class="code">{JSON.stringify(
                        quotas[id],
                        null,
                        2,
                      )}</pre>
                  </details>{/if}
                <div class="quota-footer">
                  <small
                    >{a.fixture
                      ? "Sample quota · development only"
                      : quotas[id]?.fetched
                        ? `Fetched ${stamp(quotas[id].fetched)}`
                        : "Not fetched"}</small
                  >
                  <div>
                    <button
                      disabled={busy || a.fixture}
                      onclick={() => action(() => fetchQuota(a))}
                      >Fetch quota</button
                    ><button
                      disabled={busy || a.fixture || !a.auth_index}
                      onclick={() => credentialAction(a, "reset")}
                      >Reset cooldown</button
                    >{#if quotaPlugin}<button
                        disabled={busy || a.fixture}
                        onclick={() => {
                          if (
                            confirm(
                              `Request a provider quota reset through plugin ${quotaPlugin.id}? This is different from clearing local cooldown.`,
                            )
                          )
                            action(async () => {
                              await api(
                                `/plugins/${encodeURIComponent(quotaPlugin.id)}/quota?auth_index=${encodeURIComponent(a.auth_index)}`,
                                "DELETE",
                              );
                              await fetchQuota(a);
                            });
                        }}>Plugin reset</button
                      >{/if}
                  </div>
                </div>
              </section>{/each}
          </div>
          {#if !allAccounts.length}<section class="panel">
              {@render empty(
                "No account quotas to measure",
                "Connect a Claude or Codex account to see usage windows and reset times.",
                "oauth",
              )}
            </section>{/if}
          <div class="notice">
            Reset cooldown clears local routing state only. It never resets your
            provider’s usage or buys quota.
          </div>
        {:else if route === "configuration"}
          <div class="configuration-intro">
            <div>
              <h2>The source of truth.<br />With room to inspect.</h2>
              <p>
                Visual editing for everyday changes. YAML for the whole picture.
                A diff before anything reaches your server.
              </p>
            </div>
            <div class="head-actions">
              <button onclick={() => edit("", "Visual configuration", config)}
                >Visual editor</button
              ><button
                class="primary"
                onclick={() => edit("", "YAML configuration", "", true)}
                >Open YAML editor <Icon name="payload" size={17} /></button
              >
            </div>
          </div>
          <section class="panel config-list">
            {#each [["routing", "Routing", "Strategy, retries, cooldown, and session affinity"], ["requests", "Requests", "Network proxy, headers, and payload transformations"], ["observability", "Observability", "Logs, request logging, and usage collection"], ["server", "Server", "Listener, TLS, and discovery — may require restart"], ["client", "Client behavior", "Provider-specific CLI optimizations"], ["oauth", "OAuth", "Provider policies, aliases, and model exclusions"], ["plugins", "Plugin configuration", "Trusted extensions and per-instance settings"]] as item}<button
                onclick={() => edit(item[0], item[1])}
                ><Icon
                  name={item[0] === "routing"
                    ? "configuration"
                    : item[0] === "requests"
                      ? "payload"
                      : item[0] === "observability"
                        ? "logs"
                        : item[0] === "server"
                          ? "system"
                          : item[0] === "client"
                            ? "keys"
                            : item[0]}
                /><span><strong>{item[1]}</strong><small>{item[2]}</small></span
                ><Icon name="arrow" size={18} /></button
              >{/each}
          </section>
          <div class="notice">
            v8 reads preserve persisted fields without filling defaults. Visual
            saves target one section. YAML replaces the whole file.
          </div>
        {:else if route === "logs"}
          <div class="section-toolbar">
            <label class="search"
              ><Icon name="search" size={18} /><input
                bind:value={logFilter}
                placeholder="Filter log lines"
                aria-label="Filter logs"
              /></label
            >
            <div class="head-actions">
              <button
                class:recording={tail}
                onclick={() => {
                  tail = !tail;
                  if (tail) action(() => fetchLogs());
                }}
                ><span class="connection-dot" class:offline={!tail}></span>{tail
                  ? "Pause live tail"
                  : "Resume live tail"}</button
              ><button
                class="danger-text"
                disabled={busy}
                onclick={() => {
                  if (confirm("Permanently clear the server application logs?"))
                    action(async () => {
                      await api("/observability/logs", "DELETE");
                      lines = [];
                      cursor = "";
                    }, "Logs cleared.");
                }}>Clear logs</button
              >
            </div>
          </div>
          <section class="log-terminal">
            <div class="terminal-bar">
              <span><Icon name="terminal" size={16} />Application log</span
              ><span
                >{visibleLines.length} lines · {tail ? "live" : "paused"}</span
              >
            </div>
            <div
              class="log-lines code"
              role="log"
              aria-live="off"
              bind:this={logPane}
            >
              {#each visibleLines as line, i}<div
                  class:error-line={/\[error\]|\[fatal\]/i.test(line)}
                >
                  <span>{i + 1}</span><code>{line}</code>
                </div>{/each}{#if !visibleLines.length}<p class="muted">
                  {logFilter
                    ? "No lines match your filter."
                    : "No application logs yet. Enable logging-to-file in Configuration."}
                </p>{/if}
            </div>
            <div class="terminal-footer">
              Cursor-based tail · 250 lines per poll · newest 1,500 retained
            </div>
          </section>
          <div class="logs-bottom">
            <section class="panel">
              <h2>Request inspector</h2>
              <p class="muted">Read the full request log by trace ID.</p>
              <form
                class="inline-form"
                onsubmit={(e) => {
                  e.preventDefault();
                  action(
                    async () =>
                      (requestLog = await api(
                        `/observability/logs/requests/${encodeURIComponent(requestId.trim())}`,
                        "GET",
                        undefined,
                        "text",
                      )),
                  );
                }}
              >
                <label
                  >Request ID<input
                    bind:value={requestId}
                    required
                    placeholder="8-character trace ID"
                  /></label
                ><button disabled={busy}>Read log</button>
              </form>
              {#if requestLog}<pre class="request-log code">{requestLog}</pre>
                <button
                  onclick={() =>
                    download(
                      new Blob([requestLog]),
                      `request-${requestId}.log`,
                    )}>Download request log</button
                >{/if}
            </section>
            <section class="panel">
              <div class="section-head">
                <h2>Error log files</h2>
                <span class="muted">{logFiles.length} files</span>
              </div>
              {#each logFiles as file}<div class="error-file">
                  <code>{file.name}</code><button
                    onclick={() =>
                      action(async () =>
                        download(
                          await api(
                            `/observability/logs/errors/${encodeURIComponent(file.name)}`,
                            "GET",
                            undefined,
                            "blob",
                          ),
                          file.name,
                        ),
                      )}>Download</button
                  >
                </div>{/each}{#if !logFiles.length}<p class="muted spacious">
                  No error files. A quiet server is a good server.
                </p>{/if}
            </section>
          </div>
        {:else if route === "plugins"}
          <div class="section-toolbar">
            <span
              class="status"
              class:neutral={!readPath(config, "plugins/enabled", false)}
              ><i></i>Native plugins {readPath(config, "plugins/enabled", false)
                ? "enabled"
                : "disabled"}</span
            >
            <div class="head-actions">
              <button onclick={() => edit("plugins", "Plugin configuration")}
                >Configure plugins</button
              ><button
                disabled={busy}
                onclick={() =>
                  action(async () => {
                    const result = await api("/plugins/store");
                    store = result.plugins || [];
                    if (result.source_errors?.length)
                      throw new Error(
                        result.source_errors
                          .map((s: Data) => s.message)
                          .join("; "),
                      );
                  })}>Browse plugin store</button
              >
            </div>
          </div>
          <section class="panel">
            <div class="section-head">
              <h2>Installed plugins</h2>
              <span class="muted">{plugins.length} discovered</span>
            </div>
            {#each plugins as plugin}<div class="plugin-row">
                <div class="plugin-monogram">
                  <Icon name="plugins" size={26} />
                </div>
                <div>
                  <h3>{plugin.metadata?.name || plugin.id}</h3>
                  <p class="muted">
                    {plugin.metadata?.version || "Version not reported"} · {plugin.registered
                      ? "Registered"
                      : "Restart required / not loaded"}
                  </p>
                  <div class="model-tags">
                    {#if plugin.supports_oauth}<span class="tag">OAuth</span
                      >{/if}{#if plugin.supports_quota}<span class="tag"
                        >Quota</span
                      >{/if}
                  </div>
                </div>
                <div class="head-actions">
                  <button
                    disabled={busy}
                    onclick={() =>
                      action(async () => {
                        await api(
                          fieldPath(`plugins/configs/${plugin.id}/enabled`),
                          "PUT",
                          !plugin.enabled,
                        );
                        await load();
                      }, "Plugin setting saved. A restart may be required.")}
                    >{plugin.enabled ? "Disable" : "Enable"}</button
                  ><button
                    onclick={() =>
                      edit(
                        `plugins/configs/${plugin.id}`,
                        `${plugin.id} configuration`,
                      )}>Configure</button
                  ><button
                    class="danger-text"
                    disabled={busy}
                    onclick={() => {
                      if (
                        confirm(
                          `Delete plugin ${plugin.id}, its binary, and configuration? A restart may be required.`,
                        )
                      )
                        action(async () => {
                          await api(
                            `/plugins/${encodeURIComponent(plugin.id)}`,
                            "DELETE",
                          );
                          await load();
                        }, "Plugin deleted. Check whether a restart is required.");
                    }}>Delete</button
                  >
                </div>
              </div>{/each}{#if !plugins.length}{@render empty(
                "A lean core. Extend when needed.",
                "No plugins installed. Browse the store for trusted provider integrations.",
              )}{/if}
          </section>
          {#if store.length}<section class="panel">
              <div class="section-head">
                <h2>Plugin store</h2>
                <span class="muted">Native code runs inside your server.</span>
              </div>
              {#each store as plugin}<div class="store-row">
                  <div>
                    <h3>
                      {storeText(plugin.name || plugin.id)}
                      <small>{plugin.version}</small>
                    </h3>
                    <p class="muted">{storeText(plugin.description)}</p>
                  </div>
                  <button
                    disabled={busy}
                    onclick={() => {
                      if (
                        confirm(
                          `Install ${plugin.name || plugin.id}? Plugins run trusted native code inside the server. Verify the publisher before proceeding. A restart may be required.`,
                        )
                      )
                        action(async () => {
                          const result = await api(
                            `/plugins/store/${encodeURIComponent(plugin.id)}/install${plugin.source_id ? `?source=${encodeURIComponent(plugin.source_id)}` : ""}`,
                            "POST",
                            {},
                          );
                          await load();
                          notify(
                            result.restart_required
                              ? "Installed. Restart the server to load this plugin."
                              : "Plugin installed.",
                          );
                        });
                    }}
                    >{plugin.installed
                      ? plugin.update_available
                        ? "Update"
                        : "Reinstall"
                      : "Install"}</button
                  >
                </div>{/each}
            </section>{/if}
          <div class="notice warn">
            Plugin binaries execute in-process. Install only code you trust.
            Global enablement and per-plugin enablement are independent.
          </div>
        {:else if route === "system"}
          <section class="system-banner">
            <span class="system-emblem"><Icon name="terminal" size={50} /></span
            >
            <div>
              <h2>cliproxy-rs</h2>
              <p>A fast proxy. A clear control room.</p>
              <span class="status"
                ><i></i>UI connected to {version
                  ? `CLIProxyAPI ${version}`
                  : "a v8-compatible server"}</span
              >
            </div>
            <span class="system-api">v8<span>API contract</span></span>
          </section>
          <section class="panel">
            <div class="section-head">
              <h2>Server connection</h2>
              <span class="status" class:warn={!online}
                ><i></i>{online ? "Connected" : "Offline"}</span
              >
            </div>
            <div class="info-grid">
              <div>
                <span>Endpoint</span><code>{server}/v8/management</code>
              </div>
              <div>
                <span>Server version</span><strong
                  >{version || "Not exposed by server headers"}</strong
                >
              </div>
              <div>
                <span>Routing strategy</span><strong
                  >{readPath(
                    config,
                    "routing/strategy",
                    "Runtime default",
                  )}</strong
                >
              </div>
              <div>
                <span>Last successful probe</span><strong
                  >{checked ? stamp(checked) : "Not yet checked"}</strong
                >
              </div>
              <div>
                <span>Configuration layout</span><strong
                  >{config["config-version"] ||
                    "Legacy / migrated v8 view"}</strong
                >
              </div>
              <div>
                <span>Credential files</span><strong
                  >{accounts.length} real accounts</strong
                >
              </div>
            </div>
          </section>
          <section class="panel">
            <div class="section-head">
              <div>
                <h2>Release information</h2>
                <p class="muted">
                  Checks the upstream release feed exposed by your server.
                </p>
              </div>
              <button
                disabled={busy}
                onclick={() =>
                  action(
                    async () => (latest = await api("/server/latest-version")),
                  )}>Check latest version</button
              >
            </div>
            {#if latest}<pre class="code release-result">{JSON.stringify(
                  latest,
                  null,
                  2,
                )}</pre>{:else}<p class="muted spacious">
                No automatic update checks. Your server stays in control.
              </p>{/if}
          </section>
          <div class="notice">
            Server status means the authenticated Management API responds. This
            API does not expose CPU, memory, process uptime, or historical
            latency. Those values are not invented here.
          </div>
        {/if}
      </main>
      <footer>
        <span>cliproxy-rs</span><span>Your proxy, in focus.</span><span
          >Management API v8</span
        >
      </footer>
    </div>
  </div>
{/if}
{#if toast}<div class="toast" role="status">
    <span class="connection-dot"></span>{toast}
  </div>{/if}
