---
name: cliproxy-rs Observatory
description: A precise, calm instrument panel for proxy operations.
colors:
  dark-background: "#101722"
  dark-surface: "#151e2b"
  dark-sidebar: "#0d131d"
  dark-text: "#e9eef6"
  dark-muted: "#99a8bc"
  dark-line: "#2a3647"
  ice-action: "#a4c7ff"
  mint-health: "#8ad5b2"
  light-background: "#f4f6f9"
  light-surface: "#ffffff"
  light-text: "#19283c"
  light-muted: "#5a697e"
  light-line: "#d9e1eb"
  blue-action: "#285da2"
rounded:
  panel: "12px"
  control: "7px"
typography:
  body:
    fontFamily: "Public Sans, system-ui, sans-serif"
  code:
    fontFamily: "ui-monospace, SFMono-Regular, Consolas, monospace"
---

# Design System: cliproxy-rs

## Overview

Observatory is an operator-facing instrument panel. Fine rules organize dense controls; typography, alignment, and status color carry the hierarchy. Measurement strips and an open account table replace a pile of metric cards. The selected direction and rejected alternatives are retained in `design/directions.png` and `DIRECTION.md`.

## Colors

`src/style.css` is the normative source of CSS variables. Midnight slate is the default dark world; porcelain blue-gray is the light world. Ice/blue marks actions and traffic, mint/green marks health, amber marks warnings, and red marks errors. Status also has visible text and an icon or dot; color is never its only meaning.

## Typography

Public Sans is served locally, preloaded, and used for headings, body, labels, and controls. Use the operating system monospace stack for telemetry, keys, diffs, and log lines. Strong measurement figures outrank the supporting labels. Avoid decorative display fonts inside operational forms.

## Layout

Desktop uses a fixed compact sidebar and a slim connection bar. Overview is a horizontal measurement strip, a wide traffic chart with provider mix alongside, and the account table below. Narrow widths stack content, reflow account rows, and expose every navigation link in a two-column grid rather than hiding clipped tabs. Panels retain their content-proportional height. Editors expand inline in the current page.

## Elevation & Depth

Hairline borders and adjacent surface values establish boundaries. Ordinary panels have no shadow. Only action menus and transient status toasts lift from their surface. No gradients, glows, glass blur, or decorative backdrops.

## Shapes

Panels use the 12px radius token; controls use 7px corners. Small status indicators and provider monograms are compact and consistent. Icons are hand-written SVG paths with one stroke language, not a downloaded icon library.

## Components

Native buttons, selects, inputs, details, progress, and meter elements are styled with plain CSS. Preserve focus rings, descriptive labels, keyboard behavior, and disabled states. Config edits preview removed/added lines before submission and remain visible on error. Destructive operations use native confirmation. Chart cursor and toast entrance motion respect reduced-motion preferences. Never animate layout dimensions or delay navigation with page transitions.

## Do's and Don'ts

- Keep operation labels literal, metrics truthful, and unknown values visibly absent.
- Keep development sample warnings and read-only actions obvious.
- Preserve unrecognized API fields when editing provider groups.
- Use consistent rules, density, and spacing; do not turn every sentence into a card.
- Do not render remote plugin markup as HTML, persist management keys, or claim an unavailable server metric.
