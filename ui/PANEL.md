# Using the cliproxy-rs dashboard with Go CLIProxyAPI

The cliproxy-rs dashboard also ships as one self-contained file, `dist-panel/management.html`, that drops into an existing Go CLIProxyAPI server in place of the official management panel. It is the same app the Rust binary embeds, built from the same source.

Tested against unmodified CLIProxyAPI at commit `6fecc6e` (v8.0.10), both the release binary and a source build.

## What you get

- Every screen: overview, credentials and account sign-in (OAuth, device codes, callback paste, Vertex import), provider API keys, client keys, models, payload rules, quotas, configuration (JSON and YAML with a reviewed diff), logs, usage, plugins and system.
- It talks only to the server that serves it: the v8 Management API (`/v8/management/...`), plus `GET /v1/models` with a client key when the Use with tools page tests that key. JavaScript, CSS, the font and the icons are inside the file. It makes no other requests: no CDN, no web fonts, no analytics, no update checks of its own.
- The management key is kept in the tab's memory only, never in browser storage. Reloading the page signs you out. The theme choice is the only thing stored.
- It recognises the server from its `X-CPA-VERSION`, `X-CPA-COMMIT` and `X-CPA-BUILD-DATE` headers. On Go it assumes the full v8 API and sends no capability probes; every request it makes is one you triggered or a read for the screen you are on.

## What it does not do

- It does not use the deprecated `/v0/management` API, so Go servers older than v8 are not supported.
- It does not render plugin-defined menu pages; plugins are listed, configured, enabled, installed and deleted.
- It keeps no history beyond what the server reports (twenty 10-minute buckets per credential). The live usage view reads the server's usage queue, which removes those records for other consumers; it asks before starting.
- "Check quota" asks the provider for usage through the server (`/requests/api-call` or a quota plugin) only when you press it.
- It does not update itself. Updates come from replacing the file, by hand or through Go's release updater below.

## Install by hand

1. Download `management.html` and check it against the checksum below.
2. Put it where Go serves the panel from: `static/management.html` next to your `config.yaml`. If you set `WRITABLE_PATH`, it is `$WRITABLE_PATH/static/management.html`. `MANAGEMENT_STATIC_PATH` takes precedence over both: a path ending in `management.html` is the file itself, and any other path is a directory that holds `management.html`.
3. Stop Go from replacing it with the official panel. In a v8 config:

   ```yaml
   management:
     secret-key: "your management key"   # required; Go hashes it on start
     disable-control-panel: false
     disable-auto-update-panel: true
   ```

   In the older layout the same keys live under `remote-management:`.
4. Open `http://<host>:<port>/management.html` and sign in with the management key. For access from another machine, also set `allow-remote: true` and use HTTPS.

Place the file before the first visit to `/management.html`: if the file is missing on that visit, Go downloads the official panel into the same place.

## Install through Go's updater

Go can fetch the panel from a GitHub repository's latest release. Every cliproxy-rs release carries this file as `management.html`, built from the release commit:

```yaml
management:
  panel-github-repository: "https://github.com/vayungodara/cliproxy-rs"
  disable-auto-update-panel: false
```

Go reads `https://api.github.com/repos/vayungodara/cliproxy-rs/releases/latest`, takes the asset named exactly `management.html`, checks it against the asset's `sha256` digest when GitHub provides one, and writes it to `static/management.html`. It checks again every three hours and replaces the file when the release changes.

The updater only sees published releases, not drafts. Until the first release is published, install by hand.

## Checksum

SHA-256 of the `dist-panel/management.html` built from this commit:

<!-- sha256 -->`be8b157ca2fd52dca59f6661fbf13e95210c48a42205cf67ca15946fde35e8db`<!-- /sha256 -->

```sh
sha256sum static/management.html
```

`npm run build` writes this value here and to `dist-panel/management.html.sha256`. The build is deterministic, so building the same commit gives the same file.

## Size

About 184 KB on disk and 71 KB gzip, of which 18 KB is the inlined font. The JavaScript and CSS inside are the same as the Rust build and stay within its budget (49,500 B and 6,853 B gzip).

## Remove

Remove `panel-github-repository` from the config (or set it back to empty), set `disable-auto-update-panel: false`, then delete `static/management.html`. Go downloads the official panel on the next visit. If `panel-github-repository` still points at cliproxy-rs, Go downloads this panel again instead.
