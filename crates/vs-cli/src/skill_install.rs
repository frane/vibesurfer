//! `vs skill install` — install vibesurfer into every detected agent.
//!
//! Each agent is targeted on two surfaces (when supported):
//!
//!   1. **Skill** — SKILL.md (or GEMINI.md for Gemini) at the agent's
//!      conventional skills location, plus any sibling manifest the
//!      agent expects (Gemini's `gemini-extension.json`).
//!   2. **MCP** — an `mcpServers.vibesurfer = {command: "vs",
//!      args: ["mcp"]}` entry in the agent's MCP config file. Most
//!      agents share a JSON shape; Codex stores `[mcp_servers.<name>]`
//!      as TOML, hand-rolled here so we don't pull a TOML crate.
//!
//! Detection: an agent is considered installed when its config dir
//! exists or its CLI is on PATH. The canonical `~/.agents/` target
//! is always written. Per-agent failures don't abort — the run
//! reports each result and exits non-zero only if no agent at all
//! could be reached.
//!
//! Mirror of the `agented::internal::agents` shape. Keep this file
//! the single source of truth for vibesurfer agent integrations:
//! adding a new agent is one entry in `agents()`, not three patches
//! across the binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};
use serde_json::{json, Map, Value};

const SERVER_NAME: &str = "vibesurfer";
const SKILL_MD: &str = include_str!("../SKILL.md");

// ============================================================================
// Agent catalog
// ============================================================================

struct Agent {
    name: &'static str,
    /// True for the canonical `~/.agents/` target — always written
    /// even when not "detected" (it's the cross-client convention,
    /// not an installed binary).
    always_write: bool,
    detect: fn() -> bool,
    /// Where the SKILL.md (or GEMINI.md) goes. None if the agent
    /// has no skill surface.
    skill_path: fn(home: &Path) -> Option<PathBuf>,
    /// Optional sibling-manifest writer (Gemini extension manifest).
    /// Called after the skill file is written.
    skill_post: Option<fn(skill_path: &Path) -> Result<()>>,
    /// Where the MCP config file lives. None if the agent has no MCP
    /// surface.
    mcp_path: fn(home: &Path) -> Option<PathBuf>,
    /// Apply pattern: `Json` (mcpServers map) or `Toml` (Codex's
    /// `[mcp_servers.<name>]` sections).
    mcp_format: McpFormat,
}

#[derive(Copy, Clone)]
enum McpFormat {
    None,
    Json,
    Toml,
}

fn agents() -> Vec<Agent> {
    vec![
        // Canonical cross-client convention. Always write.
        Agent {
            name: "agents",
            always_write: true,
            detect: || false,
            skill_path: |h| Some(h.join(".agents/skills/vibesurfer/SKILL.md")),
            skill_post: None,
            mcp_path: |_| None,
            mcp_format: McpFormat::None,
        },
        Agent {
            name: "claude",
            always_write: false,
            detect: || dir_exists(".claude") || file_exists(".claude.json") || on_path("claude"),
            skill_path: |h| Some(h.join(".claude/skills/vibesurfer/SKILL.md")),
            skill_post: None,
            mcp_path: |h| Some(h.join(".claude.json")),
            mcp_format: McpFormat::Json,
        },
        Agent {
            name: "claude-desktop",
            always_write: false,
            detect: || claude_desktop_dir_exists(),
            skill_path: |_| None,
            skill_post: None,
            mcp_path: |h| Some(claude_desktop_config_path(h)),
            mcp_format: McpFormat::Json,
        },
        Agent {
            name: "codex",
            always_write: false,
            detect: || dir_exists(".codex") || on_path("codex"),
            skill_path: |h| Some(h.join(".codex/skills/vibesurfer/SKILL.md")),
            skill_post: None,
            mcp_path: |h| Some(h.join(".codex/config.toml")),
            mcp_format: McpFormat::Toml,
        },
        Agent {
            name: "cursor",
            always_write: false,
            detect: || project_dir_exists(".cursor") || on_path("cursor"),
            // Cursor is project-scoped — write into ./.cursor of the
            // current working dir, not $HOME.
            skill_path: |_| {
                std::env::current_dir()
                    .ok()
                    .map(|cwd| cwd.join(".cursor/skills/vibesurfer/SKILL.md"))
            },
            skill_post: None,
            mcp_path: |_| {
                std::env::current_dir()
                    .ok()
                    .map(|cwd| cwd.join(".cursor/mcp.json"))
            },
            mcp_format: McpFormat::Json,
        },
        Agent {
            name: "gemini",
            always_write: false,
            detect: || dir_exists(".gemini") || on_path("gemini"),
            // Gemini reads agent context from GEMINI.md inside an
            // extension dir, not SKILL.md.
            skill_path: |h| Some(h.join(".gemini/extensions/vibesurfer/GEMINI.md")),
            skill_post: Some(write_gemini_manifest),
            mcp_path: |h| Some(h.join(".gemini/settings.json")),
            mcp_format: McpFormat::Json,
        },
        Agent {
            name: "openclaw",
            always_write: false,
            detect: || dir_exists(".openclaw") || on_path("openclaw"),
            skill_path: |h| Some(h.join(".openclaw/workspace/skills/vibesurfer/SKILL.md")),
            skill_post: None,
            mcp_path: |_| None,
            mcp_format: McpFormat::None,
        },
    ]
}

// ============================================================================
// Public entry point
// ============================================================================

pub fn run() -> Result<()> {
    let home = home_dir().context("could not resolve $HOME")?;
    let agents = agents();
    let mut wrote_skill = 0usize;
    let mut wrote_mcp = 0usize;
    let mut detected = 0usize;
    let mut failures = Vec::new();

    for agent in &agents {
        let active = agent.always_write || (agent.detect)();
        if !active {
            println!("  - {:<14}  skipped (not installed)", agent.name);
            continue;
        }
        detected += 1;
        let mut lines = Vec::new();

        if let Some(path) = (agent.skill_path)(&home) {
            match write_skill(&path) {
                Ok(()) => {
                    lines.push(format!("skill → {}", path.display()));
                    if let Some(post) = agent.skill_post {
                        if let Err(e) = post(&path) {
                            failures.push(format!("{}: post-install: {e:#}", agent.name));
                        }
                    }
                    wrote_skill += 1;
                }
                Err(e) => failures.push(format!("{}: skill: {e:#}", agent.name)),
            }
        }

        if let Some(path) = (agent.mcp_path)(&home) {
            let result = match agent.mcp_format {
                McpFormat::None => Ok(false),
                McpFormat::Json => apply_json(&path, SERVER_NAME, mcp_server_value()),
                McpFormat::Toml => apply_toml(&path, SERVER_NAME, "vs", &["mcp"]),
            };
            match result {
                Ok(true) => {
                    lines.push(format!("mcp   → {}", path.display()));
                    wrote_mcp += 1;
                }
                Ok(false) => {} // already up-to-date
                Err(e) => failures.push(format!("{}: mcp: {e:#}", agent.name)),
            }
        }

        if lines.is_empty() {
            println!("  · {:<14}  (already up to date)", agent.name);
        } else {
            for (i, line) in lines.iter().enumerate() {
                let mark = if i == 0 { "✓" } else { " " };
                let label = if i == 0 { agent.name } else { "" };
                println!("  {mark} {label:<14}  {line}");
            }
        }
    }

    println!(
        "{wrote_skill} skill files, {wrote_mcp} MCP entries written across {detected} detected agents."
    );
    for f in &failures {
        eprintln!("  ! {f}");
    }
    if detected == 0 {
        anyhow::bail!("no agent surfaces found; install one (Claude, Codex, Cursor, Gemini, OpenClaw) and retry");
    }
    if !failures.is_empty() {
        anyhow::bail!("{} target(s) failed; see above", failures.len());
    }
    Ok(())
}

// ============================================================================
// Skill helpers
// ============================================================================

fn write_skill(path: &Path) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for {}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    std::fs::write(path, SKILL_MD).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn write_gemini_manifest(skill_path: &Path) -> Result<()> {
    let dir = skill_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for {}", skill_path.display()))?;
    let manifest = dir.join("gemini-extension.json");
    let body = format!(
        r#"{{
  "name": "vibesurfer",
  "version": "{ver}",
  "contextFileName": "{ctx}"
}}
"#,
        ver = env!("CARGO_PKG_VERSION"),
        ctx = skill_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("GEMINI.md"),
    );
    std::fs::write(&manifest, body).with_context(|| format!("write {}", manifest.display()))?;
    Ok(())
}

// ============================================================================
// MCP — JSON apply (Claude Code / Desktop, Cursor, Gemini)
// ============================================================================

fn mcp_server_value() -> Value {
    json!({
        "command": "vs",
        "args": ["mcp"],
    })
}

fn apply_json(path: &Path, name: &str, server: Value) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut root: Value = if path.exists() {
        let s = std::fs::read_to_string(path)?;
        if s.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        json!({})
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object at root", path.display()))?;
    let mcp = root_obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let mcp_obj = mcp
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mcpServers in {} is not a JSON object", path.display()))?;
    if mcp_obj.get(name) == Some(&server) {
        return Ok(false);
    }
    mcp_obj.insert(name.to_string(), server);
    let pretty = serde_json::to_string_pretty(&root)?;
    std::fs::write(path, format!("{pretty}\n"))?;
    Ok(true)
}

// ============================================================================
// MCP — TOML apply (Codex)
//
// Hand-rolled section editor for `[mcp_servers.<name>]`. Codex's TOML
// is shallow enough that we can do this without pulling a parser.
// ============================================================================

fn apply_toml(path: &Path, name: &str, command: &str, args: &[&str]) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let header = format!("[mcp_servers.{name}]");
    let new_section = render_toml_section(&header, command, args);
    let mut updated = String::new();
    let mut replaced = false;
    let mut skip_until_next_header = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // New section starts; if we were skipping the old one,
            // stop skipping now.
            skip_until_next_header = false;
            if line.trim() == header {
                // Replace this section.
                updated.push_str(&new_section);
                replaced = true;
                skip_until_next_header = true;
                continue;
            }
        }
        if skip_until_next_header {
            continue;
        }
        updated.push_str(line);
        updated.push('\n');
    }
    if !replaced {
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        if !updated.is_empty() {
            updated.push('\n');
        }
        updated.push_str(&new_section);
    }
    if updated == body {
        return Ok(false);
    }
    std::fs::write(path, updated)?;
    Ok(true)
}

fn render_toml_section(header: &str, command: &str, args: &[&str]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    let _ = writeln!(out, "command = {}", toml_string(command));
    if !args.is_empty() {
        out.push_str("args = [");
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&toml_string(a));
        }
        out.push_str("]\n");
    }
    out
}

fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ============================================================================
// Detection
// ============================================================================

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn dir_exists(rel: &str) -> bool {
    home_dir().is_some_and(|h| h.join(rel).is_dir())
}

fn file_exists(rel: &str) -> bool {
    home_dir().is_some_and(|h| h.join(rel).is_file())
}

fn project_dir_exists(rel: &str) -> bool {
    std::env::current_dir()
        .ok()
        .is_some_and(|cwd| cwd.join(rel).is_dir())
}

fn on_path(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

#[cfg(target_os = "macos")]
fn claude_desktop_config_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Claude/claude_desktop_config.json")
}

#[cfg(target_os = "linux")]
fn claude_desktop_config_path(home: &Path) -> PathBuf {
    home.join(".config/Claude/claude_desktop_config.json")
}

#[cfg(target_os = "windows")]
fn claude_desktop_config_path(home: &Path) -> PathBuf {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    if let Some(p) = appdata {
        return p.join("Claude/claude_desktop_config.json");
    }
    home.join("AppData/Roaming/Claude/claude_desktop_config.json")
}

fn claude_desktop_dir_exists() -> bool {
    home_dir().is_some_and(|h| {
        claude_desktop_config_path(&h)
            .parent()
            .is_some_and(Path::is_dir)
    })
}
