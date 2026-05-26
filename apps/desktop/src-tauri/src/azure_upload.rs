use crate::UploadProgress;
use crate::general_settings::{AzureStorageConfig, GeneralSettingsStore};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, ipc::Channel};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

const AZURE_API_VERSION: &str = "2021-12-02";
const BLOCK_SIZE: usize = 8 * 1024 * 1024;
const MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub struct AzureUploadedItem {
    pub link: String,
    pub id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AzureUploadError {
    #[error("Azure storage not configured. Set account, container, and SAS token in Settings.")]
    NotConfigured,
    #[error("Recording file missing: {0}")]
    MissingFile(PathBuf),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Azure responded with {status}: {body}")]
    AzureRejected { status: StatusCode, body: String },
    #[error("Settings store error: {0}")]
    Settings(String),
}

impl From<AzureUploadError> for String {
    fn from(err: AzureUploadError) -> Self {
        err.to_string()
    }
}

fn normalize_sas(token: &str) -> String {
    token.trim().trim_start_matches('?').to_string()
}

fn blob_base_url(cfg: &AzureStorageConfig, blob_name: &str) -> String {
    format!(
        "https://{}.blob.core.windows.net/{}/{}",
        cfg.account_name, cfg.container_name, blob_name
    )
}

fn public_share_url(cfg: &AzureStorageConfig, blob_name: &str) -> String {
    blob_base_url(cfg, blob_name)
}

fn block_id_for(index: u32) -> String {
    let raw = format!("cap-block-{:08}", index);
    BASE64.encode(raw.as_bytes())
}

#[instrument(skip(app, channel, file_path))]
pub async fn upload_video_to_azure(
    app: &AppHandle,
    video_id: String,
    file_path: PathBuf,
    channel: Option<Channel<UploadProgress>>,
) -> Result<AzureUploadedItem, AzureUploadError> {
    let cfg = GeneralSettingsStore::get(app)
        .map_err(AzureUploadError::Settings)?
        .map(|s| s.azure_storage)
        .unwrap_or_default();

    if !cfg.is_active() {
        return Err(AzureUploadError::NotConfigured);
    }

    if !file_path.exists() {
        return Err(AzureUploadError::MissingFile(file_path));
    }

    let blob_name = format!("{}.mp4", video_id);
    let total_size = tokio::fs::metadata(&file_path).await?.len();
    let content_type = guess_content_type(&file_path);

    info!(
        "Uploading {} ({} bytes) to azure blob {}/{}",
        file_path.display(),
        total_size,
        cfg.container_name,
        blob_name
    );

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;

    let block_ids = upload_blocks(
        &client,
        &cfg,
        &blob_name,
        &file_path,
        total_size,
        channel.as_ref(),
    )
    .await?;

    commit_block_list(&client, &cfg, &blob_name, &block_ids, &content_type).await?;

    if let Some(channel) = channel {
        channel.send(UploadProgress { progress: 1.0 }).ok();
    }

    let link = public_share_url(&cfg, &blob_name);
    Ok(AzureUploadedItem { link, id: video_id })
}

fn guess_content_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp4") => "video/mp4".to_string(),
        Some("mov") => "video/quicktime".to_string(),
        Some("webm") => "video/webm".to_string(),
        Some("mkv") => "video/x-matroska".to_string(),
        Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
        Some("png") => "image/png".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

async fn upload_blocks(
    client: &reqwest::Client,
    cfg: &AzureStorageConfig,
    blob_name: &str,
    file_path: &Path,
    total_size: u64,
    channel: Option<&Channel<UploadProgress>>,
) -> Result<Vec<String>, AzureUploadError> {
    let file = File::open(file_path).await?;
    let mut reader = BufReader::new(file);
    let mut buffer = vec![0u8; BLOCK_SIZE];
    let mut block_ids: Vec<String> = Vec::new();
    let mut uploaded: u64 = 0;
    let mut index: u32 = 0;

    loop {
        let mut filled = 0usize;
        while filled < BLOCK_SIZE {
            let n = reader.read(&mut buffer[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }

        if filled == 0 {
            break;
        }

        let block_id = block_id_for(index);
        let chunk = Bytes::copy_from_slice(&buffer[..filled]);
        put_block_with_retry(client, cfg, blob_name, &block_id, chunk).await?;
        block_ids.push(block_id);

        uploaded = uploaded.saturating_add(filled as u64);
        index = index.saturating_add(1);

        if let Some(channel) = channel
            && total_size > 0
        {
            let progress = (uploaded as f64 / total_size as f64).clamp(0.0, 0.999);
            channel.send(UploadProgress { progress }).ok();
        }

        if filled < BLOCK_SIZE {
            break;
        }
    }

    if block_ids.is_empty() {
        return Err(AzureUploadError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file is empty",
        )));
    }

    Ok(block_ids)
}

async fn put_block_with_retry(
    client: &reqwest::Client,
    cfg: &AzureStorageConfig,
    blob_name: &str,
    block_id: &str,
    chunk: Bytes,
) -> Result<(), AzureUploadError> {
    let url = format!(
        "{}?comp=block&blockid={}&{}",
        blob_base_url(cfg, blob_name),
        urlencoding::encode(block_id),
        normalize_sas(&cfg.sas_token)
    );

    let mut last_err: Option<AzureUploadError> = None;
    for attempt in 0..MAX_RETRIES {
        let response = client
            .put(&url)
            .header("x-ms-version", AZURE_API_VERSION)
            .header("Content-Length", chunk.len())
            .body(chunk.clone())
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                warn!(
                    "put_block attempt {} failed: {} {}",
                    attempt + 1,
                    status,
                    body.chars().take(200).collect::<String>()
                );
                last_err = Some(AzureUploadError::AzureRejected { status, body });
                if !is_retryable(status) {
                    break;
                }
            }
            Err(err) => {
                warn!("put_block attempt {} network error: {}", attempt + 1, err);
                last_err = Some(AzureUploadError::Http(err));
            }
        }

        tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
    }

    Err(last_err.unwrap_or_else(|| AzureUploadError::AzureRejected {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: "unknown failure uploading block".to_string(),
    }))
}

fn is_retryable(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
}

async fn commit_block_list(
    client: &reqwest::Client,
    cfg: &AzureStorageConfig,
    blob_name: &str,
    block_ids: &[String],
    content_type: &str,
) -> Result<(), AzureUploadError> {
    let url = format!(
        "{}?comp=blocklist&{}",
        blob_base_url(cfg, blob_name),
        normalize_sas(&cfg.sas_token)
    );

    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
    for id in block_ids {
        body.push_str("<Latest>");
        body.push_str(&xml_escape(id));
        body.push_str("</Latest>");
    }
    body.push_str("</BlockList>");

    let response = client
        .put(&url)
        .header("x-ms-version", AZURE_API_VERSION)
        .header("x-ms-blob-content-type", content_type)
        .header("Content-Type", "application/xml; charset=UTF-8")
        .body(body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        error!("commit_block_list failed: {} {}", status, body);
        return Err(AzureUploadError::AzureRejected { status, body });
    }

    debug!("committed {} blocks to {}", block_ids.len(), blob_name);
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[tauri::command]
#[specta::specta]
pub async fn azure_test_connection(app: AppHandle) -> Result<(), String> {
    let cfg = GeneralSettingsStore::get(&app)
        .map_err(|e| e.to_string())?
        .map(|s| s.azure_storage)
        .unwrap_or_default();

    if !cfg.is_active() {
        return Err(
            "Azure storage not fully configured. Account name, container, and SAS token are required."
                .to_string(),
        );
    }

    let probe_blob = format!("cap-test-{}.txt", Uuid::new_v4());
    let url = format!(
        "{}?{}",
        blob_base_url(&cfg, &probe_blob),
        normalize_sas(&cfg.sas_token)
    );
    let body = format!("cap-azure-connection-test:{}", probe_blob);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let put = client
        .put(&url)
        .header("x-ms-version", AZURE_API_VERSION)
        .header("x-ms-blob-type", "BlockBlob")
        .header("Content-Type", "text/plain")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("Network error reaching Azure: {e}"))?;

    if !put.status().is_success() {
        let status = put.status();
        let detail = put.text().await.unwrap_or_default();
        return Err(format!(
            "Azure rejected write probe ({status}). Check SAS perms (need rcw at minimum) and container name. Detail: {}",
            detail.chars().take(300).collect::<String>()
        ));
    }

    let del = client
        .delete(&url)
        .header("x-ms-version", AZURE_API_VERSION)
        .send()
        .await
        .map_err(|e| format!("Network error during cleanup: {e}"))?;

    if !del.status().is_success() && del.status() != StatusCode::NOT_FOUND {
        warn!(
            "Could not delete probe blob {}: {} (SAS may lack delete perm; not fatal)",
            probe_blob,
            del.status()
        );
    }

    Ok(())
}
