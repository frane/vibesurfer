//! PR1 acceptance: short-form CLI aliases produce the same wire
//! `Request` as the long form. One test per primitive (20), one per
//! `inspect` subcommand short kind (9), plus the `inspect` top-level
//! alias itself with a long kind (1). Total: 30.

use clap::Parser as _;
use vs_cli::commands::Cli;
use vs_protocol::Request;

/// Parse argv as the CLI and build the wire request, given a fixed
/// session id so every subcommand that requires one gets the same
/// resolved value.
fn req_for(args: &[&str]) -> Request {
    let mut full = vec!["vs"];
    full.extend(args);
    let cli = Cli::try_parse_from(&full).expect("parse");
    cli.command
        .to_request(Some("s_test"))
        .expect("build request")
}

/// The contract: long form and short form must serialize to the exact
/// same `Request`. `Request` already derives `PartialEq` over
/// primitive name, args (ordered), and flags (BTreeMap), so this is
/// the right equivalence.
fn assert_parity(long_args: &[&str], short_args: &[&str], label: &str) {
    let l = req_for(long_args);
    let s = req_for(short_args);
    assert_eq!(
        l, s,
        "{label}: long form {long_args:?} and short form {short_args:?} must produce identical wire requests"
    );
}

// --- 20 primitive aliases ---

#[test]
fn alias_session_open() {
    assert_parity(&["session-open"], &["so"], "session-open/so");
}

#[test]
fn alias_session_close() {
    assert_parity(&["session-close"], &["sc"], "session-close/sc");
}

#[test]
fn alias_open() {
    assert_parity(
        &["open", "https://example.com"],
        &["o", "https://example.com"],
        "open/o",
    );
}

#[test]
fn alias_close() {
    assert_parity(&["close", "p_1"], &["c", "p_1"], "close/c");
}

#[test]
fn alias_view() {
    assert_parity(&["view", "p_1"], &["v", "p_1"], "view/v");
}

#[test]
fn alias_read() {
    assert_parity(&["read", "p_1", "7"], &["r", "p_1", "7"], "read/r");
}

#[test]
fn alias_act() {
    assert_parity(
        &["act", "p_1", "3", "click", "--token=abc"],
        &["a", "p_1", "3", "click", "--token=abc"],
        "act/a",
    );
}

#[test]
fn alias_find() {
    assert_parity(&["find", "needle"], &["f", "needle"], "find/f");
}

#[test]
fn alias_wait() {
    assert_parity(
        &["wait", "p_1", "stable"],
        &["w", "p_1", "stable"],
        "wait/w",
    );
}

#[test]
fn alias_extract() {
    assert_parity(
        &["extract", "p_1", "table", "--token=abc"],
        &["x", "p_1", "table", "--token=abc"],
        "extract/x",
    );
}

#[test]
fn alias_mark() {
    assert_parity(
        &["mark", "p_1", "7", "anchor", "--token=abc"],
        &["m", "p_1", "7", "anchor", "--token=abc"],
        "mark/m",
    );
}

#[test]
fn alias_annotate() {
    assert_parity(
        &["annotate", "ref:5", "note", "hello"],
        &["an", "ref:5", "note", "hello"],
        "annotate/an",
    );
}

#[test]
fn alias_status() {
    assert_parity(&["status"], &["st"], "status/st");
}

#[test]
fn alias_log() {
    assert_parity(&["log"], &["l"], "log/l");
}

#[test]
fn alias_skill() {
    assert_parity(&["skill"], &["sk"], "skill/sk");
}

#[test]
fn alias_capture() {
    assert_parity(&["capture", "p_1"], &["cap", "p_1"], "capture/cap");
}

#[test]
fn alias_viewport() {
    assert_parity(
        &["viewport", "p_1", "mobile"],
        &["vp", "p_1", "mobile"],
        "viewport/vp",
    );
}

#[test]
fn alias_layout() {
    assert_parity(
        &["layout", "p_1", "3", "4"],
        &["lay", "p_1", "3", "4"],
        "layout/lay",
    );
}

#[test]
fn alias_auth() {
    assert_parity(&["auth", "list"], &["au", "list"], "auth/au");
}

#[test]
fn alias_inspect() {
    // The 20th primitive — `inspect` ↔ `i` at the command level,
    // long kind (`console`) so this isolates the command alias from
    // the kind alias.
    assert_parity(
        &["inspect", "p_1", "console"],
        &["i", "p_1", "console"],
        "inspect/i (long kind)",
    );
}

// --- 9 inspect kind short forms ---
//
// CLI normalizes each short kind to its long form before building
// the request; both rows therefore serialize identically. PR 2 will
// add wire-side acceptance of the short kind too.

#[test]
fn alias_inspect_kind_console() {
    assert_parity(
        &["inspect", "p_1", "console"],
        &["i", "p_1", "co"],
        "inspect console / i co",
    );
}

#[test]
fn alias_inspect_kind_network() {
    assert_parity(
        &["inspect", "p_1", "network"],
        &["i", "p_1", "n"],
        "inspect network / i n",
    );
}

#[test]
fn alias_inspect_kind_request() {
    assert_parity(
        &["inspect", "p_1", "request", "42"],
        &["i", "p_1", "req", "42"],
        "inspect request / i req",
    );
}

#[test]
fn alias_inspect_kind_eval() {
    assert_parity(
        &["inspect", "p_1", "eval", "1+1"],
        &["i", "p_1", "e", "1+1"],
        "inspect eval / i e",
    );
}

#[test]
fn alias_inspect_kind_storage() {
    assert_parity(
        &["inspect", "p_1", "storage", "cookies"],
        &["i", "p_1", "s", "cookies"],
        "inspect storage / i s",
    );
}

#[test]
fn alias_inspect_kind_scripts() {
    assert_parity(
        &["inspect", "p_1", "scripts"],
        &["i", "p_1", "scr"],
        "inspect scripts / i scr",
    );
}

#[test]
fn alias_inspect_kind_script() {
    assert_parity(
        &["inspect", "p_1", "script", "0"],
        &["i", "p_1", "src", "0"],
        "inspect script / i src",
    );
}

#[test]
fn alias_inspect_kind_dom() {
    assert_parity(
        &["inspect", "p_1", "dom"],
        &["i", "p_1", "d"],
        "inspect dom / i d",
    );
}

#[test]
fn alias_inspect_kind_performance() {
    assert_parity(
        &["inspect", "p_1", "performance"],
        &["i", "p_1", "p"],
        "inspect performance / i p",
    );
}

// --- Frequent-flag short forms ---
//
// Not in the 30-test budget but still part of the PR 1 contract:
// short flags resolve to the same Request as the long flags.

#[test]
fn flag_full_short_on_view() {
    assert_parity(
        &["view", "p_1", "--full"],
        &["v", "p_1", "-F"],
        "view --full / -F",
    );
}

#[test]
fn flag_full_short_on_inspect() {
    assert_parity(
        &["inspect", "p_1", "console", "--full"],
        &["i", "p_1", "co", "-F"],
        "inspect --full / -F",
    );
}

#[test]
fn flag_since_short_on_log() {
    assert_parity(
        &["log", "--since=42"],
        &["l", "-s", "42"],
        "log --since / -s",
    );
}

#[test]
fn flag_limit_short_on_log() {
    assert_parity(
        &["log", "--limit=10"],
        &["l", "-n", "10"],
        "log --limit / -n",
    );
}

#[test]
fn flag_page_short_on_log() {
    assert_parity(
        &["log", "--page=p_1"],
        &["l", "-P", "p_1"],
        "log --page / -P",
    );
}

#[test]
fn flag_session_short_global() {
    // The --session flag is global; both forms must produce the same
    // request body. We pass it explicitly here because the helper's
    // resolved session_id is also "s_test" — that double-bind is
    // intentional, it asserts that `-S` parses cleanly.
    let l = req_for(&["--session=s_test", "view", "p_1"]);
    let s = req_for(&["-S", "s_test", "view", "p_1"]);
    assert_eq!(l, s, "--session / -S parity");
}
