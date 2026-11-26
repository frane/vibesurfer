# Not detected as automated

Anti-bot systems (Cloudflare, hCaptcha, reCAPTCHA, DataDome) check `event.isTrusted` first, then movement timing, then TLS and HTTP fingerprints. vibesurfer's input is built to pass all three.

## Trusted input

Clicks, cursor moves, drags, wheel turns and typing are native OS input on every engine, so every event carries `isTrusted = true`, as from a real cursor and keyboard:

- **macOS**: `NSEvent` mouse and key events on the `WKWebView`.
- **Linux**: XTest over `x11rb` (X11, Xwayland), or the RemoteDesktop portal under pure Wayland.
- **Windows**: `SendMouseInput` on a WebView2 composition controller; keys through the DevTools protocol, since WebView2's visual hosting has no keyboard injection of its own.

That covers `vs act click` on a ref (all engines since 0.2.21) and the coordinate commands `vs click-at`, `vs hover-at`, `vs move-to`, `vs drag`, `vs wheel` and `vs type`. In `human` mode a click is preceded by a Bezier path from the last cursor position with Fitts-law arrival timing, so the pointer reaches the target the way a hand does. `vs type` sends the full keydown, beforeinput, input sequence, which is why DraftJS and ProseMirror editors accept it where `act fill` does not.

The tree walker honors ARIA `role` (Radix, Headless UI, Reach UI and every div-as-button) and treats focusable divs and spans as actionable, so React UIs come out as refs without coordinate workarounds.

## Where it stands

Run by vibesurfer against each site, macOS / WKWebView, v0.2.2. These are our own numbers, not a comparison with another browser. Each engine reports as itself: Safari on the WebKit engines, Chromium on Windows, where WebView2 is Chromium. The Chromium-only rows read differently there by design.

| Probe | Result |
|---|---|
| [bot.sannysoft.com](https://bot.sannysoft.com/) | Every headless-artifact check passes: WebDriver, plugin array, languages, the PHANTOM_* and HEADCHR_* families, Selenium markers. `window.chrome` reports missing, which is correct for a WebKit browser. |
| [bot.incolumitas.com](https://bot.incolumitas.com/) | 31 checks OK, 1 FAIL (`webDriverAdvanced`). `navigator.webdriver` is `false` behind a native getter on `Navigator.prototype`, with no own property and absent from `Object.keys`. That is spec-correct; the check expects a Chrome-shaped descriptor. |
| [CreepJS](https://abrahamjuliot.github.io/creepjs/) | Stable fingerprint, no lies section. Headless heuristics: `chromium: false 6%`, `like headless: 33%`, `headless: 20%`. Every flag that fires is a Chromium-only API WebKit lacks (`window.chrome`, Content Index, Contacts Picker, Network Information). The ones that would mean a real problem (`noPlugins`, `noWebShare`, `hasWebDriver`, `noUserActivation`, `headlessUA`, the permissions bug) are clear. `enumerateDevices` reports a mic and a webcam. |
| In-repo `fingerprint` cell | Passes on all three engines on every commit. No automation artifacts (`Function.prototype.toString` native for every builtin we replace, no enumerable `__vs*` globals) and no impossible values (`hasFocus`, non-zero `outerWidth`/`outerHeight`, visible document, sane screen, DPR and concurrency). |

Two of those artifacts were ours in 0.2.1: the download shim's replacements stopped reporting `[native code]`, and the shim globals were enumerable on `window`. Both were fixed in 0.2.2 on all three engines, and the cell keeps them from coming back.

The remaining score on each probe is a Chromium-shaped expectation. `window.chrome`, `navigator.connection`, the Contacts and Content Index APIs and a `webdriver` property that reads as absent are things Chrome has and Safari does not; a real Safari scores the same. We do not add them: a Chromium API in a browser whose User-Agent says Safari is exactly the inconsistency CreepJS's lie detection looks for.

These are heuristic scores that move when the sites change, and passing them is not passing a commercial anti-bot service on a live site. Cloudflare's supported-browsers page puts embedded engines under limited support whatever their configuration. Where a challenge does appear, solve it: `vs view` reports Turnstile, hCaptcha and reCAPTCHA widgets with their box, and one trusted click usually clears them (see [SKILL.md](../skills/vibesurfer/SKILL.md)).
