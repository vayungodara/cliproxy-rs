# Rescue copy for PR #284 (20 November 2026)

This branch holds work for #284 that existed only on vayun-core, which lost its connection on 20 November. It starts from the head of #284 at that time (`cb6c39fa`) and adds only this `rescue-284/` directory. It is not meant to be merged. Delete it when #284 is done.

## What is here

| Path | What it is |
| --- | --- |
| `recapture/` | The script that retakes `docs/img/overview-light.png` and `overview-dark.png`, with its mock provider, traffic generator and browser capture |
| `dashboard-trial/` | Output of the 210-minute trial run on 18 November: eight pictures and the logs they came from |
| `checks/` | Scripts used to check #284: the GitHub render captures, table widths, the README link check, the replay of the Add group form's write, and a probe of which dashboard a binary serves |

Nothing here holds a real key. The keys in the config, logs and pictures are made up (`sk-shot-...`), and every request went to a mock on loopback.

## Retaking the dashboard screenshots

```sh
HEIGHTS=1100 rescue-284/recapture/run.sh <cliproxy binary> <out-dir>
```

- It needs `unshare`, `ip`, `curl`, `python3`, `node`, a Chromium at `/usr/bin/chromium` (or `CHROME_PATH`) and a `playwright-core` directory in `PW`. The default `PW` is a path on vayun-core.
- Everything runs in a network namespace with loopback only: the mock provider on 9101, the proxy with a temporary config and auth-dir, 275 requests through the proxy, then a headless browser on the Provider keys page, OpenAI-compatible tab.
- The run takes 210 minutes. The dashboard's traffic view is twenty 10-minute buckets on the wall clock, so a shorter run fills only the last buckets. `run.sh <binary> <out-dir> 30 30` is a 30-second check that the pieces work.
- `DIST=<path to ui/dist>` serves that dashboard build in front of the binary. The trial used it, because the 0.4.4 binary embeds the dashboard from before #282. A binary that already has #282 needs no `DIST`.
- Output: `plain/` shows the page as served, with the mock's base URLs. `relabelled/` is the same page with those URLs replaced by the providers' real ones on their way to the browser; everything else comes from the server. Files are named `overview-<theme>-<height>.png`, and height 1100 gives 2560 x 2200.

Decided for the final pictures: 2560 x 2200, the relabelled URLs, taken on the 0.4.5 release binary once it is published. The PR description and the alt text must say that the base URLs were rewritten and that the data is demo data from a local mock. The alt text is reworded to match the new mock log.

## The trial run

It used the 0.4.4 release binary with master's `ui/dist` (8004a890) in front. All 275 client responses were 200. The mock answered 14 requests with 429, all on the key ending `451d`. The page made no request outside loopback.

| Key | Requests shown | Failed | Red buckets |
| --- | --- | --- | --- |
| DeepSeek `sk-...451d` | 50 | 14 | 5 in a row, the 8th to 12th of 20 |
| DeepSeek `sk-...b518` | 56 | 0 | none |
| GLM `2bf...dc99` | 77 | 0 | none |
| Kimi `sk-...e5c2` | 49 | 0 | none |
| OpenRouter `sk-...dad6` | 56 | 0 | none |

Kimi shows 49 on the page and 50 in the mock log, because one request was older than the 200-minute window when the page was captured. A new run gives different counts, so take the alt text's numbers from that run's `run.log`.

## State of #284 when this was pushed

- Head `cb6c39fa`, exact-head CI green, nothing unpushed. Master had moved to `71ae3eeb` (#283), which does not touch `README.md`, `docs/demo` or `docs/img`, and a trial merge was clean.
- Still to do, as approved by Vayun: make the other-proxies table fit the README column; after 0.4.5 is published, remove the sentence about v0.4.4 and retake the two dashboard pictures.

## The other-proxies table

It needs 933 px. GitHub's README column was 861 px on the blob page at a 1280 px wide window. The six columns were 129, 122, 193, 181, 181 and 126 px wide. Two things that cannot wrap set the wide ones: the code span `config.example.yaml` in the CLIProxyAPI column, and the code span `/chat/completions` followed by a comma in the claude-code-router and LiteLLM columns. If both could wrap, the table would need 781 px. GitHub removes `<wbr>` from a README, so that tag does not help (tried in 50b5ecee, undone in c39032c8).
