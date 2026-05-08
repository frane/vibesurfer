//! `vs skill install` — write the bundled `vibesurfer` SKILL.md into
//! every detected agent's skills directory.
//!
//! Modeled after `ae skill install`. Agent surfaces evolve; this list
//! is the union of canonical conventions as of M6:
//!
//!   ~/.claude/skills/vibesurfer/SKILL.md      Claude Code / Desktop
//!   ~/.config/codex/skills/vibesurfer/SKILL.md Codex CLI
//!   ~/.cursor/skills/vibesurfer/SKILL.md      Cursor
//!   ~/.openclaw/skills/vibesurfer/SKILL.md    OpenClaw
//!   ~/.smithery/skills/vibesurfer/SKILL.md    Smithery
//!   ~/.agents/skills/vibesurfer/SKILL.md      canonical fallback
//!
//! For each candidate: write only if the parent agent dir already
//! exists (i.e. the agent is installed on this machine), or if it's
//! the canonical `~/.agents/` path. Don't fail if any individual
//! write fails — print the result per dir, exit 0 unless every write
//! failed.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// The bundled SKILL.md content. Compiled in at build time so the
/// installed `vs` binary doesn't need access to the source tree.
const SKILL_MD: &str = include_str!("../../../skills/vibesurfer/SKILL.md");

struct Target {
    label: &'static str,
    /// Path to the agent's root dir (parent of the `skills/` subdir).
    agent_root: PathBuf,
    /// Subdir name under the agent root that holds skills.
    skills_subdir: &'static str,
    /// True if this dir should be created when missing (e.g. canonical
    /// `~/.agents/` should always exist after install).
    create_if_missing: bool,
}

pub fn run() -> Result<()> {
    let home = home_dir().context("could not resolve $HOME")?;
    let targets = vec![
        Target {
            label: "Claude",
            agent_root: home.join(".claude"),
            skills_subdir: "skills",
            create_if_missing: false,
        },
        Target {
            label: "Codex",
            agent_root: home.join(".config").join("codex"),
            skills_subdir: "skills",
            create_if_missing: false,
        },
        Target {
            label: "Cursor",
            agent_root: home.join(".cursor"),
            skills_subdir: "skills",
            create_if_missing: false,
        },
        Target {
            label: "OpenClaw",
            agent_root: home.join(".openclaw"),
            skills_subdir: "skills",
            create_if_missing: false,
        },
        Target {
            label: "Smithery",
            agent_root: home.join(".smithery"),
            skills_subdir: "skills",
            create_if_missing: false,
        },
        Target {
            label: "Canonical (~/.agents)",
            agent_root: home.join(".agents"),
            skills_subdir: "skills",
            create_if_missing: true,
        },
    ];

    let mut wrote = 0usize;
    let mut skipped = 0usize;
    for t in &targets {
        let exists = t.agent_root.is_dir();
        if !exists && !t.create_if_missing {
            println!("  - {:<24}  skipped (not installed)", t.label);
            skipped += 1;
            continue;
        }
        let dir = t.agent_root.join(t.skills_subdir).join("vibesurfer");
        let skill_path = dir.join("SKILL.md");
        match write_skill(&dir, &skill_path) {
            Ok(()) => {
                println!("  ✓ {:<24}  {}", t.label, skill_path.display());
                wrote += 1;
            }
            Err(e) => {
                eprintln!("  ✗ {:<24}  {}: {e:#}", t.label, skill_path.display());
            }
        }
    }
    println!("{wrote} written, {skipped} skipped (agent not detected).");
    if wrote == 0 {
        anyhow::bail!("no agent surfaces found; install one (Claude, Codex, Cursor, …) and retry");
    }
    Ok(())
}

fn write_skill(dir: &Path, file: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    std::fs::write(file, SKILL_MD).with_context(|| format!("write {}", file.display()))?;
    Ok(())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
