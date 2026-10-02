# Concept round, 2026-10-02

Four directions for the Overview and Credentials screens were rendered as Painter concepts before any code was written. The images are concepts, not the build. None of these rasters ship in the app.

| Direction | Concept |
| --- | --- |
| A. Instrument, after Rams-era Braun (ET66, T3). **Chosen.** | [overview, light](https://ampcode.com/user-content/artifacts/80eab032eb50b3ea1cbace50e904f6d45794112453f64a33a0ab2e2857a057be-file.png) · [overview, dark](https://ampcode.com/user-content/artifacts/b519f67733ed52c2ceec924563e4ffb275696199b836f5e6b49e1f7975edc3d4-file.png) · [credentials](https://ampcode.com/user-content/artifacts/46c30ce45928e5efc5761c3ee4760e0e1a968453bbbbacbb89391a79cfeea94d-file.png) |
| B. Signal box: interlocking track diagram on cream enamel | [overview](https://ampcode.com/user-content/artifacts/00d0fcc32f41248e239ccf6cb60b522a183dad94b1578a5daed3655d353bd4df-file.png) |
| C. Transit map: midnight enamel, one line per provider | [overview](https://ampcode.com/user-content/artifacts/fdb34e505a271a07d58ff3dc50d4e71f395808994ded25334bb1f38fc69558d0-file.png) |
| D. Patch panel: black rack faceplate, one port per credential | [overview](https://ampcode.com/user-content/artifacts/c4ea44d2a8d589c323a04a546732a98119e6bf58c7566b64b94279c40efdb30c-file.png) |

The previous dashboard, for comparison: [overview](https://ampcode.com/user-content/artifacts/102d5ee334a8bed6b9269302a5669e80efc3fed85e3b46794479516b2a13ef84-file.png).

## Why A

1. Its signature element shows real data. Go reports twenty 10-minute buckets per credential in `recent_requests`; the grille draws exactly those, so there is no destructive telemetry and no empty chart on first run.
2. It is a control language (keys, lamps, a recessed display, hairlines) rather than one picture, so it carries over to forms, the YAML editor and logs without costume.
3. It is cheap to build well: flat colour, one accent, CSS and SVG dots, no chart library.
4. It works in both themes: grey device body by day, black ET66 at night.

B and C make striking single screenshots, but their diagrams stop working past about six credentials and have nothing to say about configuration or logs. D is a dark-only skeuomorph that puts every credential on a card.

## What A took from the others

- From C: credentials are grouped by provider, and the list ends with an empty "connect another account" row.
- From B: the routing strategy and retry count sit inside the display window, next to the traffic they govern.

## Corrections from concept to build

- The concepts used yellow for the cooling lamp. Yellow is reserved for the primary key and the current page; cooling is orange.
- The credentials concept showed editable per-credential model chips. Go reports those models read-only, so they render as plain chips.
- Tracked capital labels became sentence case.
