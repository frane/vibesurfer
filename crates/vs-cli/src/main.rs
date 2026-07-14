//! `vs` — the vibesurfer CLI.
//!
//! One binary, two modes:
//!
//! - default: parse args, send a request to the daemon, print the response.
//! - `vs serve`: host the daemon in this process. Auto-spawn re-execs
//!   `vs serve` when the socket is missing.
//!
//! Exit codes per `docs/PROTOCOL.md`:
//! - `0` success envelope (`@`)
//! - `1` error envelope (`!`) or local error
//! - `2` warnings + success

#![cfg_attr(not(target_os = "macos"), forbid(unsafe_code))]
#![cfg_attr(target_os = "macos", allow(unsafe_code))]

use clap::Parser as _;
use vs_cli::commands::{render, run, Cli, Command};
use vs_cli::serve::{self, ServeArgs};
use vs_protocol::Envelope;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    if let Command::Serve { stop } = cli.command {
        let paths = vs_daemon::config::Paths::at(
            cli.home
                .clone()
                .unwrap_or_else(|| vs_cli::paths::Paths::home().root),
        );
        if let Err(e) = serve::run(&ServeArgs { paths, stop }) {
            eprintln!("error: {e:#}");
            return std::process::ExitCode::from(1);
        }
        return std::process::ExitCode::SUCCESS;
    }

    if matches!(cli.command, Command::Mcp) {
        if let Err(e) = vs_cli::mcp::run(&cli) {
            eprintln!("error: {e:#}");
            return std::process::ExitCode::from(1);
        }
        return std::process::ExitCode::SUCCESS;
    }

    if let Command::Skill { sub, .. } = &cli.command {
        if sub.as_deref() == Some("install") {
            if let Err(e) = vs_cli::skill_install::run() {
                eprintln!("error: {e:#}");
                return std::process::ExitCode::from(1);
            }
            return std::process::ExitCode::SUCCESS;
        }
    }

    let resp = match run(&cli) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e:#}");
            return std::process::ExitCode::from(1);
        }
    };
    print!("{}", render(&resp, cli.json));
    let exit = match resp.envelope {
        Envelope::Success(_) if !resp.warnings.is_empty() => 2,
        Envelope::Success(_) => 0,
        Envelope::Error { .. } => 1,
    };
    std::process::ExitCode::from(exit)
}
