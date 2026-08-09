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

const REPOSITORY: &str = "tomagranate/agents";

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
    browser_download_url: String,
}

pub fn run(args: UpdateArgs) -> Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let release = fetch_release(args.version.as_deref())?;
    let latest = Version::parse(release.tag_name.trim_start_matches('v'))
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
        if args.version.is_some() {
            bail!("Homebrew installs the newest formula version only; omit --version");
        }
        println!("Updating the Homebrew installation...");
        util::command_status("brew", ["update"], None)?;
        util::command_status("brew", ["upgrade", "agents"], None)?;
        println!("Updated agents through Homebrew.");
        return Ok(());
    }

    install_standalone(&release, &executable)?;
    println!("Updated agents {current} → {latest}.");
    Ok(())
}

fn fetch_release(version: Option<&str>) -> Result<Release> {
    let endpoint = if let Some(version) = version {
        format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/tags/v{}",
            version.trim_start_matches('v')
        )
    } else {
        format!("https://api.github.com/repos/{REPOSITORY}/releases/latest")
    };
    reqwest::blocking::Client::new()
        .get(endpoint)
        .header(
            "User-Agent",
            format!("agents/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()?
        .error_for_status()?
        .json()
        .context("could not read the GitHub release")
}

pub(crate) fn latest_version() -> Result<Version> {
    let release = fetch_release(None)?;
    Version::parse(release.tag_name.trim_start_matches('v'))
        .with_context(|| format!("invalid release version {}", release.tag_name))
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
    let archive_bytes = download(&archive.browser_download_url)?;
    let checksum_text = String::from_utf8(download(&checksum.browser_download_url)?)?;
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
    let mut response = reqwest::blocking::Client::new()
        .get(url)
        .header(
            "User-Agent",
            format!("agents/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()?
        .error_for_status()?;
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
