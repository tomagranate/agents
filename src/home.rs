use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{config::Paths, mcp, progress::Activity, settings, util};

const HARNESSES: [&str; 4] = ["claude", "codex", "grok", "opencode"];
const SHARED_TEMPLATE: &str = include_str!("../share/templates/AGENTS.md");
const MCP_TEMPLATE: &str = include_str!("../share/templates/mcp.toml");
const CLAUDE_TEMPLATE: &str = include_str!("../share/templates/harness/claude.md");
const CODEX_TEMPLATE: &str = include_str!("../share/templates/harness/codex.md");
const GROK_TEMPLATE: &str = include_str!("../share/templates/harness/grok.md");
const OPENCODE_TEMPLATE: &str = include_str!("../share/templates/harness/opencode.md");

fn harness_template(name: &str) -> &'static str {
    match name {
        "claude" => CLAUDE_TEMPLATE,
        "codex" => CODEX_TEMPLATE,
        "grok" => GROK_TEMPLATE,
        "opencode" => OPENCODE_TEMPLATE,
        _ => "",
    }
}

fn validate_harness(harness: &str) -> Result<()> {
    if HARNESSES.contains(&harness) {
        Ok(())
    } else {
        bail!("unknown harness: {harness} (claude|codex|grok|opencode)")
    }
}

fn file_nonempty(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|text| text.chars().any(|character| !character.is_whitespace()))
        .unwrap_or(false)
}

fn skill_names(path: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(entries) = fs::read_dir(path) else {
        return names;
    };
    for entry in entries.flatten() {
        let candidate = entry.path();
        let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !candidate.join("SKILL.md").is_file() {
            continue;
        }
        names.insert(name.to_owned());
    }
    names
}

pub fn status(paths: &Paths, offline: bool, verbose: bool) -> Result<()> {
    println!("agents {}", env!("CARGO_PKG_VERSION"));
    if !paths.agents_home.join(".git").is_dir() {
        println!("Agents home: not configured");
        println!("Run: agents init");
        if verbose {
            println!("Expected path: {}", paths.agents_home.display());
        }
        return Ok(());
    }

    let fetch_state = if offline {
        "offline".to_owned()
    } else if !has_origin(&paths.agents_home) {
        "no origin".to_owned()
    } else {
        match fetch_origin(&paths.agents_home, "Checking agents-home remote") {
            Ok(()) => "current remote state fetched".to_owned(),
            Err(error) => {
                eprintln!("warning: could not fetch agents-home: {error:#}");
                "cached remote state".to_owned()
            }
        }
    };

    let branch = git_text(&paths.agents_home, &["branch", "--show-current"])?;
    let dirty = git_dirty(&paths.agents_home)?;
    let (ahead, behind) = upstream(&paths.agents_home)
        .and_then(|upstream| ahead_behind(&paths.agents_home, &upstream).ok())
        .unwrap_or((0, 0));
    println!("Agents home: {}", paths.agents_home.display());
    println!("Local");
    println!(
        "  Branch: {}",
        if branch.is_empty() {
            "detached"
        } else {
            &branch
        }
    );
    println!(
        "  Working tree: {}",
        if dirty { "changed" } else { "clean" }
    );
    println!("  Commits: {ahead} ahead, {behind} behind");
    println!(
        "  Content: {} shared skills, {} harness skills",
        skill_names(&paths.shared_skills).len(),
        HARNESSES
            .iter()
            .map(|name| skill_names(&paths.harness_skills(name)).len())
            .sum::<usize>()
    );
    println!("Remote");
    println!(
        "  Origin: {}",
        origin_url(&paths.agents_home).unwrap_or_else(|| "not configured".to_owned())
    );
    println!("  State: {fetch_state}");
    println!("Harness wiring");
    println!("  Claude: {}", file_state(&paths.claude_md));
    println!("  Codex: {}", file_state(&paths.codex_md));
    println!("  Grok: {}", directory_state(&paths.grok_rules));
    println!("  OpenCode: {}", file_state(&paths.opencode_jsonc));
    println!("Harness settings");
    for harness in HARNESSES {
        println!("  {harness}: {}", settings::status_line(paths, harness)?);
    }

    if verbose {
        println!("Paths");
        println!("  Shared Markdown: {}", paths.shared_md.display());
        println!("  Shared MCP: {}", paths.shared_mcp.display());
        println!("  Shared skills: {}", paths.shared_skills.display());
        println!("  Harness content: {}", paths.harnesses_dir.display());
        println!("  State: {}", paths.state_dir.display());
        println!(
            "  Archive config: {}",
            paths.config_dir.join("archive.toml").display()
        );
    }
    Ok(())
}

fn file_state(path: &Path) -> String {
    if path.is_file() || path.is_symlink() {
        format!("configured ({})", path.display())
    } else {
        "not configured".to_owned()
    }
}

fn directory_state(path: &Path) -> String {
    if path.is_dir() {
        format!("configured ({})", path.display())
    } else {
        "not configured".to_owned()
    }
}

pub fn init(paths: &Paths, force: bool, do_apply: bool) -> Result<()> {
    println!(
        "Scaffolding {} from embedded templates",
        paths.agents_home.display()
    );
    fs::create_dir_all(&paths.shared_skills)?;
    install_template(&paths.shared_md, SHARED_TEMPLATE, force)?;
    install_template(&paths.shared_mcp, MCP_TEMPLATE, force)?;
    for harness in HARNESSES {
        fs::create_dir_all(paths.harness_skills(harness))?;
        install_template(&paths.harness_md(harness), harness_template(harness), force)?;
    }
    settings::initialize(paths)?;
    if !paths.agents_home.join(".git").is_dir() {
        util::command_status("git", ["init", "-b", "master"], Some(&paths.agents_home))?;
        println!("  initialized Git repository");
    }
    if do_apply {
        println!();
        apply(paths)
    } else {
        println!("Done. Run: agents home advanced apply");
        Ok(())
    }
}

pub fn edit(paths: &Paths) -> Result<()> {
    if !paths.agents_home.is_dir() {
        bail!("agents home is not initialized; run agents init")
    }
    util::command_status("zed", [&paths.agents_home], None)
}

fn install_template(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.is_file() && !force {
        println!("  keep {}", path.display());
        return Ok(());
    }
    util::atomic_write(path, contents.as_bytes())?;
    println!("  wrote {}", path.display());
    Ok(())
}

pub fn sync(paths: &Paths, message: &str) -> Result<()> {
    ensure_initialized(paths)?;
    let remote = has_origin(&paths.agents_home);
    if remote {
        fetch_origin(&paths.agents_home, "Fetching agents-home")?;
    }
    commit_home_changes(paths, message)?;
    if remote {
        rebase_upstream(&paths.agents_home)?;
    }
    settings::capture(paths)?;
    commit_home_changes(paths, message)?;
    apply(paths)?;
    if remote {
        push_with_retry(&paths.agents_home)?;
        println!("Pushed agents-home.");
    } else {
        println!("No origin remote is configured. Changes remain local.");
    }
    Ok(())
}

pub fn pull(paths: &Paths) -> Result<()> {
    ensure_initialized(paths)?;
    if !has_origin(&paths.agents_home) {
        bail!("agents-home has no origin remote")
    }
    if git_dirty(&paths.agents_home)? {
        bail!("agents-home has local changes; run agents sync to preserve them")
    }
    fetch_origin(&paths.agents_home, "Fetching agents-home")?;
    rebase_upstream(&paths.agents_home)
}

pub fn push(paths: &Paths) -> Result<()> {
    ensure_initialized(paths)?;
    if git_dirty(&paths.agents_home)? {
        bail!("agents-home has local changes; run agents sync to commit them")
    }
    if !has_origin(&paths.agents_home) {
        bail!("agents-home has no origin remote")
    }
    push_with_retry(&paths.agents_home)
}

pub fn apply(paths: &Paths) -> Result<()> {
    if !paths.shared_md.is_file() {
        bail!("agents home is not initialized; run agents init")
    }
    for harness in HARNESSES {
        if !paths.harness_md(harness).is_file() {
            bail!(
                "missing {}; run agents init",
                paths.harness_md(harness).display()
            )
        }
    }

    println!("Applying agents content to harnesses...");
    for directory in [
        &paths.claude_skills,
        &paths.codex_skills,
        &paths.grok_rules,
        &paths.grok_skills,
        &paths.opencode_dir,
        &paths.opencode_skills,
    ] {
        fs::create_dir_all(directory)?;
    }

    let claude = format!(
        "@{}\n@{}\n",
        paths.shared_md.display(),
        paths.harness_md("claude").display()
    );
    util::atomic_write(&paths.claude_md, claude.as_bytes())?;
    println!("  wrote {}", paths.claude_md.display());

    if paths.codex_md.is_symlink() {
        fs::remove_file(&paths.codex_md)?;
    }
    let codex = render_markdown(paths, "codex")?;
    util::atomic_write(&paths.codex_md, codex.as_bytes())?;
    println!("  wrote {}", paths.codex_md.display());

    replace_symlink(&paths.grok_rules.join("AGENTS.md"), &paths.shared_md)?;
    replace_symlink(
        &paths.grok_rules.join("harness-grok.md"),
        &paths.harness_md("grok"),
    )?;

    settings::apply(paths)?;
    mcp::apply(paths)?;

    for (harness, destination) in [
        ("claude", &paths.claude_skills),
        ("codex", &paths.codex_skills),
        ("grok", &paths.grok_skills),
        ("opencode", &paths.opencode_skills),
    ] {
        sync_harness_skills(paths, harness, destination)?;
    }
    println!("Agents content applied.");
    Ok(())
}

fn render_markdown(paths: &Paths, harness: &str) -> Result<String> {
    validate_harness(harness)?;
    let mut output = format!(
        "<!-- Generated by `agents sync`. Edit {} and {} -->\n\n",
        paths.shared_md.display(),
        paths.harness_md(harness).display()
    );
    output.push_str(&fs::read_to_string(&paths.shared_md)?);
    output.push('\n');
    if file_nonempty(&paths.harness_md(harness)) {
        output.push_str(&fs::read_to_string(paths.harness_md(harness))?);
        output.push('\n');
    }
    Ok(output)
}

#[cfg(unix)]
fn replace_symlink(link: &Path, target: &Path) -> Result<()> {
    if link.is_symlink() || link.exists() {
        if link.is_dir() && !link.is_symlink() {
            bail!("cannot replace directory {}", link.display());
        }
        fs::remove_file(link)?;
    }
    std::os::unix::fs::symlink(target, link)?;
    println!("  linked {}", link.display());
    Ok(())
}

#[cfg(not(unix))]
fn replace_symlink(_link: &Path, _target: &Path) -> Result<()> {
    bail!("agents sync currently requires Unix symlink support")
}

fn effective_skills(paths: &Paths, harness: &str) -> BTreeMap<String, (PathBuf, &'static str)> {
    let mut skills = BTreeMap::new();
    for name in skill_names(&paths.shared_skills) {
        skills.insert(name.clone(), (paths.shared_skills.join(name), "shared"));
    }
    let harness_skills = paths.harness_skills(harness);
    for name in skill_names(&harness_skills) {
        skills.insert(name.clone(), (harness_skills.join(name), "harness"));
    }
    skills
}

fn sync_harness_skills(paths: &Paths, harness: &str, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    let effective = effective_skills(paths, harness);
    let expected = effective.keys().cloned().collect::<BTreeSet<_>>();
    if let Ok(entries) = fs::read_dir(destination) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if path.is_symlink()
                && !expected.contains(name)
                && fs::read_link(&path)
                    .ok()
                    .is_some_and(|target| target.starts_with(&paths.agents_home))
            {
                fs::remove_file(&path)?;
                println!("  removed stale {} skill link {}", harness, path.display());
            }
        }
    }
    for (name, (source, _)) in effective {
        link_skill(paths, &source, &name, destination)?;
    }
    Ok(())
}

fn link_skill(paths: &Paths, source: &Path, name: &str, destination_root: &Path) -> Result<()> {
    let destination = destination_root.join(name);
    if destination.is_symlink() {
        if fs::read_link(&destination).ok().as_deref() == Some(source) {
            return Ok(());
        }
        fs::remove_file(&destination)?;
    } else if destination.exists() {
        if destination.starts_with(&paths.agents_home) {
            bail!(
                "skill destination is inside agents-home: {}",
                destination.display()
            );
        }
        let local = destination.join("SKILL.md");
        let managed = source.join("SKILL.md");
        if fs::read(&local).ok() == fs::read(&managed).ok() {
            println!("  skip {} (local copy is identical)", destination.display());
        } else {
            println!(
                "  conflict {} (local skill remains in place)",
                destination.display()
            );
        }
        return Ok(());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, &destination)?;
    println!("  linked {} -> {}", destination.display(), source.display());
    Ok(())
}

pub fn skills(paths: &Paths, harness: Option<&str>) -> Result<()> {
    if let Some(harness) = harness {
        validate_harness(harness)?;
        if !paths.shared_md.is_file() {
            println!("Agents home: not initialized. Run: agents init");
        }
        println!("{:<32} SOURCE", "SKILL");
        let destination = harness_skill_root(paths, harness);
        let effective = effective_skills(paths, harness);
        let mut names = effective.keys().cloned().collect::<BTreeSet<_>>();
        names.extend(skill_names(destination));
        for name in names {
            let source = if destination.join(&name).exists()
                && !destination.join(&name).is_symlink()
                && effective.contains_key(&name)
            {
                "local conflict"
            } else if let Some((_, source)) = effective.get(&name) {
                source
            } else {
                "local"
            };
            println!("{name:<32} {source}");
        }
        return Ok(());
    }

    if !paths.shared_md.is_file() {
        println!("Agents home: not initialized. Run: agents init");
    }
    println!(
        "{:<28} {:<10} {:<10} {:<10} {:<10} {:<10}",
        "SKILL", "shared", "claude", "codex", "grok", "opencode"
    );
    let mut names = skill_names(&paths.shared_skills);
    for harness in HARNESSES {
        names.extend(skill_names(&paths.harness_skills(harness)));
        names.extend(skill_names(harness_skill_root(paths, harness)));
    }
    for name in names {
        print!(
            "{name:<28} {:<10}",
            if paths.shared_skills.join(&name).is_dir() {
                "yes"
            } else {
                "—"
            }
        );
        for harness in HARNESSES {
            let state = if paths.harness_skills(harness).join(&name).is_dir() {
                if paths.shared_skills.join(&name).is_dir() {
                    "override"
                } else {
                    "specific"
                }
            } else if paths.shared_skills.join(&name).is_dir() {
                "shared"
            } else if harness_skill_root(paths, harness).join(&name).exists() {
                "local"
            } else {
                "—"
            };
            print!(" {state:<10}");
        }
        println!();
    }
    println!();
    println!("A harness-specific skill replaces a shared skill with the same name.");
    Ok(())
}

fn harness_skill_root<'a>(paths: &'a Paths, harness: &str) -> &'a Path {
    match harness {
        "claude" => &paths.claude_skills,
        "codex" => &paths.codex_skills,
        "grok" => &paths.grok_skills,
        "opencode" => &paths.opencode_skills,
        _ => unreachable!("validated harness"),
    }
}

pub fn md(paths: &Paths, harness: Option<&str>) -> Result<()> {
    if let Some(harness) = harness {
        validate_harness(harness)?;
        print_source_markdown(paths, harness)?;
        return Ok(());
    }
    for harness in HARNESSES {
        println!("======== {harness} ========");
        print_source_markdown(paths, harness)?;
        println!();
    }
    Ok(())
}

fn print_source_markdown(paths: &Paths, harness: &str) -> Result<()> {
    if !paths.shared_md.is_file() {
        bail!("agents home is not initialized; run agents init")
    }
    println!("<!-- shared: {} -->", paths.shared_md.display());
    print!("{}", fs::read_to_string(&paths.shared_md)?);
    println!();
    let specific = paths.harness_md(harness);
    if file_nonempty(&specific) {
        println!("<!-- {harness}: {} -->", specific.display());
        print!("{}", fs::read_to_string(specific)?);
        println!();
    }
    Ok(())
}

fn ensure_initialized(paths: &Paths) -> Result<()> {
    if !paths.agents_home.join(".git").is_dir() {
        bail!("agents home is not configured; run agents init, then connect its Git remote")
    }
    if !paths.shared_md.is_file() {
        bail!("agents home uses an unsupported layout; run agents init or migrate it")
    }
    Ok(())
}

fn commit_home_changes(paths: &Paths, message: &str) -> Result<()> {
    util::command_status("git", ["add", "-A"], Some(&paths.agents_home))?;
    let status = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(&paths.agents_home)
        .status()?;
    if status.success() {
        println!("Nothing to commit.");
    } else if status.code() == Some(1) {
        util::command_status("git", ["commit", "-m", message], Some(&paths.agents_home))?;
    } else {
        bail!("could not inspect staged agents-home changes")
    }
    Ok(())
}

fn fetch_origin(repo: &Path, message: &str) -> Result<()> {
    let activity = Activity::delayed(message, Duration::from_millis(300));
    let output = Command::new("git")
        .args(["fetch", "--prune", "origin"])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        bail!(
            "git fetch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    activity.finish("Remote state fetched");
    Ok(())
}

fn rebase_upstream(repo: &Path) -> Result<()> {
    let Some(upstream) = upstream(repo) else {
        return Ok(());
    };
    let status = Command::new("git")
        .args(["rebase", &upstream])
        .current_dir(repo)
        .status()?;
    if status.success() {
        return Ok(());
    }
    let _ = Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(repo)
        .status();
    bail!("agents-home rebase conflicted; local commits remain unchanged")
}

fn push_with_retry(repo: &Path) -> Result<()> {
    for attempt in 1..=3 {
        let status = Command::new("git")
            .args(["push", "-u", "origin", "HEAD"])
            .current_dir(repo)
            .status()?;
        if status.success() {
            return Ok(());
        }
        if attempt == 3 {
            bail!("push failed after three attempts")
        }
        fetch_origin(repo, "Refreshing agents-home before retry")?;
        rebase_upstream(repo)?;
    }
    unreachable!()
}

fn has_origin(repo: &Path) -> bool {
    origin_url(repo).is_some()
}

fn origin_url(repo: &Path) -> Option<String> {
    git_text(repo, &["remote", "get-url", "origin"])
        .ok()
        .filter(|value| !value.is_empty())
}

fn upstream(repo: &Path) -> Option<String> {
    if let Ok(value) = git_text(
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    ) {
        return Some(value);
    }
    let branch = git_text(repo, &["branch", "--show-current"]).ok()?;
    let candidate = format!("origin/{branch}");
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &candidate])
        .current_dir(repo)
        .status()
        .ok()
        .filter(|status| status.success())
        .map(|_| candidate)
}

fn ahead_behind(repo: &Path, upstream: &str) -> Result<(usize, usize)> {
    let range = format!("HEAD...{upstream}");
    let text = git_text(repo, &["rev-list", "--left-right", "--count", &range])?;
    let mut fields = text.split_whitespace();
    let ahead = fields.next().context("missing ahead count")?.parse()?;
    let behind = fields.next().context("missing behind count")?.parse()?;
    Ok((ahead, behind))
}

fn git_dirty(repo: &Path) -> Result<bool> {
    Ok(!git_text(repo, &["status", "--porcelain"])?.is_empty())
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(repo).output()?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "))
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
