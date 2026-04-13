use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "plugit", about = "Claude Code plugin lifecycle tool")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Stamp plugin.json with HEAD commit hash and commit if changed
    Stamp,
    /// Reinstall the plugin via the claude CLI
    Install {
        /// Marketplace name to install from (default: plugin name from manifest)
        #[arg(long)]
        marketplace: Option<String>,
    },
    /// Full pre-push workflow: stamp + detect changes + conditional reinstall
    Push {
        /// Marketplace name to install from (default: plugin name from manifest)
        #[arg(long)]
        marketplace: Option<String>,
        /// Directories that count as plugin sources, colon-separated
        #[arg(long, default_value = ".claude-plugin/:skills/:agents/:hooks/")]
        watch: String,
    },
}

// ---------------------------------------------------------------------------
// Plugin manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    name: String,
    version: String,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk up from cwd looking for `.claude-plugin/plugin.json`.
fn find_manifest() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().context("cwd")?;
    loop {
        let candidate = dir.join(".claude-plugin").join("plugin.json");
        if candidate.exists() {
            return Ok(candidate);
        }
        if !dir.pop() {
            bail!("no .claude-plugin/plugin.json found walking up from cwd");
        }
    }
}

/// Repo root = directory containing the manifest's `.claude-plugin/`.
fn repo_root(manifest: &Path) -> PathBuf {
    manifest
        .parent() // .claude-plugin/
        .and_then(|p| p.parent()) // repo root
        .unwrap_or(manifest)
        .to_path_buf()
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .context("running git")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn head_hash(repo: &Path) -> Result<String> {
    let h = git(repo, &["rev-parse", "--short", "HEAD"])?;
    if h.is_empty() {
        bail!("git rev-parse --short HEAD returned nothing");
    }
    Ok(h)
}

// ---------------------------------------------------------------------------
// stamp
// ---------------------------------------------------------------------------

fn stamp(manifest_path: &Path) -> Result<String> {
    let repo = repo_root(manifest_path);
    let hash = head_hash(&repo)?;

    let raw = std::fs::read_to_string(manifest_path).context("reading manifest")?;
    let mut manifest: Manifest = serde_json::from_str(&raw).context("parsing manifest")?;

    if manifest.version == hash {
        println!("[plugit] version already {hash} — nothing to stamp");
        return Ok(hash);
    }

    manifest.version = hash.clone();
    let serialized = serde_json::to_string_pretty(&manifest)? + "\n";
    std::fs::write(manifest_path, &serialized).context("writing manifest")?;

    // Stage and commit
    let rel = manifest_path
        .strip_prefix(&repo)
        .unwrap_or(manifest_path)
        .to_string_lossy()
        .to_string();
    git(&repo, &["add", &rel])?;
    git(
        &repo,
        &["commit", "-m", &format!("chore: set plugin version to {hash}")],
    )?;

    println!("[plugit] version stamped to {hash}");
    Ok(hash)
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

fn install(manifest_path: &Path, marketplace: Option<&str>) -> Result<()> {
    let repo = repo_root(manifest_path);
    let raw = std::fs::read_to_string(manifest_path).context("reading manifest")?;
    let manifest: Manifest = serde_json::from_str(&raw).context("parsing manifest")?;
    let name = &manifest.name;
    // Default marketplace to the plugin name — the standard `name@name` convention
    let marketplace = marketplace.unwrap_or(name);

    let claude = which_claude()?;

    println!("[plugit] uninstalling {name}...");
    // Ignore errors — plugin may not be installed yet
    let _ = Command::new(&claude)
        .current_dir(&repo)
        .args(["plugin", "uninstall", name])
        .status();

    println!("[plugit] installing {name}@{marketplace}...");
    let status = Command::new(&claude)
        .current_dir(&repo)
        .args(["plugin", "install", &format!("{name}@{marketplace}")])
        .status()
        .context("running claude plugin install")?;

    if !status.success() {
        bail!("claude plugin install exited non-zero");
    }

    println!("[plugit] {name} installed — restart Claude Code to apply");
    Ok(())
}

fn which_claude() -> Result<String> {
    let out = Command::new("which").arg("claude").output();
    if let Ok(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    bail!("'claude' not found on PATH — install Claude Code first");
}

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

fn push(manifest_path: &Path, marketplace: Option<&str>, watch_dirs: &[&str]) -> Result<()> {
    let repo = repo_root(manifest_path);

    // 1. Stamp
    stamp(manifest_path)?;

    // 2. Detect changed plugin files since remote tip
    let remote_before = git(&repo, &["rev-parse", "@{u}"]).unwrap_or_default();
    let base = if remote_before.is_empty() {
        "HEAD~1".to_string()
    } else {
        remote_before
    };

    let diff = git(&repo, &["diff", "--name-only", &base, "HEAD"])?;
    let changed: Vec<&str> = diff
        .lines()
        .filter(|line| watch_dirs.iter().any(|dir| line.starts_with(dir)))
        .collect();

    if changed.is_empty() {
        println!("[plugit] no plugin sources changed — skipping reinstall");
        return Ok(());
    }

    println!("[plugit] plugin sources changed:");
    for f in &changed {
        println!("  {f}");
    }

    // 3. Reinstall
    install(manifest_path, marketplace)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    let manifest = find_manifest()?;

    match cli.command {
        Cmd::Stamp => {
            stamp(&manifest)?;
        }
        Cmd::Install { marketplace } => {
            install(&manifest, marketplace.as_deref())?;
        }
        Cmd::Push { marketplace, watch } => {
            let dirs: Vec<&str> = watch.split(':').collect();
            push(&manifest, marketplace.as_deref(), &dirs)?;
        }
    }

    Ok(())
}
