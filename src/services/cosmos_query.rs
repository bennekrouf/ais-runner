//! Minimal Cosmos DB SQL-API client used by the Cosmos tab's query console.
//!
//! Talks the REST protocol directly so we don't have to pull in the
//! fast-moving `azure_data_cosmos` crate. Three operations: list databases,
//! list containers, run a SQL query. All requests are signed with a master
//! key using the documented HMAC-SHA256 scheme; we don't support AAD auth
//! because the local emulator (the primary target here) only accepts the
//! well-known master key.

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const API_VERSION: &str = "2018-12-31";

/// Build a reqwest client that accepts the emulator's self-signed cert and
/// uses a tight timeout — query failures should surface fast, not hang the UI.
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

/// Sign a Cosmos REST request per
/// https://learn.microsoft.com/rest/api/cosmos-db/access-control-on-cosmosdb-resources
///
/// `resource_type` is the plural noun ("dbs", "colls", "docs"). `resource_link`
/// is the path *without* a leading slash and *without* a trailing slash —
/// e.g. `""` for the root, `"dbs/mydb"` for a database, `"dbs/mydb/colls/mycoll"`
/// for a container. The date string must be the same RFC-1123 value sent in the
/// `x-ms-date` header.
fn auth_header(
    verb: &str,
    resource_type: &str,
    resource_link: &str,
    date_rfc1123: &str,
    master_key_b64: &str,
) -> Result<String, String> {
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(master_key_b64.trim())
        .map_err(|e| format!("invalid master key (not base64): {e}"))?;

    // StringToSign is fixed-layout — the trailing two newlines are required
    // even though the corresponding header fields are empty for master-key auth.
    let to_sign = format!(
        "{}\n{}\n{}\n{}\n\n",
        verb.to_lowercase(),
        resource_type.to_lowercase(),
        resource_link,
        date_rfc1123.to_lowercase(),
    );

    let mut mac = HmacSha256::new_from_slice(&key_bytes)
        .map_err(|e| format!("hmac init failed: {e}"))?;
    mac.update(to_sign.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    // Cosmos expects the *entire* token string ("type=master&ver=1.0&sig=...")
    // to be URL-encoded before being placed in the Authorization header.
    let token = format!("type=master&ver=1.0&sig={sig}");
    Ok(percent_encode(&token))
}

/// RFC-3986 reserved-and-then-some encoder — matches Cosmos's expectation for
/// the Authorization header. We can't use `url::form_urlencoded` (not a dep),
/// so a small inline encoder is easier than pulling in a crate.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// RFC 1123 in GMT, the format Cosmos demands in `x-ms-date`.
fn now_rfc1123() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn normalize_endpoint(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_string()
}

/// Send a Cosmos request and try the http:// variant if TLS fails — same trick
/// `cosmos_check::test_cosmos_endpoint` uses for the Linux vnext emulator.
async fn send_with_http_fallback(
    client: &reqwest::Client,
    method: reqwest::Method,
    endpoint: &str,
    path: &str,
    headers: Vec<(&'static str, String)>,
    body: Option<String>,
) -> Result<reqwest::Response, String> {
    let try_send = |url: String| {
        let mut req = client.request(method.clone(), &url);
        for (k, v) in &headers {
            req = req.header(*k, v);
        }
        if let Some(b) = &body {
            req = req.body(b.clone());
        }
        req.send()
    };

    let primary = format!("{}{}", endpoint, path);
    match try_send(primary).await {
        Ok(r) => Ok(r),
        Err(ref e) if e.to_string().contains("wrong version number")
                   || e.to_string().to_lowercase().contains("tls")
                   || e.to_string().to_lowercase().contains("ssl") =>
        {
            let http = format!("{}{}", endpoint.replacen("https://", "http://", 1), path);
            try_send(http).await.map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// `GET /dbs` — returns the list of database IDs visible to the account.
pub async fn list_databases(endpoint: &str, key: &str) -> Result<Vec<String>, String> {
    let endpoint = normalize_endpoint(endpoint);
    let client = client()?;
    let date = now_rfc1123();
    let auth = auth_header("GET", "dbs", "", &date, key)?;

    let resp = send_with_http_fallback(
        &client,
        reqwest::Method::GET,
        &endpoint,
        "/dbs",
        vec![
            ("authorization",  auth),
            ("x-ms-date",      date),
            ("x-ms-version",   API_VERSION.to_string()),
        ],
        None,
    ).await?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(v["Databases"].as_array().map(|a| {
        a.iter().filter_map(|d| d["id"].as_str().map(str::to_string)).collect()
    }).unwrap_or_default())
}

/// `GET /dbs/{db}/colls` — returns the list of container IDs in a database.
pub async fn list_containers(endpoint: &str, key: &str, db: &str) -> Result<Vec<String>, String> {
    let endpoint = normalize_endpoint(endpoint);
    let client = client()?;
    let date = now_rfc1123();
    let link = format!("dbs/{db}");
    let auth = auth_header("GET", "colls", &link, &date, key)?;

    let resp = send_with_http_fallback(
        &client,
        reqwest::Method::GET,
        &endpoint,
        &format!("/dbs/{db}/colls"),
        vec![
            ("authorization", auth),
            ("x-ms-date",     date),
            ("x-ms-version",  API_VERSION.to_string()),
        ],
        None,
    ).await?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(v["DocumentCollections"].as_array().map(|a| {
        a.iter().filter_map(|d| d["id"].as_str().map(str::to_string)).collect()
    }).unwrap_or_default())
}

/// `POST /dbs/{db}/colls/{coll}/docs` with the query payload — returns the
/// raw `Documents` array plus the count, ready for the UI to render.
pub async fn run_query(
    endpoint: &str,
    key: &str,
    db: &str,
    coll: &str,
    query: &str,
) -> Result<serde_json::Value, String> {
    let endpoint = normalize_endpoint(endpoint);
    let client = client()?;
    let date = now_rfc1123();
    let link = format!("dbs/{db}/colls/{coll}");
    let auth = auth_header("POST", "docs", &link, &date, key)?;
    let body = json!({ "query": query, "parameters": [] }).to_string();

    let resp = send_with_http_fallback(
        &client,
        reqwest::Method::POST,
        &endpoint,
        &format!("/dbs/{db}/colls/{coll}/docs"),
        vec![
            ("authorization",                              auth),
            ("x-ms-date",                                  date),
            ("x-ms-version",                               API_VERSION.to_string()),
            ("x-ms-documentdb-isquery",                    "true".to_string()),
            ("x-ms-documentdb-query-enablecrosspartition", "true".to_string()),
            ("x-ms-max-item-count",                        "100".to_string()),
            ("content-type",                               "application/query+json".to_string()),
        ],
        Some(body),
    ).await?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_preserves_unreserved_and_escapes_the_rest() {
        assert_eq!(percent_encode("abcXYZ-_.~"), "abcXYZ-_.~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a+b=c"), "a%2Bb%3Dc");
    }

    #[test]
    fn auth_header_is_deterministic_for_fixed_inputs() {
        // Sanity: same inputs produce the same signature byte-for-byte.
        let key = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let a = auth_header("GET", "dbs", "", "Sun, 01 Jan 2023 00:00:00 GMT", &key).unwrap();
        let b = auth_header("GET", "dbs", "", "Sun, 01 Jan 2023 00:00:00 GMT", &key).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("type%3Dmaster%26ver%3D1.0%26sig%3D"));
    }
}
