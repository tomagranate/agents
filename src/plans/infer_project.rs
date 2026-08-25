use std::{env, path::Path, process::Command};

use anyhow::{Context, Result, bail};

/// Names the project from the Git remote, or from the directory name.
pub fn infer_project() -> Result<String> {
    let current_dir = env::current_dir().context("could not read the current directory")?;

    if let Some(remote) = git_remote(&current_dir)
        && let Some(name) = repository_name(&remote)
    {
        return Ok(name);
    }

    current_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!("could not infer a project from Git or the current directory")
        })
}

fn git_remote(current_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(current_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn repository_name(remote: &str) -> Option<String> {
    let remote = remote.trim_end_matches('/').trim_end_matches(".git");
    let name = remote.rsplit(['/', ':']).next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Resolves the project for one command. `--no-project` wins over inference.
pub fn selected_project(project: Option<String>, no_project: bool) -> Result<Option<String>> {
    if no_project {
        return Ok(None);
    }
    if let Some(project) = project {
        let project = project.trim();
        if project.is_empty() {
            bail!("project cannot be empty");
        }
        return Ok(Some(project.to_owned()));
    }
    infer_project().map(Some)
}

#[cfg(test)]
mod tests {
    use super::repository_name;

    #[test]
    fn extracts_repository_names() {
        assert_eq!(
            repository_name("git@github.com:tomagranate/agents.git"),
            Some("agents".to_owned())
        );
        assert_eq!(
            repository_name("https://github.com/tomagranate/agents"),
            Some("agents".to_owned())
        );
    }
}
