//! Declarative flow runner (`vs flow run <file>`).
//!
//! A flow file is a JSON array of steps; each step is an array of `vs`
//! arguments (exactly what you would type after `vs`). Steps run in
//! order inside one session, reusing the normal command dispatch, so a
//! flow is just a scripted sequence of primitives with two conveniences:
//!
//! - `$page` in any argument expands to the id of the last page a step
//!   opened or navigated (`open` / `goto`).
//! - `$token` expands to that page's current state token, fetched with a
//!   `vs_view` right before the step, so `act` steps do not have to
//!   thread tokens by hand.
//!
//! The runner stops at the first step that returns an error envelope.

use std::path::Path;

use anyhow::{bail, Context as _, Result};
use clap::Parser as _;
use vs_protocol::Envelope;

use crate::client::Response;
use crate::commands::{run, Cli, Command, FlowSub};

/// Entry point for the local `Flow` command (routed from `main`, like
/// `serve` / `mcp`).
pub fn run_flow(cli: &Cli) -> Result<()> {
    let Command::Flow { sub } = &cli.command else {
        bail!("run_flow called with a non-Flow command");
    };
    match sub {
        FlowSub::Run { file } => run_file(cli, file),
    }
}

fn run_file(outer: &Cli, file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("read flow file {}", file.display()))?;
    let steps: Vec<Vec<String>> = serde_json::from_str(&text)
        .context("flow file must be a JSON array of arg arrays, e.g. [[\"open\",\"https://x\"]]")?;

    // One session for the whole flow: honor an explicit session, else
    // open a fresh one and thread it through every step.
    let session = match &outer.session {
        Some(s) => s.clone(),
        None => open_session(outer)?,
    };

    let mut page: Option<String> = None;
    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        if step.is_empty() {
            continue;
        }
        // Resolve $token first (needs a view), then $page.
        let token = if step.iter().any(|a| a.contains("$token")) {
            Some(current_token(outer, &session, page.as_deref(), n)?)
        } else {
            None
        };
        let args: Vec<String> = step
            .iter()
            .map(|a| substitute(a, page.as_deref(), token.as_deref()))
            .collect();

        let resp = run_step(outer, &session, &args)
            .with_context(|| format!("step {n}: {}", step.join(" ")))?;

        // Capture the page id from open/goto for later $page expansion.
        if matches!(step[0].as_str(), "open" | "o" | "goto" | "g") {
            if let Some(p) = resp.body.first().map(|s| s.trim().to_string()) {
                if p.starts_with("p_") {
                    page = Some(p);
                }
            }
        }

        match &resp.envelope {
            Envelope::Success(_) => {
                eprintln!("flow: step {n} ok: {}", step.join(" "));
            }
            Envelope::Error { code, args: eargs } => {
                eprintln!(
                    "flow: step {n} FAILED: {} -> ! {code} {}",
                    step.join(" "),
                    eargs.join(" ")
                );
                bail!("flow stopped at step {n} ({code})");
            }
        }
    }
    eprintln!("flow: {} step(s) ok", steps.len());
    Ok(())
}

/// Substitute `$page` / `$token` tokens in one argument. Whole-word or
/// embedded (e.g. `--token=$token`) both work.
fn substitute(arg: &str, page: Option<&str>, token: Option<&str>) -> String {
    let mut out = arg.to_string();
    if let Some(p) = page {
        out = out.replace("$page", p);
    }
    if let Some(t) = token {
        out = out.replace("$token", t);
    }
    out
}

/// Build a `Cli` for one step and dispatch it, forcing the flow's
/// session and carrying the outer global flags (socket/home/no_spawn).
fn run_step(outer: &Cli, session: &str, step_args: &[String]) -> Result<Response> {
    let mut cmdline = vec!["vs".to_string()];
    cmdline.push("--session".to_string());
    cmdline.push(session.to_string());
    push_globals(outer, &mut cmdline);
    cmdline.extend(step_args.iter().cloned());
    let step_cli =
        Cli::try_parse_from(&cmdline).with_context(|| format!("parse step {step_args:?}"))?;
    run(&step_cli)
}

/// Open a fresh session and return its id.
fn open_session(outer: &Cli) -> Result<String> {
    let mut argv = vec!["vs".to_string(), "session-open".to_string()];
    push_globals(outer, &mut argv);
    let cli = Cli::try_parse_from(&argv)?;
    let resp = run(&cli)?;
    resp.body
        .first()
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("s_"))
        .context("flow: session-open returned no session id")
}

/// View the current page and return its state token for `$token`.
fn current_token(outer: &Cli, session: &str, page: Option<&str>, step: usize) -> Result<String> {
    let page = page.with_context(|| {
        format!("step {step}: $token needs a page, but no open/goto has run yet")
    })?;
    let resp = run_step(outer, session, &["view".to_string(), page.to_string()])?;
    match resp.envelope {
        Envelope::Success(t) => Ok(t.to_string()),
        Envelope::Error { code, .. } => bail!("step {step}: view for $token failed ({code})"),
    }
}

/// Carry the global flags that affect daemon addressing onto a step's
/// argv. `--session` is set separately by the caller.
fn push_globals(outer: &Cli, argv: &mut Vec<String>) {
    if let Some(sock) = &outer.socket {
        argv.push("--socket".to_string());
        argv.push(sock.display().to_string());
    }
    if let Some(home) = &outer.home {
        argv.push("--home".to_string());
        argv.push(home.display().to_string());
    }
    if outer.no_spawn {
        argv.push("--no-spawn".to_string());
    }
}
