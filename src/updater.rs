use std::{
    fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Args;
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use tempfile::{NamedTempFile, tempdir};

use crate::util;

// Releases live in the private Hangar monorepo, tagged agents-vX.Y.Z. Every
// request needs a GitHub token: GH_TOKEN, GITHUB_TOKEN, or `gh auth token`.
const REPOSITORY: &str = "tomagranate/hangar";
const TAG_PREFIX: &str = "agents-v";

#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    /// Report whether an update is available without installing it.
    #[arg(long)]
    pub check: bool,
    /// Install one release version instead of the newest version.
    #[arg(long)]
    pub version: Option<String>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    /// The API URL. Private repos serve asset bytes only through it.
    url: String,
}

#[derive(Deserialize)]
struct GitRef {
    #[serde(rename = "ref")]
    name: String,
}

pub fn run(args: UpdateArgs) -> Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let release = fetch_release(args.version.as_deref())?;
    let latest = release_version(&release.tag_name)
        .with_context(|| format!("invalid release version {}", release.tag_name))?;
    if latest <= current && args.version.is_none() {
        println!("agents {current} is current.");
        return Ok(());
    }
    if args.check {
        println!("agents {current} → {latest}");
        return Ok(());
    }

    let executable = std::env::current_exe()?.canonicalize()?;
    if is_homebrew_install(&executable) {
        bail!(
            "agents {latest} is out, but Homebrew can no longer install agents. \
             Run `brew uninstall agents`, then `primer update` to install it from Hangar."
        );
    }

    install_standalone(&release, &executable)?;
    println!("Updated agents {current} → {latest}.");
    Ok(())
}

fn fetch_release(version: Option<&str>) -> Result<Release> {
    let tag = match version {
        Some(version) => format!("{TAG_PREFIX}{}", version.trim_start_matches('v')),
        None => newest_tag()?,
    };
    get(&format!(
        "https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}"
    ))?
    .json()
    .context("could not read the GitHub release")
}

/// Finds the highest agents-vX.Y.Z tag. Hangar also publishes releases for
/// other packages, so GitHub's "latest release" is not the agents release.
fn newest_tag() -> Result<String> {
    let refs: Vec<GitRef> = get(&format!(
        "https://api.github.com/repos/{REPOSITORY}/git/matching-refs/tags/{TAG_PREFIX}"
    ))?
    .json()
    .context("could not read the agents release tags")?;
    newest(
        refs.iter()
            .filter_map(|r| r.name.strip_prefix("refs/tags/")),
    )
    .context("no agents release found")
}

/// Picks the highest version among agents-vX.Y.Z tags and ignores the rest.
fn newest<'a>(tags: impl Iterator<Item = &'a str>) -> Option<String> {
    tags.filter_map(|tag| release_version(tag).map(|version| (version, tag)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, tag)| tag.to_owned())
}

fn release_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix(TAG_PREFIX)?).ok()
}

pub(crate) fn latest_version() -> Result<Version> {
    let tag = newest_tag()?;
    release_version(&tag).with_context(|| format!("invalid release version {tag}"))
}

fn token() -> Result<String> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return Ok(value.trim().to_owned());
        }
    }
    let output = util::command_output("gh", ["auth", "token"], None)
        .context("agents releases are private: install gh and run `gh auth login`")?;
    let token = String::from_utf8(output.stdout)?.trim().to_owned();
    if token.is_empty() {
        bail!("gh has no token: run `gh auth login`");
    }
    Ok(token)
}

fn get(url: &str) -> Result<reqwest::blocking::Response> {
    request(url, "application/vnd.github+json")
}

fn request(url: &str, accept: &str) -> Result<reqwest::blocking::Response> {
    Ok(reqwest::blocking::Client::new()
        .get(url)
        .header(
            "User-Agent",
            format!("agents/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(token()?)
        .send()?
        .error_for_status()?)
}

fn is_homebrew_install(executable: &Path) -> bool {
    let Ok(output) = util::command_output("brew", ["--prefix", "agents"], None) else {
        return false;
    };
    let prefix = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    !prefix.is_empty() && executable.starts_with(Path::new(&prefix))
}

fn install_standalone(release: &Release, executable: &Path) -> Result<()> {
    let target = release_target()?;
    let archive_name = format!("agents-{target}.tar.gz");
    let checksum_name = format!("{archive_name}.sha256");
    let archive = release
        .assets
        .iter()
        .find(|asset| asset.name == archive_name)
        .with_context(|| format!("release does not contain {archive_name}"))?;
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)
        .with_context(|| format!("release does not contain {checksum_name}"))?;

    println!("Downloading {}...", release.tag_name);
    let archive_bytes = download(&archive.url)?;
    let checksum_text = String::from_utf8(download(&checksum.url)?)?;
    let expected = checksum_text
        .split_whitespace()
        .next()
        .context("checksum file is empty")?;
    let actual = util::sha256_hex(&archive_bytes);
    if actual != expected {
        bail!("release checksum mismatch: expected {expected}, got {actual}");
    }

    let extraction = tempdir()?;
    tar::Archive::new(GzDecoder::new(Cursor::new(archive_bytes))).unpack(extraction.path())?;
    let candidate =
        find_binary(extraction.path()).context("release archive has no agents binary")?;
    let parent = executable
        .parent()
        .context("installed binary has no parent directory")?;
    let mut replacement = NamedTempFile::new_in(parent)?;
    let mut source = fs::File::open(candidate)?;
    std::io::copy(&mut source, &mut replacement)?;
    replacement.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        replacement
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    replacement.as_file().sync_all()?;
    replacement
        .persist(executable)
        .map_err(|error| error.error)?;
    Ok(())
}

fn download(url: &str) -> Result<Vec<u8>> {
    let mut response = request(url, "application/octet-stream")?;
    let mut bytes = Vec::new();
    response.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn find_binary(root: &Path) -> Option<PathBuf> {
    walkdir::WalkDir::new(root)
        .max_depth(3)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .find(|path| path.is_file() && path.file_name().is_some_and(|name| name == "agents"))
}

fn release_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        (os, arch) => bail!("no release build for {os}/{arch}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_picks_the_highest_agents_version_and_ignores_other_packages() {
        let tags = [
            "agents-v0.9.0",
            "runners-v9.9.9",
            "agents-v0.12.0",
            "primer-0123abcd",
            "agents-v0.11.3",
            "agents-vnot-a-version",
        ];
        assert_eq!(newest(tags.into_iter()).as_deref(), Some("agents-v0.12.0"));
        assert_eq!(newest(["runners-v0.1.19"].into_iter()), None);
    }
}
