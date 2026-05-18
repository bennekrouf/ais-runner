/// Direct HTTP client for the local Azurite blob-storage emulator.
///
/// Uses the Azure Blob Storage REST API with Shared Key auth and a pinned
/// API version that Azurite supports — completely independent of the az CLI.
///
/// Well-known Azurite development-storage credentials (public, safe to embed).
/// Key taken directly from Azurite source (common/utils/constants.js — updated in v3.x):
///   account  : devstoreaccount1
///   key (b64): Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use sha2::Sha256;
use std::sync::OnceLock;

use crate::services::azure_cli::BlobInfo;

type HmacSha256 = Hmac<Sha256>;

const ACCOUNT:       &str = "devstoreaccount1";
const ACCOUNT_KEY:   &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const BLOB_ENDPOINT: &str = "http://127.0.0.1:10000/devstoreaccount1";
/// Pinned to a version that every recent Azurite release supports.
const API_VERSION:   &str = "2021-08-06";

// ── Shared Key auth ───────────────────────────────────────────────────────────

fn make_date() -> String {
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Build the `Authorization: SharedKey …` header value.
///
/// `x_ms_extras` is any additional `x-ms-*` headers beyond date/version,
/// provided pre-lowercased and sorted, e.g. `&[("x-ms-blob-type", "BlockBlob")]`.
fn auth_header(
    method:        &str,
    content_type:  &str,
    content_length: Option<u64>,
    date:          &str,
    resource_path: &str,           // path after /devstoreaccount1, e.g. "/my-container"
    query_pairs:   &[(&str, &str)],// query params (will be sorted)
    x_ms_extras:   &[(&str, &str)],// extra x-ms-* headers (sorted)
) -> String {
    // ── Canonicalized headers ───────────────────────────────────────────────
    // All x-ms-* headers, lower-case, alphabetical order, each "name:value\n".
    // We always have date + version; extras are inserted in sorted order.
    let mut hdrs: Vec<(String, String)> = x_ms_extras
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    hdrs.push(("x-ms-date".into(), date.to_string()));
    hdrs.push(("x-ms-version".into(), API_VERSION.to_string()));
    hdrs.sort_by(|a, b| a.0.cmp(&b.0));
    let canon_hdrs: String = hdrs
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect();

    // ── Canonicalized resource ──────────────────────────────────────────────
    // Azurite uses path-based routing (host:10000/devstoreaccount1/…), so the
    // full URL path already starts with /devstoreaccount1.  The canonical
    // resource is /{account}{full-url-path} = /{account}/{account}{resource}.
    let base = format!("/{}/{}{}", ACCOUNT, ACCOUNT, resource_path);
    let mut sorted_qp: Vec<(&str, &str)> = query_pairs.to_vec();
    sorted_qp.sort_by_key(|(k, _)| *k);
    let canon_resource = if sorted_qp.is_empty() {
        base
    } else {
        let params: String = sorted_qp
            .iter()
            .map(|(k, v)| format!("\n{}:{}", k, v))
            .collect();
        format!("{}{}", base, params)
    };

    // ── String to sign (Shared Key, Blob service) ───────────────────────────
    // Positions: Verb, CE, CL(lang), CL(len), CMD5, CT, Date, IMS, IM, INM, IUM, Range
    let cl = match content_length {
        Some(n) if n > 0 => n.to_string(),
        _ => String::new(),
    };
    let string_to_sign = format!(
        "{}\n\n\n{}\n\n{}\n\n\n\n\n\n\n{}{}",
        method, cl, content_type, canon_hdrs, canon_resource
    );

    // ── HMAC-SHA256 ─────────────────────────────────────────────────────────
    let key_bytes = B64.decode(ACCOUNT_KEY).expect("static key");
    let mut mac = HmacSha256::new_from_slice(&key_bytes).expect("valid key");
    mac.update(string_to_sign.as_bytes());
    let sig = B64.encode(mac.finalize().into_bytes());

    format!("SharedKey {}:{}", ACCOUNT, sig)
}

/// Returns the shared blocking HTTP client.
///
/// `reqwest::blocking::Client` is expensive to construct — it spins up an
/// internal tokio runtime with its own thread pool.  Creating one per call
/// (the previous behaviour) caused the process to exhaust OS thread/fd limits
/// when the blob-watcher loop ran at full speed.  A single static instance is
/// safe: the client is `Send + Sync` and connection-pools automatically.
fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build azurite HTTP client")
    })
}

// ── Public API ────────────────────────────────────────────────────────────────

/// List all container names.
pub fn list_containers() -> Result<Vec<String>, String> {
    let date = make_date();
    let auth = auth_header("GET", "", None, &date, "", &[("comp", "list")], &[]);
    let url = format!("{}?comp=list", BLOB_ENDPOINT);

    let resp = client()
        .get(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("Authorization", auth)
        .send()
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, extract_error_message(&body)));
    }

    // Parse <Container><Name>...</Name>
    let re = regex::Regex::new(r"<Container><Name>([^<]+)</Name>").unwrap();
    Ok(re.captures_iter(&body).map(|c| c[1].to_string()).collect())
}

/// List blobs inside a container (name + size).
pub fn list_blobs(container: &str) -> Result<Vec<BlobInfo>, String> {
    let date  = make_date();
    let path  = format!("/{}", container);
    let query = &[("comp", "list"), ("restype", "container")];
    let auth  = auth_header("GET", "", None, &date, &path, query, &[]);
    let url   = format!("{}/{}?comp=list&restype=container", BLOB_ENDPOINT, container);

    let resp = client()
        .get(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("Authorization", auth)
        .send()
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, extract_error_message(&body)));
    }

    // Each blob: <Blob><Name>…</Name><Properties>…<Content-Length>…</Content-Length>
    let re = regex::Regex::new(
        r"<Blob><Name>([^<]+)</Name>.*?<Content-Length>(\d+)</Content-Length>"
    ).unwrap();
    Ok(re.captures_iter(&body)
        .map(|c| BlobInfo {
            name: c[1].to_string(),
            size: c[2].parse().unwrap_or(0),
        })
        .collect())
}

/// Create a container (idempotent — 409 Conflict is treated as success).
pub fn create_container(name: &str) -> Result<(), String> {
    let date  = make_date();
    let path  = format!("/{}", name);
    let query = &[("restype", "container")];
    let auth  = auth_header("PUT", "", None, &date, &path, query, &[]);
    let url   = format!("{}/{}?restype=container", BLOB_ENDPOINT, name);

    let resp = client()
        .put(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("Authorization", auth)
        .header("Content-Length", "0")
        .send()
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if status.is_success() || status.as_u16() == 409 {
        return Ok(());
    }
    let body = resp.text().unwrap_or_default();
    Err(format!("HTTP {}: {}", status, extract_error_message(&body)))
}

/// Create a virtual folder inside a container by uploading a zero-byte `.keep` blob.
///
/// Azure Blob Storage has no real directories; a folder is just a naming convention.
/// This creates `{prefix}/.keep` so the path appears in listings immediately.
pub fn create_virtual_folder(container: &str, prefix: &str) -> Result<(), String> {
    // Normalise: strip trailing slash, add /.keep
    let trimmed = prefix.trim_end_matches('/');
    let blob_name = format!("{}/.keep", trimmed);
    let date         = make_date();
    let path         = format!("/{}/{}", container, blob_name);
    let content_type = "application/octet-stream";
    let extras       = &[("x-ms-blob-type", "BlockBlob")];
    let auth = auth_header("PUT", content_type, Some(0), &date, &path, &[], extras);
    let url  = format!("{}/{}/{}", BLOB_ENDPOINT, container, percent_encode(&blob_name));

    let resp = client()
        .put(&url)
        .header("x-ms-date",      &date)
        .header("x-ms-version",   API_VERSION)
        .header("x-ms-blob-type", "BlockBlob")
        .header("Authorization",  auth)
        .header("Content-Type",   content_type)
        .header("Content-Length", "0")
        .body(vec![])
        .send()
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if status.is_success() { return Ok(()); }
    let body = resp.text().unwrap_or_default();
    Err(format!("HTTP {}: {}", status, extract_error_message(&body)))
}

/// Delete all blobs in a container. Returns number of blobs deleted.
pub fn clear_container(container: &str) -> Result<u64, String> {
    let blobs = list_blobs(container)?;
    let n     = blobs.len() as u64;
    for blob in blobs {
        delete_blob(container, &blob.name)?;
    }
    Ok(n)
}

/// Download a blob and save it to a local file path.
pub fn download_blob(container: &str, blob_name: &str, dest_path: &str) -> Result<(), String> {
    let date = make_date();
    let path = format!("/{}/{}", container, blob_name);
    let auth = auth_header("GET", "", None, &date, &path, &[], &[]);
    let url  = format!("{}/{}/{}", BLOB_ENDPOINT, container, percent_encode(blob_name));

    let resp = client()
        .get(&url)
        .header("x-ms-date",     &date)
        .header("x-ms-version",  API_VERSION)
        .header("Authorization", auth)
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, extract_error_message(&body)));
    }
    let bytes = resp.bytes().map_err(|e| e.to_string())?;
    std::fs::write(dest_path, &bytes).map_err(|e| e.to_string())
}

/// Upload a local file as a BlockBlob (overwrites if it already exists).
pub fn upload_blob(container: &str, file_path: &str, blob_name: &str) -> Result<(), String> {
    let data = std::fs::read(file_path).map_err(|e| e.to_string())?;
    upload_blob_bytes_sync(container, blob_name, data)
}

/// Upload raw bytes as a BlockBlob. Used by the async trigger path.
pub fn upload_blob_bytes_sync(container: &str, blob_name: &str, data: Vec<u8>) -> Result<(), String> {
    let content_length = data.len() as u64;
    let content_type   = "application/octet-stream";
    let date           = make_date();
    let path           = format!("/{}/{}", container, blob_name);
    let extras         = &[("x-ms-blob-type", "BlockBlob")];
    let auth = auth_header(
        "PUT", content_type, Some(content_length), &date, &path, &[], extras,
    );
    let url = format!("{}/{}/{}", BLOB_ENDPOINT, container, percent_encode(blob_name));

    let resp = client()
        .put(&url)
        .header("x-ms-date",       &date)
        .header("x-ms-version",    API_VERSION)
        .header("x-ms-blob-type",  "BlockBlob")
        .header("Authorization",   auth)
        .header("Content-Type",    content_type)
        .header("Content-Length",  content_length)
        .body(data)
        .send()
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if status.is_success() { return Ok(()); }
    let body = resp.text().unwrap_or_default();
    Err(format!("HTTP {}: {}", status, extract_error_message(&body)))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn delete_blob(container: &str, blob_name: &str) -> Result<(), String> {
    let date = make_date();
    let path = format!("/{}/{}", container, blob_name);
    let auth = auth_header("DELETE", "", None, &date, &path, &[], &[]);
    let url  = format!("{}/{}/{}", BLOB_ENDPOINT, container, percent_encode(blob_name));

    let resp = client()
        .delete(&url)
        .header("x-ms-date",     &date)
        .header("x-ms-version",  API_VERSION)
        .header("Authorization", auth)
        .send()
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if status.is_success() || status.as_u16() == 404 { return Ok(()); }
    let body = resp.text().unwrap_or_default();
    Err(format!("Delete '{}': HTTP {}: {}", blob_name, status, extract_error_message(&body)))
}

/// Minimal percent-encoding: encodes only characters that break URL paths.
fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9'
            | '-' | '_' | '.' | '~' | '/' => c.to_string(),
            ' ' => "%20".to_string(),
            c   => format!("%{:02X}", c as u32),
        })
        .collect()
}

/// Extract the `<Message>` text from an Azure XML error response, or return the raw body.
fn extract_error_message(body: &str) -> String {
    let re = regex::Regex::new(r"<Message>([^<]+)</Message>").unwrap();
    re.captures(body)
        .map(|c| c[1].trim().to_string())
        .unwrap_or_else(|| body.chars().take(200).collect())
}
