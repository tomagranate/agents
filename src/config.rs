use std::{env, path::PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub agents_home: PathBuf,
    pub shared_md: PathBuf,
    pub harnesses_dir: PathBuf,
    pub shared_skills: PathBuf,
    pub claude_md: PathBuf,
    pub claude_settings: PathBuf,
    pub claude_skills: PathBuf,
    pub codex_md: PathBuf,
    pub codex_settings: PathBuf,
    pub codex_skills: PathBuf,
    pub grok_rules: PathBuf,
    pub grok_settings: PathBuf,
    pub grok_skills: PathBuf,
    pub opencode_dir: PathBuf,
    pub opencode_jsonc: PathBuf,
    pub opencode_skills: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let agents_home = env::var_os("AGENTS_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".agents"));
        let config_dir = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("agents");
        let state_dir = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("agents");
        let shared_dir = agents_home.join("shared");
        let harnesses_dir = agents_home.join("harnesses");
        let opencode_dir = home.join(".config/opencode");
        Ok(Self {
            shared_md: shared_dir.join("AGENTS.md"),
            shared_skills: shared_dir.join("skills"),
            claude_md: home.join(".claude/CLAUDE.md"),
            claude_settings: home.join(".claude/settings.json"),
            claude_skills: home.join(".claude/skills"),
            codex_md: home.join(".codex/AGENTS.md"),
            codex_settings: home.join(".codex/config.toml"),
            codex_skills: home.join(".codex/skills"),
            grok_rules: home.join(".grok/rules"),
            grok_settings: home.join(".grok/config.toml"),
            grok_skills: home.join(".grok/skills"),
            opencode_jsonc: opencode_dir.join("opencode.jsonc"),
            opencode_skills: opencode_dir.join("skills"),
            home,
            agents_home,
            harnesses_dir,
            opencode_dir,
            config_dir,
            state_dir,
        })
    }

    pub fn harness_dir(&self, harness: &str) -> PathBuf {
        self.harnesses_dir.join(harness)
    }

    pub fn harness_md(&self, harness: &str) -> PathBuf {
        self.harness_dir(harness).join("AGENTS.md")
    }

    pub fn harness_skills(&self, harness: &str) -> PathBuf {
        self.harness_dir(harness).join("skills")
    }
}
