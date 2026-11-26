#![allow(clippy::too_many_lines)]
//! The JSON Schema of every MCP tool, for `tools/list`: one tool per
//! agent-facing `vs` command, named after its wire primitive.

use serde_json::{json, Value};

/// JSON Schema list for `tools/list`. Each tool's name matches the
/// vibesurfer wire primitive (`vs_open`, `vs_view`, …), so an MCP
/// client can recognize them by their wire name.
/// `ui` adds the MCP Apps wiring (SEP-1865): `_meta.ui` on vs_watch
/// pointing at the live panel, plus the app-only vs_live_frame tool
/// the panel polls. Off unless the client declared the
/// `io.modelcontextprotocol/ui` extension — no schema tax otherwise.
pub fn list(ui: bool) -> Vec<Value> {
    let mut tools = base_tools();
    if ui {
        for t in &mut tools {
            if t["name"] == "vs_watch" {
                t["_meta"] = json!({ "ui": { "resourceUri": LIVE_PANEL_URI } });
            }
        }
        let mut frame = tool("vs_live_frame", "One live frame of a page as an image block. Polled by the live panel; not for the model.", obj(&[
            ("page", str_prop("Page id.", true)),
        ]));
        frame["_meta"] = json!({ "ui": { "visibility": ["app"] } });
        tools.push(frame);
    }
    tools
}

/// URI of the live-view panel resource.
pub const LIVE_PANEL_URI: &str = "ui://vibesurfer/live-panel";

fn base_tools() -> Vec<Value> {
    vec![
        tool("vs_session_open", "Create a vibesurfer session. Writes the active-session pointer.", obj(&[
            ("policy", str_prop("Optional policy id (e.g. \"strict\", \"default\").", false)),
            ("isolated", bool_prop("Own in-memory cookie/storage jar, shared by this session's pages only, discarded on close. Default: the shared persistent jar.", false)),
            ("intercept", bool_prop("Route pages through a daemon HTTP proxy for `vs net` (observe/hold/fulfill/abort requests). Implies an own jar.", false)),
            ("popups", bool_prop("Open target=_blank links and window.open() as new pages in the session instead of in place.", false)),
            ("ignore_https_errors", bool_prop("With intercept: accept any upstream certificate (self-signed dev servers).", false)),
        ])),
        tool("vs_session_close", "Close the active session. `all` closes every session in the workspace and `idle_for` (e.g. \"2h\") closes only those untouched that long — the cleanup for sessions left behind by agents that died without closing.", obj(&[
            ("all", bool_prop("Close every session, not just this caller's.", false)),
            ("idle_for", str_prop("Close sessions untouched for at least this long (2h, 90m, 30s).", false)),
        ])),
        tool("vs_open", "Open a page in the active session.", obj(&[
            ("url", str_prop("URL to navigate to.", true)),
            ("capture", bool_prop("Attach a screenshot image block to the result. Default false; VS_THUMBS=1 forces on.", false)),
        ])),
        tool("vs_goto", "Navigate an existing page to a URL in place, reusing its web view (much faster than vs_open for successive navigations). Refs are fresh afterward. Returns the page id + new token.", obj(&[
            ("page", str_prop("Page id to navigate.", true)),
            ("url", str_prop("URL to navigate to.", true)),
        ])),
        tool("vs_close", "Close a page.", obj(&[
            ("page", str_prop("Page id (e.g. p_xxxxxx).", true)),
        ])),
        tool("vs_view", "Snapshot the a11y tree of a page. First call after open is a full tree; subsequent calls are deltas unless --full is set. `? captcha_visible <provider> <state>` means a bot challenge gates the page; the node carries challenge=<provider>:<state> and challenge_box=x,y,w,h. Solve it like anything the tree cannot address: vs_capture, then vs_click_at at (x+28, y+h/2). Never write a token into the response field.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("full", bool_prop("Force a full re-baseline instead of a delta.", false)),
        ])),
        tool("vs_read", "Read the full text of a single ref on a page.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number from a previous vs_view.", true)),
        ])),
        tool("vs_act", "Perform an action on a ref, or on a mark taken earlier with vs_mark. Requires the state token from the most recent vs_view (optimistic concurrency). A click that opens a new window (target=_blank, window.open) loads in this page and returns after it navigates.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number. Give this or `mark`.", false)),
            ("mark", str_prop("Mark name, instead of `ref`. Resolves to whatever ref carries the mark now, so it survives a re-render; warns `mark_reaimed` when the ref moved.", false)),
            ("op", str_prop("One of: click, fill, scroll, key, submit, hover, focus.", true)),
            ("value", str_prop("Optional value (text for fill, key chord for key, etc.).", false)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("group", str_prop("Optional audit-group label (e.g. \"login-flow\").", false)),
            ("mode", str_prop("Ref-click cursor motion: careful (default, fast, trusted), human (slow humanized path for detector-scored flows), robotic (teleport). macOS native click only.", false)),
            ("capture", bool_prop("Attach a screenshot image block to the result. Default false; VS_THUMBS=1 forces on.", false)),
        ])),
        tool("vs_prompt_input", "Prompt the human at the local terminal for a value, then fill it into a ref. The agent that invokes this never sees the value the user types — vs reads from /dev/tty itself and ships the bytes to the daemon, which writes them into the field via the trusted prototype-setter fill path. Use --secret for passwords, TANs, credit-card numbers, and anything else the agent should not see.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number of the input field.", true)),
            ("message", str_prop("Prompt text shown to the human. Include the field label and any disambiguating context (e.g. \"Flatex Kundennummer:\", \"TAN sent to your phone:\").", true)),
            ("secret", bool_prop("Disable terminal echo while reading. Default: false. Set true for any credential.", false)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("group", str_prop("Optional audit-group label.", false)),
        ])),
        tool("vs_prompt_form", "Ask the human for several values at once (e.g. a full login) via a browser form. Returns `form\\t<id>` and `url\\t<localhost URL, valid 10 min, single submit>` immediately. Relay the URL to the user verbatim, then call vs_prompt_form_wait with the form id. Opening or reloading the URL does not use it up, so a link preview cannot break it. The daemon fills each ref on submit; you never see the values.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("fields", json!({
                "type": "array",
                "description": "Fields in fill order.",
                "required": true,
                "items": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "integer", "description": "Ref number of the input to fill."},
                        "label": {"type": "string", "description": "Human-facing label shown on the form (e.g. \"Flatex Kundennummer\")."},
                        "secret": {"type": "boolean", "description": "Render as a masked password input. Default false."}
                    },
                    "required": ["ref", "label"]
                }
            })),
            ("token", str_prop("State token from the most recent read.", true)),
            ("group", str_prop("Optional audit-group label.", false)),
        ])),
        tool("vs_prompt_form_wait", "Park until the vs_prompt_form form is submitted, then fill the refs in order. Returns the new state token. Call right after relaying the URL. `! TIMEOUT` (\\\"still waiting\\\") means the form and its URL are live and the human has not finished: call again with the same form id, do not mint a new form or tell the user the link is dead. Only `cancelled` and `unknown` are dead.", obj(&[
            ("form", str_prop("Form id from vs_prompt_form.", true)),
            ("timeout_ms", uint_prop("How long to wait, in milliseconds. Default 300000 (5 min).", false)),
        ])),
        tool("vs_watch", "Mint a read-only live-view URL for a page (~1 fps screenshots while open, 30 min). Relay it so the human can watch the browser work. Returns `url\\t<URL>`.", obj(&[
            ("page", str_prop("Page id.", true)),
        ])),
        tool("vs_record_start", "Start recording a page to an H.264 MP4 (plays natively everywhere). Captures frames until vs_record_stop. Returns `path\\t<file>`. One recording per page. Downscaled to 960px wide by default.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("fps", uint_prop("Frames per second, clamped 1..=30. Default 24.", false)),
            ("width", uint_prop("Downscale width in px. Smaller = faster encode, lighter files. Default 960. Ignored with retina.", false)),
            ("retina", bool_prop("Record at full device (retina) resolution instead of the downscale. Much larger and heavier.", false)),
            ("format", str_prop("mp4 (default) or webm (VP8).", false)),
        ])),
        tool("vs_record_stop", "Stop recording a page, flush the encoder, and return `path\\t<mp4 file>`.", obj(&[
            ("page", str_prop("Page id.", true)),
        ])),
        tool("vs_pending_url", "Mint a single-use localhost URL where the human can fulfill every pending prompt entry as one browser form. Returns `url\\t<URL>`.", obj(&[])),

        tool("vs_prompt_confirm", "Block until the human at the local terminal presses Enter. Returns `ok` on confirm or aborts on EOF / Ctrl-C. Use as a human-in-loop gate before a sensitive vs_act click (e.g. \"about to transfer $5000 — Enter to confirm\"). No state change; the next call after this still uses whatever state token the agent already had.", obj(&[
            ("page", str_prop("Page id (passed for audit context; the primitive itself does not touch the page).", true)),
            ("message", str_prop("Prompt text shown to the human. State what they are confirming.", true)),
        ])),
        tool("vs_move_to", "Move the cursor to (x, y) along a humanized Bezier path. Trusted (isTrusted=true) on all platforms.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("Target X in CSS pixels (client space, top-left origin).", true)),
            ("y", num_prop("Target Y in CSS pixels.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_click_at", "Click at exact coordinates. Trusted (isTrusted=true) on every engine; the Bezier lead-in from the last cursor position simulates real user motion. Reaches what the tree cannot: canvas, WebGL, map tiles, PDF viewers, cross-origin iframes, closed shadow roots (captchas). Loop vs_capture -> act -> vs_capture; the tree often will not change, so verify visually. Keep the viewport fixed between capture and click. A new-window link or form loads in this page.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("Click X in CSS pixels.", true)),
            ("y", num_prop("Click Y in CSS pixels.", true)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
            ("button", str_prop("left | right | middle. Default left.", false)),
            ("count", num_prop("2 for a double click, 3 for a triple click.", false)),
            ("modifiers", str_prop("Held keys, comma-separated: shift,ctrl,alt,meta.", false)),
        ])),
tool("vs_type", "Type text into the FOCUSED element with trusted keystrokes (real keydown/beforeinput/input, isTrusted=true). Use for rich-text editors (DraftJS/ProseMirror/contenteditable) and framework inputs where vs_act fill is ignored. Place the caret first with vs_click_at. Use --secret for credentials.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("text", str_prop("Text to type into the focused element.", true)),
            ("secret", bool_prop("Redact the text in the audit log (length only). Default false.", false)),
            ("mode", str_prop("Cadence: human | careful | robotic. Default human.", false)),
        ])),
        tool("vs_hover_at", "Hover at coordinates without clicking.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("Hover X in CSS pixels.", true)),
            ("y", num_prop("Hover Y in CSS pixels.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_cookies", "The page's cookie jar, HttpOnly included: op list | clear | set (json: array of {name, value, domain, path, expires_unix?, secure, http_only, same_site?}). Returns the jar as JSON.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("op", str_prop("list | set | clear.", true)),
            ("json", str_prop("JSON array of cookies, for set.", false)),
        ])),
        tool("vs_drop", "Drop files and strings onto (x, y) as a native drag from another app: trusted dragenter, dragover, drop. Returns accepted or refused.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("X in CSS pixels.", true)),
            ("y", num_prop("Y in CSS pixels.", true)),
            ("files", json!({ "type": "array", "items": { "type": "string" }, "description": "File paths." })),
            ("data", json!({ "type": "object", "additionalProperties": { "type": "string" }, "description": "MIME type to string, e.g. {\"text/plain\": \"hi\"}." })),
        ])),
        tool("vs_inspect", "Engine state of a page. kind: console | network | request <seq> | eval <expr> | storage <scope> | scripts | script <seq> | dom | performance | response | cookie-events. Kind-specific positionals go in args.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("kind", str_prop("What to inspect (see description).", true)),
            ("args", json!({ "type": "array", "items": { "type": "string" }, "maxItems": 3, "description": "Positionals after kind, e.g. [\"document.title\"] for eval." })),
            ("frame", str_prop("eval only: the subframe with this URL, or #N (Nth subframe, newest first). Cross-origin too.", false)),
            ("since", str_prop("Only entries after this sequence number.", false)),
            ("level", str_prop("console only: minimum level.", false)),
            ("status", str_prop("network only: status filter.", false)),
            ("max", str_prop("Most entries to return.", false)),
            ("full", bool_prop("Do not truncate values.", false)),
            ("unsafe_log", bool_prop("Write the unredacted expression to the audit log.", false)),
        ])),
        tool("vs_init_script", "Run JavaScript at document start in every later document of the page, before its own scripts (like Playwright's addInitScript). The current document is not touched.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("source", str_prop("JavaScript source.", true)),
        ])),
        tool("vs_net", "Requests and dialogs of an intercept/popups session. op: poll (print and clear events: req/res/fin/fail/sw/dialog/page/close/crash lines) | route (state on|off: hold every request until answered) | continue | fulfill | abort | fetch (perform a held request, keep it held) | body (a kept response body, base64) | dialog (action accept|dismiss, text for a prompt; unanswered dialogs dismiss after 10 s).", obj(&[
            ("op", str_prop("poll | route | continue | fulfill | abort | fetch | body | dialog.", true)),
            ("id", uint_prop("Request or dialog id from poll. All ops but poll and route.", false)),
            ("wait_ms", uint_prop("poll: block up to this long (max 30000) for the first event.", false)),
            ("state", str_prop("route: on | off.", false)),
            ("action", str_prop("dialog: accept | dismiss.", false)),
            ("text", str_prop("dialog: the prompt's answer. Default: its default value.", false)),
            ("url", str_prop("continue/fetch: send to this URL instead.", false)),
            ("method", str_prop("continue/fetch: use this method.", false)),
            ("headers", json!({ "type": "array", "items": { "type": "string" }, "description": "continue/fetch/fulfill: \"name: value\" lines; replaces all headers." })),
            ("body_b64", str_prop("continue/fetch/fulfill: body, base64.", false)),
            ("status", uint_prop("fulfill: status code (default 200).", false)),
            ("error", str_prop("abort: error name in the fail line (default failed).", false)),
        ])),
        tool("vs_wheel", "Move to (x, y) and turn the wheel by (dx, dy) CSS px: a trusted wheel event (positive dy scrolls down).", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("Pointer X in CSS pixels.", true)),
            ("y", num_prop("Pointer Y in CSS pixels.", true)),
            ("dx", num_prop("Horizontal delta in CSS pixels.", true)),
            ("dy", num_prop("Vertical delta in CSS pixels.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_drag", "Press at (x1, y1), drag along a humanized path to (x2, y2), release.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x1", num_prop("Drag start X.", true)),
            ("y1", num_prop("Drag start Y.", true)),
            ("x2", num_prop("Drag end X.", true)),
            ("y2", num_prop("Drag end Y.", true)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_find", "Search across all open pages in the session.", obj(&[
            ("query", str_prop("Substring or pattern.", true)),
        ])),
        tool("vs_wait", "Wait for a condition on a page (stable | text | ref-appears | ref-gone | net-idle | token-change).", obj(&[
            ("page", str_prop("Page id.", true)),
            ("cond", str_prop("Condition name.", true)),
            ("value", str_prop("Optional argument (text substring, ref number).", false)),
            ("timeout", uint_prop("Max wait in milliseconds (default 5000).", false)),
        ])),
        tool("vs_extract", "Extract structured data using a known schema (list, table; form/jsonld/webmcp not yet implemented in the stub).", obj(&[
            ("page", str_prop("Page id.", true)),
            ("schema", str_prop("Schema name.", true)),
            ("token", str_prop("State token from the most recent read.", true)),
        ])),
        tool("vs_mark", "Persist a ref as a named anchor in the session.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number.", true)),
            ("name", str_prop("Anchor name (unique per session).", true)),
            ("token", str_prop("State token from the most recent read.", true)),
        ])),
        tool("vs_annotate", "Attach a (key, value) annotation to a target. Target is one of \"ref:N\", \"mark:NAME\", or \"page\".", obj(&[
            ("target", str_prop("Annotation target (ref:N | mark:NAME | page).", true)),
            ("key", str_prop("Key.", true)),
            ("value", str_prop("Optional value.", false)),
        ])),
        tool("vs_status", "Active session + open pages + backend capabilities.", obj(&[])),
        tool("vs_log", "Slice the audit log.", obj(&[
            ("page", str_prop("Filter by page id.", false)),
            ("group", str_prop("Filter by audit group label.", false)),
            ("since", uint_prop("Filter to actions started at or after this epoch second.", false)),
            ("limit", uint_prop("Max number of rows.", false)),
        ])),
        tool("vs_skill", "Skill bundle management (list, show <name>).", obj(&[
            ("sub", str_prop("Subcommand: list or show.", false)),
            ("name", str_prop("Skill name (for show).", false)),
        ])),
        tool("vs_capture", "Take a screenshot. Defaults to viewport scope; pass a ref to capture an element, or full_page=true.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Optional ref to capture instead of the viewport.", false)),
            ("full_page", bool_prop("Capture the full document, not just the viewport.", false)),
        ])),
        tool("vs_download", "Save a file out of a page to disk; returns the path. With `url`, the page fetches it using its own session cookies (relative URLs resolve against the current document) — this is how you get at a PDF behind a login, or the contents of an iframe (its src shows as an `ifr` node's label in vs_view). Without `url`, drains the newest download the page started on its own (a download link, a viewer's save button, a blob: navigation, an export form answering with an attachment) — headless web views have no download UI, so those are captured rather than saved. `list=true` shows what's captured and waiting. A TIMEOUT leaves the capture in place: call again rather than raising timeout_ms past ~60000.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("url", str_prop("URL to fetch from inside the page. Omit to take a download the page started itself.", false)),
            ("dest", str_prop("Write here instead of the suggested name. Relative paths resolve under the downloads directory.", false)),
            ("list", bool_prop("List captured download intents instead of saving one.", false)),
            ("id", uint_prop("Drain this buffered entry (from list) instead of the newest.", false)),
            ("timeout_ms", uint_prop("How long to wait for the bytes (default 30000).", false)),
        ])),
        tool("vs_viewport", "Set the viewport. Spec is a preset (mobile, desktop, etc.) or WxH.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("spec", str_prop("Preset name or WxH.", true)),
            ("dpr", uint_prop("Device-pixel ratio (default 2).", false)),
            ("zoom", num_prop("Page zoom: the page lays out width/zoom CSS px wide. With spec \"keep\" only the zoom changes.", false)),
        ])),
        tool("vs_layout", "Compute layout boxes (getBoundingClientRect) for one or more refs.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("refs", json!({
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "minItems": 1,
                "description": "Ref numbers."
            })),
        ])),
        tool("vs_auth", "Persist or restore per-origin auth (cookies + storage). Sub: save | load | list | clear | import | webauthn. `import` (rest: name, path-to-json) brings a session captured elsewhere — the passkey fallback: a human logs in with a passkey in a real browser, exports cookies + storage as a v2 auth-blob JSON, and imports it so `load` injects it into a headless page. `webauthn` (rest: page) installs a virtual WebAuthn authenticator (pure-JS ES256, no CDP) so passkey registration and login work headlessly.", obj(&[
            ("sub", str_prop("save | load | list | clear.", true)),
            ("rest", json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Remaining positional args (e.g. PAGE NAME for save/load).",
                "default": []
            })),
        ])),
    ]
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    let mut t = json!({ "name": name, "description": description });
    t["inputSchema"] = schema;
    t
}

fn obj(props: &[(&str, Value)]) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, schema) in props {
        let mut schema = schema.clone();
        // The `*_prop` helpers tag the property with a sentinel
        // `required: true`. We hoist that into the parent object's
        // `required` array (where JSON Schema 2020-12 expects it)
        // and strip it from the property itself — leaving it inline
        // produces a schema that the Anthropic API rejects as
        // invalid.
        let is_required = schema
            .as_object_mut()
            .and_then(|m| m.remove("required"))
            .and_then(|v| v.as_bool())
            == Some(true);
        properties.insert((*name).into(), schema);
        if is_required {
            required.push(Value::String((*name).into()));
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn str_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "string", "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
}

fn uint_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "integer", "minimum": 0, "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
}

fn num_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "number", "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
}

fn bool_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "boolean", "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
}
