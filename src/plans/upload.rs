use std::{
    fs,
    net::ToSocketAddrs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use reqwest::{Url, blocking::multipart, header::LOCATION};

use super::client::{Client, PlanResponse};

const MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024;

pub struct UploadOptions {
    pub path: PathBuf,
    /// None sends no membership fields (keep existing membership on replace).
    /// Some(None) clears membership; Some(Some(slug)) sets it.
    pub project: Option<Option<String>>,
    pub title: Option<String>,
    pub entry: String,
    pub replace: Option<String>,
    /// Upload a prototype instead of a plan document.
    pub prototype: bool,
}

struct UploadFile {
    path: String,
    contents: Vec<u8>,
    content_type: &'static str,
}

pub fn upload(client: &Client, options: UploadOptions) -> Result<String> {
    let files = collect_files(&options.path, &options.entry)?;
    let mut form = multipart::Form::new().text("entry", options.entry);

    match options.project {
        Some(Some(project)) => form = form.text("project", project),
        Some(None) => form = form.text("no_project", "1"),
        None => {}
    }
    if let Some(title) = options.title {
        form = form.text("title", title);
    }
    if let Some(replace) = options.replace {
        form = form.text("replace", replace);
    }
    if options.prototype {
        form = form.text("kind", "prototype");
    }

    for file in files {
        let part = multipart::Part::bytes(file.contents)
            .file_name(file.path)
            .mime_str(file.content_type)?;
        form = form.part("files", part);
    }

    let response = client.post("/api/plans").multipart(form).send()?;
    let header_location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body: PlanResponse = client.json(response)?;
    if options.prototype {
        let url = body
            .plan
            .prototype_url
            .context("the plans service did not return a prototype URL; it may be out of date")?;
        warn_if_unresolvable(&url);
        return Ok(url);
    }
    let location = header_location.or(body.location);

    Ok(location
        .map(|location| client.resolve_location(&location))
        .unwrap_or_else(|| client.plan_url(&body.plan.id)))
}

// Prototype hosts need a wildcard DNS record that lives outside this service.
// Warn when the host does not resolve, so a missing record is loud.
fn warn_if_unresolvable(url: &str) {
    let Some(host) = Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    else {
        return;
    };
    if (host.as_str(), 443).to_socket_addrs().is_err() {
        eprintln!(
            "warning: {host} does not resolve. Check the wildcard DNS record for prototype hosts."
        );
    }
}

fn collect_files(path: &Path, entry: &str) -> Result<Vec<UploadFile>> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("could not read {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("upload path cannot be a symbolic link");
    }

    let mut paths = Vec::new();
    let expected_entry = if metadata.is_file() {
        if !matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("html" | "htm")
        ) {
            bail!("single-file uploads must use the .html or .htm extension");
        }
        path.file_name()
            .and_then(|name| name.to_str())
            .context("upload filename is not valid UTF-8")?
            .to_owned()
    } else if metadata.is_dir() {
        let entry = normalize_upload_path(entry)?;
        collect_directory(path, path, &mut paths)?;
        entry
    } else {
        bail!("upload path must be an HTML file or directory");
    };

    if metadata.is_file() {
        normalize_upload_path(&expected_entry)?;
        paths.push((path.to_path_buf(), expected_entry.clone()));
    }
    if paths.is_empty() {
        bail!("upload is empty");
    }
    if !paths
        .iter()
        .any(|(_, relative)| relative == &expected_entry)
    {
        bail!("entry file is missing: {expected_entry}");
    }
    let entry_lower = expected_entry.to_ascii_lowercase();
    if !entry_lower.ends_with(".html") && !entry_lower.ends_with(".htm") {
        bail!("entry file must be HTML");
    }

    paths.sort_by(|left, right| left.1.cmp(&right.1));
    let mut total = 0_u64;
    let mut files = Vec::with_capacity(paths.len());
    for (source, relative) in paths {
        let size = fs::metadata(&source)?.len();
        total = total.checked_add(size).context("upload size overflowed")?;
        if total > MAX_UPLOAD_BYTES {
            bail!("upload exceeds the 10 MB limit");
        }
        files.push(UploadFile {
            content_type: content_type(&relative),
            contents: fs::read(&source)
                .with_context(|| format!("could not read {}", source.display()))?,
            path: relative,
        });
    }
    if total == 0 {
        bail!("upload is empty");
    }

    Ok(files)
}

fn collect_directory(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("could not read {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "upload directories cannot contain symbolic links: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            collect_directory(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .context("file escaped the upload directory")?;
            let relative = relative
                .to_str()
                .context("upload path is not valid UTF-8")?;
            let relative = normalize_upload_path(relative)?;
            files.push((path, relative));
        }
    }
    Ok(())
}

fn normalize_upload_path(path: &str) -> Result<String> {
    let candidate = path.replace('\\', "/");
    let bytes = candidate.as_bytes();
    let has_windows_prefix =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    if candidate.is_empty()
        || candidate.starts_with('/')
        || has_windows_prefix
        || candidate.contains('\0')
        || candidate.split('/').any(|part| part == "..")
    {
        bail!("path escape is not allowed: {path}");
    }

    let normalized = candidate
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        bail!("path escape is not allowed: {path}");
    }
    Ok(normalized)
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "md" => "text/markdown",
        "txt" => "text/plain",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "mjs" => "text/javascript",
        "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}
