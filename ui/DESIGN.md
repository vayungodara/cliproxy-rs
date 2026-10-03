---
name: cliproxy-rs Instrument
description: The proxy drawn as a well-made device, after Rams-era Braun.
colors:
  light-body: "#e8e6e1"
  light-panel: "#dddad3"
  light-raised: "#f4f3ef"
  light-line: "#d3cfc7"
  light-ink: "#1c1b19"
  light-ink-2: "#5b5850"
  light-hole: "#c9c5bc"
  dark-body: "#1a1917"
  dark-panel: "#121110"
  dark-raised: "#262420"
  dark-line: "#2e2b27"
  dark-ink: "#ece8e0"
  dark-ink-2: "#a39e94"
  dark-hole: "#2b2925"
  accent: "#f2c230"
  accent-pressed: "#e8b71c"
  ok: "#2e9150 / #47b26b"
  warn: "#e2771f / #ee8a3c"
  bad: "#d23e28 / #ec5a43"
  warn-text: "#8f4006 / #ee8a3c"
rounded:
  key: "999px"
  field: "8px"
  window: "12px"
typography:
  ui:
    fontFamily: "Host"
  code:
    fontFamily: "ui-monospace, SF Mono, Cascadia Mono, Menlo, Consolas"
---

# Design system: Instrument

## World

The dashboard is drawn as the face of a calm, precise device, after Dieter Rams' Braun work: the ET66 calculator, the T3 radio, the SK4. The device vocabulary is the component vocabulary: a recessed **display window** for readings, round **keys** for actions, small **lamps** for state, and a perforated **grille** for traffic. Everything else is space, alignment and hairlines.

Light theme is the grey device body for daytime. Dark theme is the black ET66 for late-night work. Both use the same structure and the same accent.

## Anti-references

- The KPI-card SaaS dashboard: a row of equal metric cards, a gradient area chart, every section in a bordered card.
- The previous "Observatory" UI of this project: taglines, status said three times, an empty chart dominating first run.
- Terminal cosplay: green-on-black monospace everywhere.
- Grafana-style walls of dark panels; glass, glow and gradients.
- Skeuomorphic hardware: screws, brushed metal, RJ45 ports. Braun is the grammar, not a costume.

## Colour

`src/style.css` is the source of truth. Strategy: restrained. Neutrals carry 95% of the surface.

- **Body** is the device shell. **Panel** is recessed (display windows, segmented controls, code). **Raised** is a key or field face.
- **Accent** (Braun yellow `#f2c230`) appears in exactly three places: the primary key, the current-page dot, and text selection. Never on a status.
- **Lamps**: green ready, orange cooling, red failing, hollow ring disabled or not available. A lamp always has a word next to it or an accessible label.
- **Grille**: unlit holes use `--hole`; lit holes are ink at four strengths; a hole is red when its bucket had failures.
- Text uses `--ink` or `--ink-2` (5:1 or better on body and panel in both themes, checked with axe-core). `--ink-3` is never text: it draws hole edges, hollow lamps and hover borders.
- Warning text in logs uses `--warn-text`, a darker orange in the light theme, because the lamp orange is too light for small text.
- Depth comes from two shadow tokens only: `--sunk` (inset, for recessed windows, tracks and nav holes) and `--lift` (a 1 px top highlight and a soft drop, for key faces). Lamps add a small inner highlight. The yellow key darkens to `#e8b71c` on hover.

## Type

One family, Host Grotesk (declared as `font-family: Host`), self-hosted as a 17.5 KB Latin subset with weights 400–650 and falling back to `system-ui`. Its figures are tabular by default, so numbers align without features.

| Role | Size / weight |
| --- | --- |
| Display reading | 52 px / 500, −0.035em (40 px on phones) |
| Page title | 24 px / 600, −0.02em |
| Section title | 17 px / 600 |
| Body, controls | 14 px / 400–500 |
| Legends, meta | 12.5–13 px / 400, `--ink-2` |

Monospace is for data only: keys, file names in detail views, paths, log lines and diffs. Sentence case everywhere; no tracked capitals, no eyebrows above headings.

## Layout

- Desktop: a 216 px key column (wordmark, pages, theme and sign-out keys) and a content column up to 1200 px with 48 px gutters. Connection state and server version appear once, top right.
- Pages stack sections with 40 px between them; a section title sits 16 px above its content.
- Lists are open: hairline rows, no card. Groups (by provider) are a bold label and a count.
- Phones (≤ 860 px): a top bar with wordmark, connection lamp and a menu key that opens the page list full-screen. Rows reflow into two lines, the grille under the name. Touch targets are at least 40 px.

## Components

- **Key**: pill, raised face with a 1 px border and a faint top highlight; pressing moves it down 1 px and sinks the shadow. Primary is yellow with dark text. Quiet keys have no border. Danger is red text, never a red fill.
- **Display window**: 12 px radius, inset shadow, readings separated by hairlines. Used on Overview and the live Usage view only.
- **Grille**: SVG circles, 20 ten-minute buckets, oldest left. Rows use 7 px holes; the Overview display stacks five holes per bucket like a level meter. Each grille has an accessible summary ("86 requests in 200 minutes, 86 failed").
- **Segmented control**: recessed track, the selected option is a raised key face, `aria-pressed`.
- **Fields**: raised face, 8 px radius, label above in `--ink-2`. Focus is a 1 px ink ring plus the border.
- **States**: loading is one breathing skeleton row; empty explains what to do next; not available names the route and status code; errors keep the last good data visible when there is any.
- **Toast**: an ink pill at the bottom centre. Success clears itself after four seconds; errors stay until dismissed and use `role=alert`.
- Icons are 15 hand-drawn 24 px strokes (1.7 px, round caps) in an SVG sprite in `index.html`.

## Motion

Motion only reports state. Keys press (120 ms). Lamps on live things (log tail, OAuth waiting, live usage) breathe on a 2.4 s cycle. Toasts rise 8 px in 200 ms. No page transitions, no entrance choreography. `prefers-reduced-motion` turns all animation and transition off.

## Do and don't

- Do show each fact once, in the place it is used.
- Do disable an action the server lacks and say so in one line. Don't offer it and let it fail.
- Do keep explanatory copy to the actions that are costly to get wrong (deleting, draining the usage queue, installing native plugins).
- Don't add taglines, cards around sections, gradients, glows, or a second accent colour.
- Don't render remote plugin HTML, store the management key, or show a metric the API does not report.
