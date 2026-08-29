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

/// reqwest reports a dead emulator as "error sending request for url (...)",
/// which names neither the cause nor the fix. Azurite dying mid-run is common
/// enough that the bare message costs real debugging time.
fn transport_error(e: reqwest::Error) -> String {
    if e.is_connect() || e.is_timeout() {
        format!(
            "Azurite is not responding on 127.0.0.1:10000 (it was reachable earlier if \
             previous steps passed, so it stopped mid-run) — {}",
            crate::services::workflows::AZURITE_RESET_HINT
        )
    } else {
        e.to_string()
    }
}
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use sha2::Sha256;
use std::sync::OnceLock;

/// One blob as listed from a container: just the name and the size the
/// browser shows. Lives here rather than in the `az` wrapper because
/// Azurite is local — nothing in `services::azure` produces one.
#[derive(Debug, Clone)]
pub struct BlobInfo {
    pub name: String,
    pub size: u64,
}

type HmacSha256 = Hmac<Sha256>;

const ACCOUNT: &str = "devstoreaccount1";
const ACCOUNT_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const BLOB_ENDPOINT: &str = "http://127.0.0.1:10000/devstoreaccount1";
/// Pinned to a version that every recent Azurite release supports.
const API_VERSION: &str = "2021-08-06";

// ── Shared Key auth ───────────────────────────────────────────────────────────

fn make_date() -> String {
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Build the `Authorization: SharedKey …` header value.
///
/// `x_ms_extras` is any additional `x-ms-*` headers beyond date/version,
/// provided pre-lowercased and sorted, e.g. `&[("x-ms-blob-type", "BlockBlob")]`.
fn auth_header(
    method: &str,
    content_type: &str,
    content_length: Option<u64>,
    date: &str,
    resource_path: &str, // path after /devstoreaccount1, e.g. "/my-container"
    query_pairs: &[(&str, &str)], // query params (will be sorted)
    x_ms_extras: &[(&str, &str)], // extra x-ms-* headers (sorted)
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
    let canon_hdrs: String = hdrs.iter().map(|(k, v)| format!("{}:{}\n", k, v)).collect();

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
        .map_err(transport_error)?;

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
    let date = make_date();
    let path = format!("/{}", container);
    let query = &[("comp", "list"), ("restype", "container")];
    let auth = auth_header("GET", "", None, &date, &path, query, &[]);
    let url = format!(
        "{}/{}?comp=list&restype=container",
        BLOB_ENDPOINT, container
    );

    let resp = client()
        .get(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("Authorization", auth)
        .send()
        .map_err(transport_error)?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, extract_error_message(&body)));
    }

    // Each blob: <Blob><Name>…</Name><Properties>…<Content-Length>…</Content-Length>
    let re =
        regex::Regex::new(r"<Blob><Name>([^<]+)</Name>.*?<Content-Length>(\d+)</Content-Length>")
            .unwrap();
    Ok(re
        .captures_iter(&body)
        .map(|c| BlobInfo {
            name: c[1].to_string(),
            size: c[2].parse().unwrap_or(0),
        })
        .collect())
}

/// A blob-safe container name derived from `name`: lowercased, every run of
/// invalid characters (dots, underscores, spaces, …) collapsed to a single
/// hyphen, leading/trailing hyphens trimmed. `ais.ignite.kyriba.payment`
/// → `ais-ignite-kyriba-payment`.
pub fn suggest_container_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Validate a container name against Azure's rules, returning a human-readable
/// reason if invalid. Rules: 3–63 chars, lowercase letters/digits/hyphens only,
/// must start and end with a letter or digit, no consecutive hyphens.
/// (Names like `ais.ignite.kyriba.payment` fail on the dots — that's a Service
/// Bus entity name, not a blob container.)
pub fn validate_container_name(name: &str) -> Result<(), String> {
    let hint = || {
        let s = suggest_container_name(name);
        if s.len() >= 3 {
            format!(" Try '{s}'.")
        } else {
            String::new()
        }
    };
    if name.len() < 3 || name.len() > 63 {
        return Err(format!("Container name must be 3–63 characters.{}", hint()));
    }
    if name.contains('.') {
        return Err(format!(
            "Container names can't contain dots — only lowercase letters, digits, and hyphens.{}",
            hint()
        ));
    }
    if name
        .chars()
        .any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    {
        return Err(format!(
            "Container names allow only lowercase letters, digits, and hyphens.{}",
            hint()
        ));
    }
    let first = name.chars().next().unwrap();
    let last = name.chars().last().unwrap();
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return Err(format!(
            "Container name must start and end with a letter or digit.{}",
            hint()
        ));
    }
    if name.contains("--") {
        return Err(format!(
            "Container name can't contain consecutive hyphens.{}",
            hint()
        ));
    }
    Ok(())
}

/// Create a container (idempotent — 409 Conflict is treated as success).
pub fn create_container(name: &str) -> Result<(), String> {
    // Validate locally first so the user gets an actionable message with a
    // suggested name, not Azurite's opaque "invalid characters" 400.
    validate_container_name(name)?;

    let date = make_date();
    let path = format!("/{}", name);
    let query = &[("restype", "container")];
    let auth = auth_header("PUT", "", None, &date, &path, query, &[]);
    let url = format!("{}/{}?restype=container", BLOB_ENDPOINT, name);

    let resp = client()
        .put(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("Authorization", auth)
        .header("Content-Length", "0")
        .send()
        .map_err(transport_error)?;

    let status = resp.status();
    if status.is_success() || status.as_u16() == 409 {
        return Ok(());
    }
    let body = resp.text().unwrap_or_default();
    Err(format!("HTTP {}: {}", status, extract_error_message(&body)))
}

/// Destination name when moving `name` from under `old_prefix/` to `new_prefix/`.
/// Returns `None` when `name` is not inside `old_prefix/`.
///
/// Prefixes are compared with a trailing slash so renaming `pay` never captures
/// a sibling folder like `payments`.
fn rewrite_prefix(name: &str, old_prefix: &str, new_prefix: &str) -> Option<String> {
    let old = format!("{}/", old_prefix.trim_end_matches('/'));
    let new = format!("{}/", new_prefix.trim_end_matches('/'));
    name.strip_prefix(old.as_str())
        .map(|rest| format!("{new}{rest}"))
}

/// Server-side `Copy Blob` within the same account — the payload never passes
/// through this process. Azurite completes same-account copies synchronously.
fn copy_blob(container: &str, src: &str, dst: &str) -> Result<(), String> {
    let date = make_date();
    // Sign the same encoded paths we send (see delete_blob).
    let src_enc = percent_encode(src);
    let dst_enc = percent_encode(dst);
    let source = format!("{}/{}/{}", BLOB_ENDPOINT, container, src_enc);
    let path = format!("/{}/{}", container, dst_enc);
    let extras: &[(&str, &str)] = &[("x-ms-copy-source", source.as_str())];
    let auth = auth_header("PUT", "", None, &date, &path, &[], extras);
    let url = format!("{}/{}/{}", BLOB_ENDPOINT, container, dst_enc);

    let resp = client()
        .put(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("x-ms-copy-source", &source)
        .header("Authorization", auth)
        .header("Content-Length", "0")
        .send()
        .map_err(transport_error)?;

    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    Err(format!(
        "Copy '{src}' → '{dst}': HTTP {status}: {}",
        extract_error_message(&body)
    ))
}

/// Rename a virtual folder by moving every blob under `old_prefix/` to
/// `new_prefix/`. Returns the number of blobs moved.
///
/// Blob storage has no real folders — a "folder" is just a shared name prefix —
/// so this is a copy-then-delete per blob and is **not atomic**. Each original
/// is deleted only after its copy succeeded, so a failure mid-way leaves data
/// duplicated, never lost.
///
/// Refuses to overwrite: if any destination name already exists the whole
/// rename is rejected before anything is written.
pub fn rename_virtual_folder(
    container: &str,
    old_prefix: &str,
    new_prefix: &str,
) -> Result<u64, String> {
    let old = old_prefix.trim().trim_end_matches('/');
    let new = new_prefix.trim().trim_end_matches('/');
    if new.is_empty() {
        return Err("New folder name is empty".into());
    }
    if old == new {
        return Ok(0);
    }
    if new.contains("//") {
        return Err(format!("Invalid folder name '{new}'"));
    }
    // Renaming a folder into its own subtree would recurse into what we create.
    if new.starts_with(&format!("{old}/")) {
        return Err(format!("Cannot move '{old}' inside itself ('{new}')"));
    }

    let all = list_blobs(container)?;
    let existing: std::collections::HashSet<&str> = all.iter().map(|b| b.name.as_str()).collect();

    let moves: Vec<(String, String)> = all
        .iter()
        .filter_map(|b| rewrite_prefix(&b.name, old, new).map(|dst| (b.name.clone(), dst)))
        .collect();

    if moves.is_empty() {
        return Err(format!("No folder '{old}/' in container '{container}'"));
    }
    if let Some((_, dst)) = moves.iter().find(|(_, d)| existing.contains(d.as_str())) {
        return Err(format!(
            "Destination '{dst}' already exists — rename aborted"
        ));
    }

    let mut moved = 0u64;
    for (src, dst) in &moves {
        // Copy first; only drop the original once the copy is safely in place.
        copy_blob(container, src, dst)?;
        delete_blob(container, src)?;
        moved += 1;
    }
    Ok(moved)
}

/// Delete all blobs in a container. Returns the number deleted.
///
/// Deliberately does NOT abort on the first failure: one undeletable blob used
/// to leave every remaining blob in place. We delete everything we can and
/// report a partial failure so the caller can surface it.
pub fn clear_container(container: &str) -> Result<u64, String> {
    let blobs = list_blobs(container)?;
    let total = blobs.len() as u64;
    let mut deleted = 0u64;
    let mut first_err: Option<String> = None;

    for blob in blobs {
        match delete_blob(container, &blob.name) {
            Ok(()) => deleted += 1,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e)
                }
            }
        }
    }

    match first_err {
        None => Ok(deleted),
        Some(e) => Err(format!(
            "Deleted {deleted}/{total} — {} failed. First error: {e}",
            total - deleted
        )),
    }
}

/// Download a blob and save it to a local file path.
pub fn download_blob(container: &str, blob_name: &str, dest_path: &str) -> Result<(), String> {
    let date = make_date();
    // Sign the encoded path — see delete_blob for why.
    let enc = percent_encode(blob_name);
    let path = format!("/{}/{}", container, enc);
    let auth = auth_header("GET", "", None, &date, &path, &[], &[]);
    let url = format!("{}/{}/{}", BLOB_ENDPOINT, container, enc);

    let resp = client()
        .get(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("Authorization", auth)
        .send()
        .map_err(transport_error)?;

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
pub fn upload_blob_bytes_sync(
    container: &str,
    blob_name: &str,
    data: Vec<u8>,
) -> Result<(), String> {
    let content_length = data.len() as u64;
    let content_type = "application/octet-stream";
    let date = make_date();
    // Sign the encoded path — see delete_blob for why.
    let enc = percent_encode(blob_name);
    let path = format!("/{}/{}", container, enc);
    let extras = &[("x-ms-blob-type", "BlockBlob")];
    let auth = auth_header(
        "PUT",
        content_type,
        Some(content_length),
        &date,
        &path,
        &[],
        extras,
    );
    let url = format!("{}/{}/{}", BLOB_ENDPOINT, container, enc);

    let resp = client()
        .put(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("x-ms-blob-type", "BlockBlob")
        .header("Authorization", auth)
        .header("Content-Type", content_type)
        .header("Content-Length", content_length)
        .body(data)
        .send()
        .map_err(transport_error)?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().unwrap_or_default();
    Err(format!("HTTP {}: {}", status, extract_error_message(&body)))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn delete_blob(container: &str, blob_name: &str) -> Result<(), String> {
    let date = make_date();
    // Sign the SAME percent-encoded path we put in the URL. Azurite derives the
    // canonicalized resource from the request path as received, so signing the
    // raw name while sending an encoded one is a signature mismatch → HTTP 403.
    // Bites any blob whose name contains ':' etc. (e.g. ISO-timestamp prefixes).
    let enc = percent_encode(blob_name);
    let path = format!("/{}/{}", container, enc);
    let auth = auth_header("DELETE", "", None, &date, &path, &[], &[]);
    let url = format!("{}/{}/{}", BLOB_ENDPOINT, container, enc);

    let resp = client()
        .delete(&url)
        .header("x-ms-date", &date)
        .header("x-ms-version", API_VERSION)
        .header("Authorization", auth)
        .send()
        .map_err(transport_error)?;

    let status = resp.status();
    if status.is_success() || status.as_u16() == 404 {
        return Ok(());
    }
    let body = resp.text().unwrap_or_default();
    Err(format!(
        "Delete '{}': HTTP {}: {}",
        blob_name,
        status,
        extract_error_message(&body)
    ))
}

/// Minimal percent-encoding: encodes only characters that break URL paths.
fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => c.to_string(),
            ' ' => "%20".to_string(),
            c => format!("%{:02X}", c as u32),
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

#[cfg(test)]
mod rename_folder_tests {
    use super::rewrite_prefix;

    #[test]
    fn moves_blobs_under_the_prefix() {
        assert_eq!(
            rewrite_prefix("payments/a.csv", "payments", "pay").as_deref(),
            Some("pay/a.csv")
        );
        // nested paths keep their sub-structure
        assert_eq!(
            rewrite_prefix("payments/2026/07/a.csv", "payments", "pay").as_deref(),
            Some("pay/2026/07/a.csv")
        );
        // the .keep folder marker moves too, so the folder still shows up
        assert_eq!(
            rewrite_prefix("payments/.keep", "payments", "pay").as_deref(),
            Some("pay/.keep")
        );
    }

    #[test]
    fn does_not_capture_sibling_folders_sharing_a_stem() {
        // renaming "pay" must NOT sweep up "payments/…"
        assert_eq!(rewrite_prefix("payments/a.csv", "pay", "x"), None);
        assert_eq!(
            rewrite_prefix("pay/a.csv", "pay", "x").as_deref(),
            Some("x/a.csv")
        );
    }

    #[test]
    fn ignores_blobs_outside_the_folder() {
        assert_eq!(rewrite_prefix("root.csv", "payments", "pay"), None);
        assert_eq!(rewrite_prefix("other/a.csv", "payments", "pay"), None);
    }

    #[test]
    fn trailing_slashes_are_tolerated_on_both_sides() {
        assert_eq!(
            rewrite_prefix("payments/a.csv", "payments/", "pay/").as_deref(),
            Some("pay/a.csv")
        );
    }

    #[test]
    fn renaming_into_a_deeper_path_works() {
        assert_eq!(
            rewrite_prefix("payments/a.csv", "payments", "archive/payments").as_deref(),
            Some("archive/payments/a.csv")
        );
    }
}

#[cfg(test)]
mod rename_folder_live_tests {
    use super::*;

    /// End-to-end against a running Azurite. Ignored by default so the normal
    /// suite stays hermetic: run with
    ///   cargo test rename_folder_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn rename_folder_round_trip() {
        const C: &str = "ais-rename-selftest";
        create_container(C).expect("create container");
        let _ = clear_container(C); // start clean

        upload_blob_bytes_sync(C, "oldfolder/.keep", vec![]).expect("keep");
        upload_blob_bytes_sync(C, "oldfolder/a.txt", b"hello".to_vec()).expect("a");
        upload_blob_bytes_sync(C, "oldfolder/sub/b.txt", b"world".to_vec()).expect("b");
        // a sibling sharing the stem must NOT be swept up
        upload_blob_bytes_sync(C, "oldfolderX/c.txt", b"keepme".to_vec()).expect("c");

        let moved = rename_virtual_folder(C, "oldfolder", "newfolder").expect("rename");
        assert_eq!(moved, 3, "should move exactly the 3 blobs under oldfolder/");

        let names: Vec<String> = list_blobs(C).unwrap().into_iter().map(|b| b.name).collect();
        assert!(names.contains(&"newfolder/.keep".to_string()));
        assert!(names.contains(&"newfolder/a.txt".to_string()));
        assert!(
            names.contains(&"newfolder/sub/b.txt".to_string()),
            "nested path preserved"
        );
        assert!(
            names.contains(&"oldfolderX/c.txt".to_string()),
            "sibling untouched"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("oldfolder/")),
            "originals removed"
        );

        // content survived the server-side copy
        let tmp = std::env::temp_dir().join("ais-rename-selftest-a.txt");
        download_blob(C, "newfolder/a.txt", tmp.to_str().unwrap()).expect("download");
        assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "hello");
        let _ = std::fs::remove_file(&tmp);

        // refuses to clobber an existing destination
        upload_blob_bytes_sync(C, "dest/x.txt", b"x".to_vec()).unwrap();
        upload_blob_bytes_sync(C, "src/x.txt", b"y".to_vec()).unwrap();
        let err = rename_virtual_folder(C, "src", "dest").unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
        // nothing was moved on rejection
        let names2: Vec<String> = list_blobs(C).unwrap().into_iter().map(|b| b.name).collect();
        assert!(names2.contains(&"src/x.txt".to_string()));

        // refuses to move a folder into its own subtree
        let err2 = rename_virtual_folder(C, "newfolder", "newfolder/inner").unwrap_err();
        assert!(err2.contains("inside itself"), "got: {err2}");

        let _ = clear_container(C);
    }
}

#[cfg(test)]
mod container_name_tests {
    use super::{suggest_container_name, validate_container_name};

    #[test]
    fn dotted_name_is_rejected_with_a_suggestion() {
        let err = validate_container_name("ais.ignite.kyriba.payment").unwrap_err();
        assert!(err.contains("dots"), "got: {err}");
        assert!(err.contains("ais-ignite-kyriba-payment"), "got: {err}");
    }

    #[test]
    fn suggestion_is_blob_safe() {
        assert_eq!(
            suggest_container_name("ais.ignite.kyriba.payment"),
            "ais-ignite-kyriba-payment"
        );
        assert_eq!(
            suggest_container_name("My_Container.Name"),
            "my-container-name"
        );
        assert_eq!(suggest_container_name("a...b"), "a-b");
        assert_eq!(suggest_container_name("--lead.trail--"), "lead-trail");
    }

    #[test]
    fn valid_names_pass() {
        assert!(validate_container_name("kyriba-input").is_ok());
        assert!(validate_container_name("ais-ignite-kyriba-payment").is_ok());
        assert!(validate_container_name("abc").is_ok());
        assert!(validate_container_name("a1-b2-c3").is_ok());
    }

    #[test]
    fn other_rule_violations_are_caught() {
        assert!(validate_container_name("ab").is_err()); // too short
        assert!(validate_container_name("UPPER")
            .unwrap_err()
            .contains("lowercase"));
        assert!(validate_container_name("-lead")
            .unwrap_err()
            .contains("start and end"));
        assert!(validate_container_name("a--b")
            .unwrap_err()
            .contains("consecutive"));
    }
}

#[cfg(test)]
mod transport_error_tests {
    /// Azurite is down right now in this environment or it is not — either way
    /// the message for a refused connection must name the cause and the fix,
    /// which the bare reqwest string does not.
    #[test]
    fn a_refused_connection_names_azurite_and_the_fix() {
        // port 9 (discard) is never listening
        let err = reqwest::blocking::Client::new()
            .get("http://127.0.0.1:9/devstoreaccount1/x")
            .timeout(std::time::Duration::from_millis(500))
            .send()
            .unwrap_err();
        let msg = super::transport_error(err);
        assert!(msg.contains("Azurite is not responding"), "{msg}");
        assert!(msg.contains("⟳ Reset"), "{msg}");
    }
}
