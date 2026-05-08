# Codes

Short codes used on the wire. Drafted at M0; finalized at M1 when the
encoder/decoder lands.

## Role codes

Roles map first from ARIA (`role` attribute or implicit role); HTML
element fallback is secondary.

| Code | ARIA / element | Notes |
| ---- | -------------- | ----- |
| `doc`  | `document` / `<html>`     | The page root. |
| `btn`  | `button` / `<button>`     | Includes `<input type="submit/button">`. |
| `lnk`  | `link` / `<a href>`       | |
| `tf`   | `textbox` / `<input type="text\|email\|password\|...">` | Single-line text input. |
| `ta`   | `textbox` / `<textarea>`  | Multi-line text input. |
| `sel`  | `combobox` / `<select>`   | |
| `chk`  | `checkbox` / `<input type="checkbox">` | |
| `rad`  | `radio` / `<input type="radio">`       | |
| `img`  | `img` / `<img>`           | |
| `hd`   | `heading` / `<h1>..<h6>`  | Level encoded as `level=N`. |
| `p`    | `paragraph` / `<p>`       | |
| `li`   | `listitem` / `<li>`       | |
| `lst`  | `list` / `<ul>`, `<ol>`   | |
| `tbl`  | `table` / `<table>`       | |
| `row`  | `row` / `<tr>`            | |
| `cell` | `cell` / `<td>`           | |
| `hdr`  | `columnheader` / `<th>`   | |
| `nav`  | `navigation` / `<nav>`    | |
| `frm`  | `form` / `<form>`         | |
| `dlg`  | `dialog` / `<dialog>`     | |
| `itm`  | `menuitem`, `tab`, `option` | Generic interactive item. |
| `sec`  | `region` / `<section>`    | |
| `art`  | `article` / `<article>`   | |
| `mn`   | `menu` / `<menu>`         | |

When no specific role applies, the daemon emits `el` and includes a
`tag=<name>` attribute so the agent can disambiguate.

## Operation codes

Operations listed inline on each tree row, comma-separated:

| Op | Meaning |
| -- | ------- |
| `click`  | Activate (mouse click or `Enter`). |
| `fill`   | Replace text content. Requires `<val>`. |
| `scroll` | Scroll the element into view, or by amount with `<val>`. |
| `key`    | Send a key chord; `<val>` is the chord (`Enter`, `Ctrl+K`, ...). |
| `submit` | Submit the enclosing form. |
| `hover`  | Move pointer over the element. |
| `focus`  | Move keyboard focus. |

## Viewport presets

Used by `vs_viewport <preset>`. DPR defaults to 2; override per call.

| Preset       | Width × Height |
| ------------ | -------------- |
| `mobile-s`   | 320 × 568      |
| `mobile`     | 390 × 844      |
| `mobile-l`   | 414 × 896      |
| `tablet`     | 768 × 1024     |
| `tablet-l`   | 1024 × 768     |
| `laptop`     | 1366 × 768     |
| `desktop`    | 1920 × 1080    |
| `desktop-xl` | 2560 × 1440    |
| `wide`       | 3440 × 1440    |

## Error codes (`! <CODE> [args]`)

| Code                | Args                     | Meaning |
| ------------------- | ------------------------ | ------- |
| `STALE_TOKEN`       | `<new_token> <reason>`   | Pre-image token did not match. Reasons: `nav`, `mutate`, `expired`. |
| `ENGINE_UNSUPPORTED`| `<primitive> <engine>`   | The active engine cannot service this primitive. |
| `POLICY_DENY`       | `<rule> <subject>`       | Policy blocked the action. |
| `TIMEOUT`           | `<budget> <primitive>`   | Timeout fired before completion. |
| `NOT_FOUND`         | `ref=<n>` or `mark=<name>` | Target does not exist. |
| `CONFIRM_REQUIRED`  | `<reason> [<detail>...]` | Action requires explicit confirmation. |
| `DAEMON_START_FAILED` | —                      | CLI could not start or reach the daemon. |
| `ENGINE_CRASH`      | —                      | Engine thread died; daemon refusing engine work. |
| `BAD_REQUEST`       | `<detail>`             | Malformed request line. |

## Warning codes (`? <code> [args]`)

| Code               | Args              | Meaning |
| ------------------ | ----------------- | ------- |
| `nav`              | `<new_url>`       | Page navigated; baseline reset. |
| `captcha_visible`  | —                 | Heuristic detector matched. |
| `auth_loaded`      | `<name>`          | Auth blob applied; baseline reset. |
| `viewport_changed` | `<W>x<H>`         | Viewport changed; baseline reset. |
| `idempotent_hit`   | —                 | Repeat call on same `(page, token, args)` returned cached result. |
