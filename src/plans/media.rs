use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use reqwest::blocking::multipart;
use serde::{Deserialize, Serialize};

use super::client::{Client, MediaItem};

const DIRECT_UPLOAD_LIMIT: u64 = 95_000_000;
const MAX_UPLOAD_SIZE: u64 = 500_000_000;

#[derive(Deserialize)]
struct MediaResponse {
    url: String,
    id: String,
    #[allow(dead_code)]
    key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LargeUploadResponse {
    upload: PresignedUpload,
    confirm: ConfirmRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresignedUpload {
    url: Option<String>,
    #[serde(default)]
    headers: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ConfirmRequest {
    path: String,
    body: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LargeUploadMetadata<'a> {
    filename: &'a str,
    content_type: &'a str,
    byte_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

/// Small files go straight to the service. Large files use a presigned upload.
pub fn put(client: &Client, path: PathBuf, name: Option<String>) -> Result<MediaItem> {
    let metadata =
        fs::metadata(&path).with_context(|| format!("could not read {}", path.display()))?;
    if !metadata.is_file() {
        bail!("media path must be a file");
    }
    if metadata.len() > MAX_UPLOAD_SIZE {
        bail!("media cannot exceed 500 MB");
    }
    let content_type = media_content_type(&path)?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("media filename is not valid UTF-8")?;

    let response = if metadata.len() <= DIRECT_UPLOAD_LIMIT {
        let file = multipart::Part::bytes(
            fs::read(&path).with_context(|| format!("could not read {}", path.display()))?,
        )
        .file_name(filename.to_owned())
        .mime_str(content_type)?;
        let mut form = multipart::Form::new().part("file", file);
        if let Some(name) = name {
            form = form.text("name", name);
        }
        let response = client.post("/api/media").multipart(form).send()?;
        client.json::<MediaResponse>(response)?
    } else {
        large_put(
            client,
            &path,
            filename,
            content_type,
            metadata.len(),
            name.as_deref(),
        )?
    };

    Ok(MediaItem {
        id: response.id,
        key: response.key,
        url: response.url,
        byte_size: metadata.len(),
    })
}

fn large_put(
    client: &Client,
    path: &Path,
    filename: &str,
    content_type: &str,
    byte_size: u64,
    name: Option<&str>,
) -> Result<MediaResponse> {
    let metadata = LargeUploadMetadata {
        filename,
        content_type,
        byte_size,
        name,
    };
    let response = client.post("/api/media").json(&metadata).send()?;
    let prepared: LargeUploadResponse = client.json(response)?;
    let upload_url = prepared
        .upload
        .url
        .context("the server did not provide a presigned upload URL")?;

    let mut request = client
        .put_absolute(&upload_url)
        .body(fs::read(path).with_context(|| format!("could not read {}", path.display()))?);
    for (name, value) in prepared.upload.headers {
        if let Some(value) = value.as_str() {
            request = request.header(name, value);
        }
    }
    client.empty(request.send()?)?;

    let response = client
        .post(&prepared.confirm.path)
        .json(&prepared.confirm.body)
        .send()?;
    client.json(response)
}

/// Accepts either the media id or the storage key.
pub fn remove(client: &Client, id_or_key: &str) -> Result<()> {
    let items = client.list_media()?;
    let id = items
        .iter()
        .find(|item| item.id == id_or_key || item.key == id_or_key)
        .map(|item| item.id.as_str())
        .unwrap_or(id_or_key);
    client.delete_media(id)
}

fn media_content_type(path: &Path) -> Result<&'static str> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => Ok("image/png"),
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "gif" => Ok("image/gif"),
        "webp" => Ok("image/webp"),
        "avif" => Ok("image/avif"),
        "mp4" => Ok("video/mp4"),
        "webm" => Ok("video/webm"),
        "mov" | "qt" => Ok("video/quicktime"),
        _ => bail!(
            "unsupported media type: allowed extensions are png, jpg, jpeg, gif, webp, avif, mp4, webm, mov, and qt"
        ),
    }
}
