use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};

use crate::{config::Paths, util};

const HARNESSES: [&str; 4] = ["claude", "codex", "grok", "opencode"];
const SHARED_TEMPLATE: &str = include_str!("../share/templates/AGENTS.md");
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

fn file_nonempty(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|text| text.chars().any(|character| !character.is_whitespace()))
        .unwrap_or(false)
}

fn count_skills(path: &Path) -> usize {
    skill_names(path).len()
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
        if name.starts_with('.') || !candidate.join("SKILL.md").exists() {
            continue;
        }
        names.insert(name.to_owned());
    }
    names
}

pub fn status(paths: &Paths) -> Result<()> {
    println!("agents {}", env!("CARGO_PKG_VERSION"));
    println!("Global agents home: {}", paths.agents_home.display());
    println!("Templates: embedded");
    println!();
    println!("AGENTS sources");
    print_source("shared", &paths.shared_md);
    for harness in HARNESSES {
        print_source(harness, &paths.harness_dir.join(format!("{harness}.md")));
    }
    println!();
    println!("Harness entrypoints");
    print_entrypoint("claude", &paths.claude_md);
    print_entrypoint("codex", &paths.codex_md);
    if paths.grok_rules.is_dir() {
        let count = fs::read_dir(&paths.grok_rules)?
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|value| value == "md"))
            .count();
        println!(
            "  {:<12} dir {} ({count} md)",
            "grok",
            paths.grok_rules.display()
        );
    } else {
        println!("  {:<12} MISSING", "grok");
    }
    if paths.opencode_jsonc.is_file() {
        println!(
            "  {:<12} jsonc {}",
            "opencode",
            paths.opencode_jsonc.display()
        );
    } else {
        println!("  {:<12} MISSING", "opencode");
    }
    println!();
    println!("Skills (top-level SKILL.md dirs)");
    for (name, path) in [
        ("shared", &paths.shared_skills),
        ("claude", &paths.claude_skills),
        ("codex", &paths.codex_skills),
        ("grok", &paths.grok_skills),
        ("opencode", &paths.opencode_skills),
    ] {
        println!("  {name:<12} {}", count_skills(path));
    }
    println!();
    println!(
        "Commands: agents init | agents sync | agents skills | agents md [harness] | agents archive"
    );
    Ok(())
}

fn print_source(name: &str, path: &Path) {
    let state = if file_nonempty(path) {
        format!("ok  {}", path.display())
    } else if path.is_file() {
        format!("empty  {}", path.display())
    } else {
        format!("MISSING  {}", path.display())
    };
    println!("  {name:<12} {state}");
}

fn print_entrypoint(name: &str, path: &Path) {
    let state = if path.is_symlink() {
        fs::read_link(path)
            .map(|target| format!("symlink {}", target.display()))
            .unwrap_or_else(|_| "broken symlink".to_owned())
    } else if path.is_file() {
        format!("file {}", path.display())
    } else {
        "MISSING".to_owned()
    };
    println!("  {name:<12} {state}");
}

pub fn init(paths: &Paths, force: bool, do_sync: bool) -> Result<()> {
    println!(
        "Scaffolding {} from embedded templates",
        paths.agents_home.display()
    );
    fs::create_dir_all(&paths.harness_dir)?;
    fs::create_dir_all(&paths.shared_skills)?;
    install_template(&paths.shared_md, SHARED_TEMPLATE, force)?;
    for harness in HARNESSES {
        install_template(
            &paths.harness_dir.join(format!("{harness}.md")),
            harness_template(harness),
            force,
        )?;
    }
    if do_sync {
        println!();
        sync(paths)
    } else {
        println!("Done. Run: agents sync");
        Ok(())
    }
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

pub fn sync(paths: &Paths) -> Result<()> {
    if !paths.shared_md.is_file() {
        bail!("missing {} — run: agents init", paths.shared_md.display());
    }
    println!("Syncing agents entrypoints...");
    for directory in [
        &paths.harness_dir,
        &paths.claude_skills,
        &paths.codex_skills,
        &paths.grok_rules,
        &paths.grok_skills,
        &paths.opencode_dir,
        &paths.opencode_skills,
    ] {
        fs::create_dir_all(directory)?;
    }
    for harness in HARNESSES {
        let target = paths.harness_dir.join(format!("{harness}.md"));
        if !target.is_file() {
            util::atomic_write(&target, harness_template(harness).as_bytes())?;
            println!("  created {}", target.display());
        }
    }

    util::atomic_write(
        &paths.claude_md,
        b"@~/.agents/AGENTS.md\n@~/.agents/harness/claude.md\n",
    )?;
    println!("  wrote {}", paths.claude_md.display());

    if paths.codex_md.is_symlink() {
        fs::remove_file(&paths.codex_md)?;
    }
    let mut codex = String::from(
        "<!-- Generated by `agents sync`. Edit ~/.agents/AGENTS.md and ~/.agents/harness/codex.md -->\n\n",
    );
    codex.push_str(&fs::read_to_string(&paths.shared_md)?);
    codex.push('\n');
    let codex_harness = paths.harness_dir.join("codex.md");
    if file_nonempty(&codex_harness) {
        codex.push_str(&fs::read_to_string(codex_harness)?);
        codex.push('\n');
    }
    util::atomic_write(&paths.codex_md, codex.as_bytes())?;
    println!("  wrote {}", paths.codex_md.display());

    replace_symlink(&paths.grok_rules.join("AGENTS.md"), &paths.shared_md)?;
    replace_symlink(
        &paths.grok_rules.join("harness-grok.md"),
        &paths.harness_dir.join("grok.md"),
    )?;

    let opencode = format!(
        "{{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"instructions\": [\n    \"{}\",\n    \"{}\"\n  ]\n}}\n",
        paths.shared_md.display(),
        paths.harness_dir.join("opencode.md").display()
    );
    util::atomic_write(&paths.opencode_jsonc, opencode.as_bytes())?;
    println!("  wrote {}", paths.opencode_jsonc.display());
    if !paths.opencode_md.is_file()
        || fs::read_to_string(&paths.opencode_md)
            .unwrap_or_default()
            .contains("via `opencode.jsonc`")
    {
        util::atomic_write(
            &paths.opencode_md,
            b"## OpenCode\n\nHarness-only notes. Shared and OpenCode rules load from `opencode.jsonc`.\n",
        )?;
        println!("  wrote {}", paths.opencode_md.display());
    }

    println!();
    println!("Linking shared skills into Claude and Codex...");
    for name in skill_names(&paths.shared_skills) {
        link_skill(paths, &name, &paths.claude_skills)?;
        link_skill(paths, &name, &paths.codex_skills)?;
    }
    println!();
    println!("Done. Run: agents skills | agents md");
    Ok(())
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

fn link_skill(paths: &Paths, name: &str, destination_root: &Path) -> Result<()> {
    let source = paths.shared_skills.join(name);
    let destination = destination_root.join(name);
    fs::create_dir_all(destination_root)?;
    if destination.is_symlink() {
        if fs::read_link(&destination).ok().as_deref() == Some(source.as_path()) {
            return Ok(());
        }
        fs::remove_file(&destination)?;
    } else if destination.exists() {
        let local = destination.join("SKILL.md");
        let shared = source.join("SKILL.md");
        if fs::read(&local).ok() == fs::read(&shared).ok() {
            println!("  skip {} (local copy identical)", destination.display());
        } else {
            println!(
                "  conflict {} (local differs; leave local)",
                destination.display()
            );
        }
        return Ok(());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&source, &destination)?;
    println!("  linked {} -> {}", destination.display(), source.display());
    Ok(())
}

pub fn skills(paths: &Paths) -> Result<()> {
    println!(
        "{:<28} {:<10} {:<14} {:<14} {:<14} {:<14}",
        "SKILL", "shared", "claude", "codex", "grok", "opencode"
    );
    let mut names = BTreeSet::new();
    for path in [
        &paths.shared_skills,
        &paths.claude_skills,
        &paths.codex_skills,
        &paths.grok_skills,
        &paths.opencode_skills,
    ] {
        names.extend(skill_names(path));
    }
    for name in names {
        if name == ".system" {
            continue;
        }
        println!(
            "{name:<28} {:<10} {:<14} {:<14} {:<14} {:<14}",
            if paths.shared_skills.join(&name).exists() {
                "yes"
            } else {
                "—"
            },
            skill_kind(paths, &paths.claude_skills.join(&name)),
            skill_kind(paths, &paths.codex_skills.join(&name)),
            skill_kind(paths, &paths.grok_skills.join(&name)),
            skill_kind(paths, &paths.opencode_skills.join(&name)),
        );
    }
    println!();
    println!(
        "Legend: shared = ~/.agents/skills | local = harness-only | link→shared = shared symlink"
    );
    Ok(())
}

fn skill_kind(paths: &Paths, path: &Path) -> &'static str {
    if path.is_symlink() {
        if fs::canonicalize(path)
            .ok()
            .is_some_and(|resolved| resolved.starts_with(&paths.shared_skills))
        {
            "link→shared"
        } else {
            "link→other"
        }
    } else if path.is_dir() {
        if path.starts_with(&paths.shared_skills) {
            "shared"
        } else {
            "local"
        }
    } else {
        "—"
    }
}

pub fn md(paths: &Paths, harness: Option<&str>) -> Result<()> {
    if let Some(harness) = harness {
        println!("======== {harness} ========");
        print_resolved(paths, harness)?;
        return Ok(());
    }
    for harness in HARNESSES {
        println!("======== {harness} ========");
        print_resolved(paths, harness)?;
        println!();
    }
    Ok(())
}

fn print_resolved(paths: &Paths, harness: &str) -> Result<()> {
    match harness {
        "claude" => {
            if paths.claude_md.is_file() {
                print_claude_imports(paths, &paths.claude_md, 0)
            } else {
                print_shared_and_harness(paths, harness)
            }
        }
        "codex" => print_existing_or_shared(paths, &paths.codex_md, harness),
        "grok" => {
            let mut files: Vec<PathBuf> = fs::read_dir(&paths.grok_rules)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|value| value == "md"))
                .collect();
            files.sort();
            if files.is_empty() {
                print_shared_and_harness(paths, harness)
            } else {
                for path in files {
                    println!("<!-- file: {} -->", path.display());
                    print!("{}", fs::read_to_string(path)?);
                    println!();
                }
                Ok(())
            }
        }
        "opencode" => print_opencode(paths),
        _ => bail!("unknown harness: {harness} (claude|codex|grok|opencode)"),
    }
}

fn print_existing_or_shared(paths: &Paths, path: &Path, harness: &str) -> Result<()> {
    if path.is_file() && !path.is_symlink() {
        print!("{}", fs::read_to_string(path)?);
        Ok(())
    } else {
        print_shared_and_harness(paths, harness)
    }
}

fn print_shared_and_harness(paths: &Paths, harness: &str) -> Result<()> {
    if paths.shared_md.is_file() {
        print!("{}", fs::read_to_string(&paths.shared_md)?);
        println!();
    }
    let harness_path = paths.harness_dir.join(format!("{harness}.md"));
    if file_nonempty(&harness_path) {
        print!("{}", fs::read_to_string(harness_path)?);
        println!();
    }
    Ok(())
}

fn print_claude_imports(paths: &Paths, path: &Path, depth: usize) -> Result<()> {
    if depth > 5 {
        println!("<!-- import depth exceeded in {} -->", path.display());
        return Ok(());
    }
    let contents = fs::read_to_string(path)?;
    for line in contents.lines() {
        if let Some(raw_path) = line
            .strip_prefix('@')
            .filter(|line| !line.chars().any(char::is_whitespace))
        {
            let expanded = if let Some(rest) = raw_path.strip_prefix("~/") {
                paths.home.join(rest)
            } else {
                PathBuf::from(raw_path)
            };
            if expanded.is_file() {
                println!("<!-- begin import: {} -->", expanded.display());
                print_claude_imports(paths, &expanded, depth + 1)?;
                println!("<!-- end import: {} -->", expanded.display());
            } else {
                println!("<!-- missing import: {} -->", expanded.display());
                println!("{line}");
            }
        } else {
            println!("{line}");
        }
    }
    Ok(())
}

fn print_opencode(paths: &Paths) -> Result<()> {
    let mut listed = false;
    if paths.opencode_jsonc.is_file() {
        let source = fs::read_to_string(&paths.opencode_jsonc)?;
        if let Ok(value) = json5::from_str::<serde_json::Value>(&source)
            && let Some(instructions) = value.get("instructions").and_then(|value| value.as_array())
        {
            for instruction in instructions.iter().filter_map(|value| value.as_str()) {
                listed = true;
                let expanded = instruction.replace("$HOME", &paths.home.to_string_lossy());
                let path = expanded
                    .strip_prefix("~/")
                    .map(|relative| paths.home.join(relative))
                    .unwrap_or_else(|| PathBuf::from(expanded));
                println!("<!-- instructions: {} -->", path.display());
                if path.is_file() {
                    print!("{}", fs::read_to_string(path)?);
                } else {
                    println!("<!-- missing: {} -->", path.display());
                }
                println!();
            }
        }
    }
    if file_nonempty(&paths.opencode_md) {
        listed = true;
        println!("<!-- file: {} -->", paths.opencode_md.display());
        print!("{}", fs::read_to_string(&paths.opencode_md)?);
        println!();
    }
    if listed {
        Ok(())
    } else {
        print_shared_and_harness(paths, "opencode")
    }
}

pub fn pull(paths: &Paths, do_sync: bool) -> Result<()> {
    ensure_git_repo(&paths.agents_home)?;
    println!("Pulling {}...", paths.agents_home.display());
    util::command_status("git", ["pull", "--ff-only"], Some(&paths.agents_home))?;
    if do_sync {
        println!();
        sync(paths)
    } else {
        println!("Skipped sync. Run: agents sync");
        Ok(())
    }
}

pub fn push(paths: &Paths, message: Option<&str>) -> Result<()> {
    ensure_git_repo(&paths.agents_home)?;
    if let Some(message) = message {
        util::command_status("git", ["add", "-A"], Some(&paths.agents_home))?;
        let output = util::command_output(
            "git",
            ["diff", "--cached", "--quiet"],
            Some(&paths.agents_home),
        );
        if output.is_err() {
            util::command_status("git", ["commit", "-m", message], Some(&paths.agents_home))?;
        } else {
            println!("Nothing to commit.");
        }
    } else {
        let status =
            util::command_output("git", ["status", "--porcelain"], Some(&paths.agents_home))?;
        if !status.stdout.is_empty() {
            bail!("working tree dirty; commit first or use: agents push -m \"message\"");
        }
    }
    println!("Pushing {}...", paths.agents_home.display());
    util::command_status("git", ["push"], Some(&paths.agents_home))?;
    println!("Done.");
    Ok(())
}

fn ensure_git_repo(path: &Path) -> Result<()> {
    if path.join(".git").is_dir() {
        Ok(())
    } else {
        bail!("{} is not a git repository", path.display())
    }
}

pub fn print_paths(paths: &Paths) -> Result<()> {
    println!("version         {}", env!("CARGO_PKG_VERSION"));
    println!("AGENTS_HOME     {}", paths.agents_home.display());
    println!("templates       embedded");
    println!("shared AGENTS   {}", paths.shared_md.display());
    println!("harness dir     {}", paths.harness_dir.display());
    println!("shared skills   {}", paths.shared_skills.display());
    println!();
    println!("claude md       {}", paths.claude_md.display());
    println!("claude skills   {}", paths.claude_skills.display());
    println!();
    println!("codex md        {}", paths.codex_md.display());
    println!("codex skills   {}", paths.codex_skills.display());
    println!();
    println!("grok rules      {}", paths.grok_rules.display());
    println!("grok skills     {}", paths.grok_skills.display());
    println!("grok bundled    {}", paths.grok_bundled.display());
    println!();
    println!("opencode md     {}", paths.opencode_md.display());
    println!("opencode jsonc  {}", paths.opencode_jsonc.display());
    println!("opencode skills {}", paths.opencode_skills.display());
    println!();
    println!(
        "archive config  {}",
        paths.config_dir.join("archive.toml").display()
    );
    println!(
        "archive state   {}",
        paths.state_dir.join("chat-archive.sqlite").display()
    );
    Ok(())
}
