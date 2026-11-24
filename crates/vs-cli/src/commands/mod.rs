//! `clap`-derived command tree and dispatch.
//!
//! Each primitive is one subcommand. [`run`] builds a [`Request`] from
//! the subcommand, calls [`Client::call`](crate::client::Client::call),
//! and returns the response; [`render`] formats it for stdout.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use vs_protocol::Request;

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "vs",
    version,
    about = "vibesurfer — agent-native browser CLI",
    long_about = "vibesurfer client and daemon. `vs serve` runs the daemon; everything else sends a request to it over a Unix socket."
)]
pub struct Cli {
    /// Override the active session id (otherwise read from
    /// `$VIBESURFER_HOME/active-session`).
    #[arg(long, short = 'S', global = true)]
    pub session: Option<String>,

    /// Override the daemon socket path. Tests pass an explicit path;
    /// in production this defaults to `$HOME/.vibesurfer/daemon.sock`.
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Override the vibesurfer home directory. Useful for tests.
    #[arg(long, global = true)]
    pub home: Option<PathBuf>,

    /// Skip the daemon auto-spawn step. Tests start the daemon
    /// themselves and connect via `--socket`.
    #[arg(long, global = true)]
    pub no_spawn: bool,

    /// Emit the response as JSON for human inspection. The default is
    /// the line-oriented wire form that agents consume.
    #[arg(long, short = 'j', global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The subcommand vocabulary. One variant per primitive.
///
/// Each variant has a `visible_alias` short form for token economy in
/// agent contexts (mirrors agented's `s` / `i` / `d` / `w` / `br`
/// pattern). Long forms remain for human readers.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 1. Open a new session.
    #[command(visible_alias = "so")]
    SessionOpen {
        #[arg(long)]
        policy: Option<String>,
        /// Give the session its own in-memory cookie and storage jar,
        /// shared by its pages only and discarded on close or daemon
        /// restart. Default sessions share the persistent jar.
        #[arg(long)]
        isolated: bool,
        /// Route the session's pages through a daemon-side HTTP proxy
        /// so `vs net` can observe, hold, fulfill and abort every
        /// request. Implies a private jar, as `--isolated`.
        #[arg(long)]
        intercept: bool,
        /// Open a new page in the session for each `target="_blank"`
        /// link or `window.open()`, with `window.opener` set, instead
        /// of loading it in the clicked page. `vs status` lists it;
        /// an `--intercept` session also reports it on `vs net poll`.
        #[arg(long)]
        popups: bool,
    },
    /// 2. Close the active session, or a whole sweep of them.
    #[command(visible_alias = "sc")]
    SessionClose {
        /// Close every session in the workspace, not just this
        /// caller's. The blunt instrument for a daemon that has
        /// accumulated other people's leftovers.
        #[arg(long)]
        all: bool,
        /// Close every session untouched for at least this long
        /// (e.g. `2h`, `90m`, `3600s`). Implies `--all`'s reach, but
        /// spares whatever is still in use.
        #[arg(long, value_name = "DURATION")]
        idle_for: Option<String>,
    },
    /// 3. Open a page navigated to URL.
    #[command(visible_alias = "o")]
    Open { url: String },
    /// Navigate an existing page to URL in place. Reuses the page's
    /// web view instead of opening a new one, so it skips the
    /// browser-process spin-up and is much faster for successive
    /// navigations. Refs are fresh afterward (re-baselined).
    #[command(visible_alias = "g")]
    Goto {
        page: String,
        url: String,
        /// Load this HTML file as the document under URL instead of
        /// fetching it (WebKit engines). Nothing is fetched, so no
        /// response headers apply.
        #[arg(long, value_name = "FILE")]
        html: Option<String>,
    },

    /// Run a script at document start in every later document of the
    /// page, before its own scripts (like Playwright's addInitScript).
    /// SOURCE is JavaScript, or `@path` to read it from a file. The
    /// current document is not touched.
    InitScript { page: String, source: String },

    /// 4. Close a page.
    #[command(visible_alias = "c")]
    Close { page: String },
    /// 5. View a page (delta by default; `--full` re-baselines).
    #[command(visible_alias = "v")]
    View {
        page: String,
        #[arg(long, short = 'F')]
        full: bool,
    },
    /// 6. Read the full text of a ref.
    #[command(visible_alias = "r")]
    Read {
        page: String,
        #[arg(value_name = "REF")]
        r: u32,
    },
    /// 7. Perform an action on a ref.
    ///
    /// A click that opens a new window (`target="_blank"`,
    /// `window.open()`) loads in this page.
    #[command(visible_alias = "a")]
    Act {
        page: String,
        /// Ref number from `vs view`, or `mark:NAME` for a mark taken
        /// with `vs mark` — the mark resolves to whatever ref carries
        /// it now.
        #[arg(value_name = "REF|mark:NAME")]
        r: String,
        op: String,
        value: Option<String>,
        #[arg(long)]
        token: String,
        #[arg(long)]
        group: Option<String>,
        /// Cursor motion for a ref click: careful (default, fast,
        /// trusted single move), human (slow humanized path for
        /// detector-scored flows), robotic (teleport). Only the
        /// macOS native ref-click path uses this.
        #[arg(long, short = 'M', default_value = "careful")]
        mode: String,
    },

    /// 8. Search across pages in the session.
    #[command(visible_alias = "f")]
    Find { query: String },
    /// 9. Wait for a condition on a page.
    #[command(visible_alias = "w")]
    Wait {
        page: String,
        cond: String,
        value: Option<String>,
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// 13. Status summary for the active session (or all sessions).
    #[command(visible_alias = "st")]
    Status,
    /// 10. Extract structured data using a known schema.
    #[command(visible_alias = "x")]
    Extract {
        page: String,
        schema: String,
        #[arg(long)]
        token: String,
    },
    /// 11. Persist a ref as a named anchor.
    #[command(visible_alias = "m")]
    Mark {
        page: String,
        #[arg(value_name = "REF")]
        r: u32,
        name: String,
        #[arg(long)]
        token: String,
    },
    /// 12. Attach a (key, value) annotation to a target (one of
    ///     `ref:N`, `mark:NAME`, or `page`).
    #[command(visible_alias = "an")]
    Annotate {
        target: String,
        key: String,
        value: Option<String>,
    },
    /// 14. Slice the audit log.
    #[command(visible_alias = "l")]
    Log {
        #[arg(long, short = 'P')]
        page: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long, short = 's')]
        since: Option<i64>,
        #[arg(long, short = 'n')]
        limit: Option<i64>,
    },
    /// 15. Skill management. Subcommand: `list` or `show <name>`.
    ///     (M6 adds `<name> [args]` execution.)
    #[command(visible_alias = "sk")]
    Skill {
        sub: Option<String>,
        name: Option<String>,
    },
    /// 16. Capture a screenshot, or `vs capture clean` to prune old
    ///     screenshots. For a shot: defaults to viewport scope; pass a
    ///     ref to capture an element, or `--full-page`.
    #[command(
        visible_alias = "cap",
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true
    )]
    Capture {
        /// `clean` prunes the captures directory instead of taking a
        /// screenshot. Mutually exclusive with the screenshot args.
        #[command(subcommand)]
        clean: Option<CaptureSub>,
        /// Page id to screenshot (required unless using `clean`).
        page: Option<String>,
        #[arg(value_name = "REF")]
        r: Option<u32>,
        #[arg(long)]
        full_page: bool,
        /// Emit the PNG bytes as base64 on the response body (instead
        /// of just a path on disk). Lets MCP-driven agents see the
        /// screenshot inline; the on-disk PNG is still written.
        #[arg(long, alias = "b64")]
        base64: bool,
    },
    /// 16b. Save a file out of a page. With a URL, the page fetches it
    ///     with its own session cookies (relative URLs resolve against
    ///     the current document). With no URL, the newest download the
    ///     page itself started (a `download` link, a viewer's save
    ///     button, a `blob:` navigation, an export form whose response
    ///     is an attachment) is drained from the capture buffer. Files
    ///     land in `~/.vibesurfer/downloads`.
    #[command(visible_alias = "dl")]
    Download {
        page: String,
        /// URL to fetch from inside the page. Omit to take a download
        /// the page started on its own.
        url: Option<String>,
        /// Write here instead of the default name. Relative paths
        /// resolve under the downloads directory.
        #[arg(long)]
        dest: Option<String>,
        /// List the captured download intents instead of saving one.
        #[arg(long)]
        list: bool,
        /// Drain a specific buffered entry (see `--list`) instead of
        /// the newest.
        #[arg(long)]
        id: Option<u64>,
        /// How long to wait for the bytes, in milliseconds.
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    /// 17. Set the viewport. `spec` is a preset (e.g. `mobile`,
    ///     `desktop`) or `WxH` (e.g. `1280x720`).
    #[command(visible_alias = "vp")]
    Viewport {
        page: String,
        spec: String,
        #[arg(long, default_value_t = 2)]
        dpr: u32,
    },
    /// 18. Compute layout boxes for one or more refs.
    #[command(visible_alias = "lay")]
    Layout {
        page: String,
        #[arg(value_name = "REF", required = true)]
        refs: Vec<u32>,
    },
    /// 19. Auth blob management. Subcommand: `save <page> <name>`,
    ///     `load <page> <name>`, `list`, `clear <name>`,
    ///     `import <name> <file>`, or `webauthn <page>`. `webauthn`
    ///     installs a virtual WebAuthn authenticator (a pure-JS ES256
    ///     software authenticator, no CDP) so passkey registration and
    ///     login work headlessly. `import` brings a session captured
    ///     elsewhere (the passkey fallback): log in with a passkey in a
    ///     real browser, export cookies + local/session storage as a v2
    ///     auth-blob JSON, and import it so `load` can inject it into a
    ///     headless page.
    #[command(visible_alias = "au")]
    Auth {
        sub: String,
        #[arg(num_args = 0..=2)]
        rest: Vec<String>,
    },
    /// 20. Inspect engine state — console, network, request detail,
    ///     storage, scripts, dom, performance, response. The first positional is
    ///     the page id; the second is the kind. Trailing positionals
    ///     are kind-specific (e.g. `request <seq>`, `eval <expr>`,
    ///     `storage <scope>`, `script <seq>`).
    #[command(visible_alias = "i")]
    Inspect {
        page: String,
        kind: String,
        #[arg(num_args = 0..=3)]
        rest: Vec<String>,
        #[arg(long, short = 's')]
        since: Option<String>,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        max: Option<String>,
        #[arg(long, short = 'F')]
        full: bool,
        /// `eval` only: evaluate in the subframe with this URL (or `#N`,
        /// the Nth subframe, newest first), cross-origin ones too.
        #[arg(long)]
        frame: Option<String>,
        #[arg(long = "unsafe-log")]
        unsafe_log: bool,
    },
    /// Move the cursor to `(x, y)` along a humanized Bezier path.
    /// Trusted events on all platforms.
    #[command(visible_alias = "mt")]
    MoveTo {
        page: String,
        x: f64,
        y: f64,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Click at `(x, y)`. Trusted on macOS (`isTrusted = true`). A
    /// new-window link or form loads in this page.
    #[command(visible_alias = "ca")]
    ClickAt {
        page: String,
        x: f64,
        y: f64,
        #[arg(long)]
        token: String,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Hover at `(x, y)`.
    #[command(visible_alias = "ha")]
    HoverAt {
        page: String,
        x: f64,
        y: f64,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Move to `(x, y)` and turn the wheel by `(dx, dy)` CSS px, a
    /// trusted native wheel event (positive dy scrolls down).
    #[command(visible_alias = "wh")]
    Wheel {
        page: String,
        x: f64,
        y: f64,
        #[arg(allow_negative_numbers = true)]
        dx: f64,
        #[arg(allow_negative_numbers = true)]
        dy: f64,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Type text into the FOCUSED element with trusted per-key
    /// events (real keydown/beforeinput/input, isTrusted=true).
    /// Rich-text editors (DraftJS/ProseMirror/contenteditable) and
    /// framework-controlled inputs need this — `act fill` bypasses
    /// their change pipeline. Place the caret first (e.g. `vs
    /// click-at`).
    #[command(visible_alias = "ty")]
    Type {
        page: String,
        text: String,
        /// Redact the text in the audit log (length only).
        #[arg(long)]
        secret: bool,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Drag from `(x1, y1)` to `(x2, y2)`.
    #[command(visible_alias = "dr")]
    Drag {
        page: String,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[arg(long)]
        token: String,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Prompt the user (in the terminal that ran vs) for a value, then
    /// fill it into a ref. The value is read from tty by the CLI and
    /// shipped to the daemon over the local socket; the agent that
    /// invoked vs prompt-input never sees the bytes. Use `--secret`
    /// for passwords, TANs, and other credentials (terminal echo off).
    #[command(visible_alias = "pi")]
    PromptInput {
        page: String,
        #[arg(value_name = "REF")]
        r: u32,
        #[arg(long)]
        message: String,
        #[arg(long)]
        secret: bool,
        #[arg(long)]
        token: String,
        #[arg(long)]
        group: Option<String>,
    },
    /// Print a read-only live-view URL for a page: an HTML page on
    /// 127.0.0.1 streaming ~1 fps screenshots while open. Watch what
    /// the agent's browser is doing from any real browser. The link
    /// is a capability (256-bit nonce, 30 min); `--open` launches it.
    Watch {
        page: String,
        #[arg(long)]
        open: bool,
    },
    /// Ask the human for several values at once via a browser form.
    /// Enqueues one pending form, prints a single-use localhost URL
    /// (open it in any browser; password managers can autofill), then
    /// parks until the form is submitted and fills each ref in order.
    /// The agent that invoked vs prompt-form never sees the values.
    #[command(visible_alias = "pf")]
    PromptForm {
        page: String,
        /// One field per flag: `<ref>=<label>` for a plain field,
        /// `<ref>=<label>,secret` for a masked one. Repeatable; fill
        /// order follows flag order.
        #[arg(long = "field", value_name = "REF=LABEL[,secret]", required = true)]
        fields: Vec<String>,
        #[arg(long)]
        token: String,
        #[arg(long)]
        group: Option<String>,
        /// Open the entry URL in the default browser.
        #[arg(long)]
        open: bool,
        /// Print the URL and form id, then exit instead of waiting.
        /// Park later with `vs prompt-form-wait <FORM>`.
        #[arg(long)]
        no_wait: bool,
        /// How long to wait for the submit, in milliseconds.
        #[arg(long = "timeout-ms", default_value_t = 300_000)]
        timeout_ms: u64,
    },
    /// Block until the user presses Enter in the terminal. Returns
    /// `ok` on confirm or `! ABORTED` if the user sends EOF / Ctrl-C.
    /// Use as a human-in-loop gate before a sensitive `vs act click`.
    #[command(visible_alias = "pc")]
    PromptConfirm {
        page: String,
        #[arg(long)]
        message: String,
    },
    /// Show the human a live view of the page (a QR code, a 2FA
    /// screen, anything visual) and block until they press Enter.
    /// Mints a read-only live-view URL and waits, so out-of-band
    /// steps like scanning a TOTP enrollment QR work headlessly.
    /// `--open` launches the browser at the live view.
    #[command(visible_alias = "ps")]
    PromptScan {
        page: String,
        #[arg(
            long,
            default_value = "Scan the code or complete the step shown, then press Enter."
        )]
        message: String,
        #[arg(long)]
        open: bool,
    },
    /// Wire-only `vs_prompt_input` variant — does NOT read from the
    /// local tty. Used by `vs mcp` so an MCP-driven agent can enqueue
    /// a prompt the local user fulfills via `vs pending fulfill`.
    /// Hidden from `--help`: only the MCP server wires it.
    #[command(hide = true)]
    PromptInputQueue {
        page: String,
        r: u32,
        #[arg(long)]
        message: String,
        #[arg(long, default_value_t = false)]
        secret: bool,
        #[arg(long)]
        token: String,
        #[arg(long)]
        group: Option<String>,
        /// Timeout in milliseconds before the daemon gives up waiting
        /// for `vs pending fulfill <id>` (default 5 min).
        #[arg(long = "timeout-ms", default_value_t = 300_000)]
        timeout_ms: u64,
    },
    /// Wire-only: one transient viewport PNG by page id, no session.
    /// Body is the on-disk path; the caller deletes it after reading.
    /// Plumbing for MCP panel frames / thumbnails. Hidden.
    #[command(hide = true)]
    Frame { page: String },
    /// Park on a form from `vs prompt-form --no-wait` (or one whose
    /// wait timed out) until the human submits, then fill its refs.
    /// `! TIMEOUT ... still waiting` means the form is live: run it
    /// again with the same form id.
    #[command(visible_alias = "pfw")]
    PromptFormWait {
        form: String,
        #[arg(long = "timeout-ms", default_value_t = 300_000)]
        timeout_ms: u64,
    },
    /// List / fulfill / cancel pending `vs_prompt_input` entries
    /// queued by an MCP-driven agent. Use `vs pending list` to see
    /// what's waiting, `vs pending fulfill <id>` to type the value at
    /// the local tty (`--secret` hides echo), `vs pending cancel <id>`
    /// to abort.
    #[command(visible_alias = "pe")]
    Pending {
        #[command(subcommand)]
        sub: PendingSub,
    },
    /// Observe and route the requests of a `--intercept` session.
    /// Request ids come from `vs net poll`.
    Net {
        #[command(subcommand)]
        sub: NetSub,
    },
    /// Record a page to an H.264 MP4 (real-time openh264, plays
    /// everywhere). `start` spawns a background capture at `--fps`;
    /// `stop` flushes and writes the file. Both print the output path.
    /// One recording per page.
    #[command(visible_alias = "rec")]
    Record {
        #[command(subcommand)]
        sub: RecordSub,
    },
    /// Print the address clients connect to: the socket path on Unix,
    /// the named pipe on Windows. For clients in other languages.
    Socket,
    /// Run the daemon in this process. The `vs` binary doubles as the

    /// daemon — `vs serve` is what auto-spawn re-execs when the socket
    /// is missing. SIGINT shuts down cleanly.
    Serve {
        /// Send SIGTERM to the running daemon (PID file at
        /// `~/.vibesurfer/daemon.pid`) and wait for the socket to
        /// disappear. Returns immediately if no daemon is running.
        #[arg(long)]
        stop: bool,
    },
    /// Run a declarative flow: a JSON file that is an array of steps,
    /// each step an array of `vs` arguments. Steps run in order in one
    /// session; `$page` expands to the last opened/navigated page and
    /// `$token` to that page's current state token (fetched as needed),
    /// so login-and-drive scripts need no manual id/token threading.
    /// Stops at the first failing step. Example file contents:
    /// `[["open","https://x"],["act","$page","5","fill","hi","--token","$token"]]`
    Flow {
        #[command(subcommand)]
        sub: FlowSub,
    },
    /// Run the MCP (Model Context Protocol) server over stdio.

    /// Speaks JSON-RPC 2.0; each of the 19 vibesurfer primitives is
    /// exposed as one MCP tool. Wire to Claude Desktop / Claude Code
    /// by configuring `vs mcp` as the server command.
    Mcp,
}

#[derive(Debug, Subcommand)]
pub enum CaptureSub {
    /// Delete screenshot PNGs from the captures directory
    /// (`~/.vibesurfer/captures`, or `$VS_CAPTURES_DIR`). With no flags,
    /// applies the default retention (keep the newest 200, drop anything
    /// older than 30 days) — the same cap the daemon enforces
    /// automatically after each capture.
    Clean {
        /// Delete every capture, ignoring the keep/age limits.
        #[arg(long)]
        all: bool,
        /// Delete captures older than this (e.g. `7d`, `12h`, `30m`,
        /// `90s`; a bare number is seconds).
        #[arg(long, value_name = "DUR")]
        older_than: Option<String>,
        /// Keep only the newest N captures; delete the rest.
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
pub enum FlowSub {
    /// Execute the flow file at PATH.
    Run { file: PathBuf },
}

/// Overrides shared by `vs net continue` and `vs net fetch`.
#[derive(Debug, clap::Args)]
pub struct NetOverrides {
    #[arg(long)]
    url: Option<String>,
    #[arg(long)]
    method: Option<String>,
    /// `name: value`; repeat for more. Replaces all request headers.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    headers: Vec<String>,
    /// Request body, base64.
    #[arg(long = "body-b64", value_name = "B64")]
    body_b64: Option<String>,
}

impl NetOverrides {
    fn apply(&self, mut r: Request) -> Request {
        if let Some(u) = &self.url {
            r = r.flag_value("url", u.clone());
        }
        if let Some(m) = &self.method {
            r = r.flag_value("method", m.clone());
        }
        r = net_headers(r, &self.headers);
        if let Some(b) = &self.body_b64 {
            r = r.flag_value("body-b64", b.clone());
        }
        r
    }
}

/// Repeated `--header` values travel as one newline-joined `--headers`.
fn net_headers(r: Request, headers: &[String]) -> Request {
    if headers.is_empty() {
        r
    } else {
        r.flag_value("headers", headers.join("\n"))
    }
}

#[derive(Debug, Subcommand)]
pub enum NetSub {
    /// Print and clear queued events: `req`, `res`, `fin`, `fail`
    /// lines, each `req`/`res` followed by `h name: value` lines.
    Poll {
        /// Block up to this long (max 30000) for the first event.
        #[arg(long = "wait-ms", default_value_t = 0)]
        wait_ms: u64,
    },
    /// `on` holds every request after its `req` line until continued,
    /// fulfilled or aborted (30 s, then it continues). `off` releases
    /// all held requests.
    Route { state: String },
    /// Send a held request on, optionally changed.
    Continue {
        id: u64,
        #[command(flatten)]
        o: NetOverrides,
    },
    /// Answer a held request without the network.
    Fulfill {
        id: u64,
        #[arg(long, default_value_t = 200)]
        status: u16,
        #[arg(long = "header", value_name = "NAME: VALUE")]
        headers: Vec<String>,
        #[arg(long = "body-b64", value_name = "B64")]
        body_b64: Option<String>,
    },
    /// Answer a `dialog` event (`--popups` sessions): `accept` or
    /// `dismiss`. A prompt takes `--text`, else its default. Dialogs
    /// nobody answers are dismissed after 10 s.
    Dialog {
        id: u64,
        action: String,
        #[arg(long)]
        text: Option<String>,
    },
    /// Fail a held request: the browser connection is closed.
    Abort {
        id: u64,
        /// Error name reported in the `fail` line (default `failed`).
        #[arg(long)]
        error: Option<String>,
    },
    /// Perform a held request and print its response without
    /// delivering it. The request stays held.
    Fetch {
        id: u64,
        #[command(flatten)]
        o: NetOverrides,
    },
    /// Print a kept response body (base64).
    Body { id: u64 },
}

#[derive(Debug, Subcommand)]
pub enum RecordSub {
    /// Start recording a page. Captures until `vs record stop <page>`.
    Start {
        page: String,
        /// Frames per second (clamped 1..=30). Default 24.
        #[arg(long, default_value_t = 24)]
        fps: u32,
        /// Downscale width in px. Smaller is faster to encode and lighter
        /// on disk; larger is sharper. Default 960. Ignored with --retina.
        #[arg(long)]
        width: Option<u32>,
        /// Record at full device (retina) resolution instead of the
        /// default downscale. Much larger and heavier.
        #[arg(long)]
        retina: bool,
    },
    /// Stop recording a page, flush the encoder, print the file path.
    Stop { page: String },
}

#[derive(Debug, Subcommand)]
pub enum PendingSub {
    /// Show all pending `vs_prompt_input` entries the daemon is
    /// holding for fulfillment.
    #[command(visible_alias = "ls")]
    List,
    /// Read the value the daemon is waiting for from the local tty
    /// (rpassword for `--secret`) and send it to the pending entry.
    /// On success the parked `vs_prompt_input` call returns and the
    /// agent observes the filled field.
    #[command(visible_alias = "f")]
    Fulfill {
        /// Pending entry id (from `vs pending list`). If omitted and
        /// there is exactly one pending entry, that one is used.
        id: Option<String>,
    },
    /// Cancel a pending entry. The parked `vs_prompt_input` call
    /// returns `BadRequest "cancelled"`.
    #[command(visible_alias = "c")]
    Cancel { id: String },
    /// Mint a fresh single-use browser URL for the pending queue.
    /// The page lists every pending entry as a form; submitting
    /// fulfills them all — the browser alternative to `fulfill`.
    #[command(visible_alias = "u")]
    Url,
}

impl Command {
    /// Build the wire [`Request`] for this subcommand. Returns `None`
    /// for commands that the CLI handles locally (none yet).
    #[allow(clippy::too_many_lines)]
    pub fn to_request(&self, session_id: Option<&str>) -> Result<Request> {
        Ok(match self {
            Self::SessionOpen {
                policy,
                isolated,
                intercept,
                popups,
            } => {
                let mut r = Request::new("vs_session_open");
                if let Some(p) = policy {
                    r = r.flag_value("policy", p.clone());
                }
                if *isolated {
                    r = r.flag("isolated");
                }
                if *intercept {
                    r = r.flag("intercept");
                }
                if *popups {
                    r = r.flag("popups");
                }
                r
            }
            Self::SessionClose { all, idle_for } => {
                let mut r = Request::new("vs_session_close");
                if let Some(d) = idle_for {
                    r = r.flag_value("idle-for", d.clone());
                } else if *all {
                    r = r.flag("all");
                } else {
                    r = r.arg(require_session(session_id)?);
                }
                r
            }
            Self::Open { url } => {
                let s = require_session(session_id)?;
                Request::new("vs_open")
                    .arg(url.clone())
                    .flag_value("session", s)
            }
            Self::Goto { page, url, html } => {
                let s = require_session(session_id)?;
                let req = Request::new("vs_goto")
                    .arg(page.clone())
                    .arg(url.clone())
                    .flag_value("session", s);
                match html {
                    Some(path) => {
                        use base64::Engine as _;
                        let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
                        req.flag_value(
                            "html",
                            base64::engine::general_purpose::STANDARD.encode(bytes),
                        )
                    }
                    None => req,
                }
            }

            Self::InitScript { page, source } => {
                let s = require_session(session_id)?;
                let js = match source.strip_prefix('@') {
                    Some(path) => std::fs::read_to_string(path)
                        .with_context(|| format!("read init script {path}"))?,
                    None => source.clone(),
                };
                Request::new("vs_init_script")
                    .arg(page.clone())
                    .arg(js)
                    .flag_value("session", s)
            }
            Self::Close { page } => {
                let s = require_session(session_id)?;
                Request::new("vs_close")
                    .arg(page.clone())
                    .flag_value("session", s)
            }
            Self::View { page, full } => {
                let s = require_session(session_id)?;
                let mut r = Request::new("vs_view")
                    .arg(page.clone())
                    .flag_value("session", s);
                if *full {
                    r = r.flag("full");
                }
                r
            }
            Self::Read { page, r } => {
                let s = require_session(session_id)?;
                Request::new("vs_read")
                    .arg(page.clone())
                    .arg(r.to_string())
                    .flag_value("session", s)
            }
            Self::Act {
                page,
                r,
                op,
                value,
                token,
                group,
                mode,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_act")
                    .arg(page.clone())
                    .arg(r.clone())
                    .arg(op.clone());
                if let Some(v) = value {
                    req = req.arg(v.clone());
                }
                req = req
                    .flag_value("session", s)
                    .flag_value("token", token.clone());
                if let Some(g) = group {
                    req = req.flag_value("group", g.clone());
                }
                req = req.flag_value("mode", mode.clone());
                req
            }
            Self::Find { query } => {
                let s = require_session(session_id)?;
                Request::new("vs_find")
                    .arg(query.clone())
                    .flag_value("session", s)
            }
            Self::Wait {
                page,
                cond,
                value,
                timeout,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_wait").arg(page.clone()).arg(cond.clone());
                if let Some(v) = value {
                    req = req.arg(v.clone());
                }
                req.flag_value("session", s)
                    .flag_value("timeout", format!("{timeout}ms"))
            }
            Self::Status => {
                let mut r = Request::new("vs_status");
                if let Some(s) = session_id {
                    r = r.flag_value("session", s.to_string());
                }
                r
            }
            Self::Extract {
                page,
                schema,
                token,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_extract")
                    .arg(page.clone())
                    .arg(schema.clone())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
            }
            Self::Mark {
                page,
                r,
                name,
                token,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_mark")
                    .arg(page.clone())
                    .arg(r.to_string())
                    .arg(name.clone())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
            }
            Self::Annotate { target, key, value } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_annotate")
                    .arg(target.clone())
                    .arg(key.clone());
                if let Some(v) = value {
                    req = req.arg(v.clone());
                }
                req.flag_value("session", s)
            }
            Self::Log {
                page,
                group,
                since,
                limit,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_log").flag_value("session", s);
                if let Some(p) = page {
                    req = req.flag_value("page", p.clone());
                }
                if let Some(g) = group {
                    req = req.flag_value("group", g.clone());
                }
                if let Some(t) = since {
                    req = req.flag_value("since", t.to_string());
                }
                if let Some(l) = limit {
                    req = req.flag_value("limit", l.to_string());
                }
                req
            }
            Self::Skill { sub, name } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_skill").flag_value("session", s);
                let sub = sub.as_deref().unwrap_or("list");
                req = req.arg(sub.to_string());
                if let Some(n) = name {
                    req = req.arg(n.clone());
                }
                req
            }
            Self::Capture {
                clean: _,
                page,
                r,
                full_page,
                base64: _,
            } => {
                // `clean` is handled locally in dispatch.rs before any
                // wire call, so by here we're taking a screenshot and a
                // page id is required. `base64` is a CLI-side
                // post-process — the daemon still returns the on-disk
                // path; dispatch.rs reads the PNG and base64-encodes it
                // before printing.
                let page = page.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("vs capture: missing page id (or use `vs capture clean`)")
                })?;
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_capture")
                    .arg(page.clone())
                    .flag_value("session", s);
                if let Some(rr) = r {
                    req = req.arg(rr.to_string());
                }
                if *full_page {
                    req = req.flag("full-page");
                }
                req
            }
            Self::Download {
                page,
                url,
                dest,
                list,
                id,
                timeout_ms,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_download")
                    .arg(page.clone())
                    .flag_value("session", s);
                if *list {
                    return Ok(req.flag("list"));
                }
                if let Some(u) = url {
                    req = req.arg(u.clone());
                }
                if let Some(d) = dest {
                    req = req.flag_value("dest", d.clone());
                }
                if let Some(i) = id {
                    req = req.flag_value("id", i.to_string());
                }
                req.flag_value("timeout-ms", timeout_ms.to_string())
            }
            Self::Viewport { page, spec, dpr } => {
                let s = require_session(session_id)?;
                Request::new("vs_viewport")
                    .arg(page.clone())
                    .arg(spec.clone())
                    .flag_value("session", s)
                    .flag_value("dpr", dpr.to_string())
            }
            Self::Layout { page, refs } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_layout").arg(page.clone());
                for r in refs {
                    req = req.arg(r.to_string());
                }
                req.flag_value("session", s)
            }
            Self::Auth { sub, rest } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_auth")
                    .arg(sub.clone())
                    .flag_value("session", s);
                for r in rest {
                    req = req.arg(r.clone());
                }
                req
            }
            Self::Inspect {
                page,
                kind,
                rest,
                since,
                level,
                status,
                max,
                full,
                unsafe_log,
                frame,
            } => {
                let s = require_session(session_id)?;
                let kind_long = normalize_inspect_kind(kind);
                let mut req = Request::new("vs_inspect")
                    .arg(kind_long.to_string())
                    .arg(page.clone());
                for r in rest {
                    req = req.arg(r.clone());
                }
                req = req.flag_value("session", s);
                if let Some(v) = since {
                    req = req.flag_value("since", v.clone());
                }
                if let Some(v) = level {
                    req = req.flag_value("level", v.clone());
                }
                if let Some(v) = status {
                    req = req.flag_value("status", v.clone());
                }
                if let Some(v) = max {
                    req = req.flag_value("max", v.clone());
                }
                if *full {
                    req = req.flag("full");
                }
                if *unsafe_log {
                    req = req.flag("unsafe-log");
                }
                if let Some(v) = frame {
                    req = req.flag_value("frame", v.clone());
                }
                req
            }
            Self::MoveTo { page, x, y, mode } => {
                let s = require_session(session_id)?;
                Request::new("vs_move_to")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone())
            }
            Self::ClickAt {
                page,
                x,
                y,
                token,
                mode,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_click_at")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
                    .flag_value("mode", mode.clone())
            }
            Self::HoverAt { page, x, y, mode } => {
                let s = require_session(session_id)?;
                Request::new("vs_hover_at")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone())
            }
            Self::Wheel {
                page,
                x,
                y,
                dx,
                dy,
                mode,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_wheel")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .arg(dx.to_string())
                    .arg(dy.to_string())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone())
            }
            Self::Type {
                page,
                text,
                secret,
                mode,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_type")
                    .arg(page.clone())
                    .arg(text.clone())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone());
                if *secret {
                    req = req.flag("secret");
                }
                req
            }
            Self::Drag {
                page,
                x1,
                y1,
                x2,
                y2,
                token,
                mode,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_drag")
                    .arg(page.clone())
                    .arg(x1.to_string())
                    .arg(y1.to_string())
                    .arg(x2.to_string())
                    .arg(y2.to_string())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
                    .flag_value("mode", mode.clone())
            }
            Self::PromptInput { .. }
            | Self::PromptConfirm { .. }
            | Self::PromptForm { .. }
            | Self::PromptScan { .. } => {
                anyhow::bail!("vs_prompt_* is local; route via main, not the wire dispatcher");
            }
            Self::PromptInputQueue {
                page,
                r,
                message,
                secret,
                token,
                group,
                timeout_ms,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_prompt_input_queue")
                    .arg(page.clone())
                    .arg(r.to_string())
                    .arg(message.clone())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
                    .flag_value("timeout-ms", timeout_ms.to_string());
                if *secret {
                    req = req.flag("secret");
                }
                if let Some(g) = group {
                    req = req.flag_value("group", g.clone());
                }
                req
            }
            Self::Watch { page, .. } => {
                let s = require_session(session_id)?;
                Request::new("vs_watch")
                    .arg(page.clone())
                    .flag_value("session", s)
            }
            Self::Frame { page } => Request::new("vs_frame").arg(page.clone()),
            Self::PromptFormWait { form, timeout_ms } => {
                let s = require_session(session_id)?;
                Request::new("vs_prompt_form_wait")
                    .arg(form.clone())
                    .flag_value("session", s)
                    .flag_value("timeout-ms", timeout_ms.to_string())
            }
            Self::Pending { sub } => match sub {
                PendingSub::List => Request::new("vs_pending_list"),
                PendingSub::Fulfill { id } => {
                    // CLI side reads value from tty before sending the
                    // wire request — handled in dispatch.rs. Here we
                    // just stub a placeholder; dispatch overrides the
                    // value arg with what the user typed.
                    let id_v = id.clone().unwrap_or_default();
                    Request::new("vs_pending_fulfill")
                        .arg(id_v)
                        .arg(String::new())
                }
                PendingSub::Cancel { id } => Request::new("vs_pending_cancel").arg(id.clone()),
                PendingSub::Url => Request::new("vs_pending_url"),
            },
            Self::Net { sub } => {
                let s = require_session(session_id)?;
                let r = match sub {
                    NetSub::Poll { wait_ms } => {
                        let r = Request::new("vs_net_poll");
                        if *wait_ms > 0 {
                            r.flag_value("wait-ms", wait_ms.to_string())
                        } else {
                            r
                        }
                    }
                    NetSub::Route { state } => Request::new("vs_net_route").arg(state.clone()),
                    NetSub::Continue { id, o } => {
                        o.apply(Request::new("vs_net_continue").arg(id.to_string()))
                    }
                    NetSub::Fulfill {
                        id,
                        status,
                        headers,
                        body_b64,
                    } => {
                        let mut r = Request::new("vs_net_fulfill")
                            .arg(id.to_string())
                            .flag_value("status", status.to_string());
                        r = net_headers(r, headers);
                        if let Some(b) = body_b64 {
                            r = r.flag_value("body-b64", b.clone());
                        }
                        r
                    }
                    NetSub::Abort { id, error } => {
                        let r = Request::new("vs_net_abort").arg(id.to_string());
                        match error {
                            Some(e) => r.flag_value("error", e.clone()),
                            None => r,
                        }
                    }
                    NetSub::Fetch { id, o } => {
                        o.apply(Request::new("vs_net_fetch").arg(id.to_string()))
                    }
                    NetSub::Body { id } => Request::new("vs_net_body").arg(id.to_string()),
                    NetSub::Dialog { id, action, text } => {
                        let r = Request::new("vs_net_dialog")
                            .arg(id.to_string())
                            .arg(action.clone());
                        match text {
                            Some(t) => r.flag_value("text", t.clone()),
                            None => r,
                        }
                    }
                };
                r.flag_value("session", s)
            }
            Self::Record { sub } => {
                let s = require_session(session_id)?;
                match sub {
                    RecordSub::Start {
                        page,
                        fps,
                        width,
                        retina,
                    } => {
                        let mut r = Request::new("vs_record")
                            .arg("start")
                            .arg(page.clone())
                            .flag_value("session", s)
                            .flag_value("fps", fps.to_string());
                        if let Some(w) = width {
                            r = r.flag_value("width", w.to_string());
                        }
                        if *retina {
                            r = r.flag("retina");
                        }
                        r
                    }
                    RecordSub::Stop { page } => Request::new("vs_record")
                        .arg("stop")
                        .arg(page.clone())
                        .flag_value("session", s),
                }
            }
            Self::Serve { .. } => {
                anyhow::bail!("vs_serve is local; route via main, not the wire dispatcher");
            }
            Self::Socket => {
                anyhow::bail!("vs socket is local; route via main, not the wire dispatcher");
            }
            Self::Flow { .. } => {
                anyhow::bail!("vs_flow is local; route via main, not the wire dispatcher");
            }
            Self::Mcp => {
                anyhow::bail!("vs_mcp is local; route via main, not the wire dispatcher");
            }
        })
    }

    /// True if this subcommand requires an active session.
    #[must_use]
    pub fn needs_session(&self) -> bool {
        !matches!(
            self,
            Self::SessionOpen { .. }
                | Self::Status
                | Self::Serve { .. }
                | Self::Socket
                | Self::Mcp
                | Self::Flow { .. }
                | Self::Pending { .. }
                | Self::Frame { .. }
        )
    }
}

fn require_session(session: Option<&str>) -> Result<String> {
    session
        .map(str::to_string)
        .context("no active session — run `vs session-open` or pass `--session=<id>`")
}

/// Map short-form inspect kind aliases to their long form. Unknown
/// inputs pass through unchanged so the wire-side parser can reject
/// or accept them — the CLI does not gatekeep here. The two-letter
/// short forms are unambiguous within the inspect subcommand set.
fn normalize_inspect_kind(kind: &str) -> &str {
    match kind {
        "co" => "console",
        "n" => "network",
        "req" => "request",
        "e" => "eval",
        "s" => "storage",
        "scr" => "scripts",
        "src" => "script",
        "res" => "response",
        "d" => "dom",
        "p" => "performance",
        "ce" => "cookie-events",
        other => other,
    }
}

mod dispatch;
mod render;

pub use dispatch::{connect, resolve_paths, resolve_session, run};
pub use render::render;
