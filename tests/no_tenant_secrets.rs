//! The repository must not carry a credential or a tenant's identity.
//!
//! An Azure Service Bus `RootManageSharedAccessKey` for a live namespace sat in
//! `test/logic-apps/local.settings.json` from May to September 2026, in a public
//! repository, because `local.settings.json` was in `.gitignore` *and* already
//! tracked — and gitignore does not apply to a tracked file. Alongside it a real
//! subscription id was compiled into the binary as a default, and one customer's
//! resource names were UI placeholders and match arms.
//!
//! Removing those fixed the instance. This fixes the class: every tracked file
//! is scanned on every `cargo test`, so the next real key or tenant name fails
//! the build instead of reaching a push.
//!
//! Scope is deliberately narrow — credential material, cloud hostnames, and
//! identity GUIDs — so it stays quiet enough that nobody is tempted to delete
//! it. When it fires on something genuinely fine, add a placeholder-shaped name
//! rather than an exception.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Secrets Microsoft publishes. Not credentials: they authenticate to an
/// emulator on localhost and are in the public documentation.
const PUBLIC_EMULATOR_SECRETS: &[&str] = &[
    // Azurite's well-known account key.
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==",
    // Cosmos DB Emulator's well-known key.
    "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==",
    // The Service Bus emulator's documented placeholder.
    "SAS_KEY_VALUE",
];

/// Hostname labels that name nothing real. Checked after stripping a resource
/// prefix, so `sbns-foo` and `func-example` both reduce to a placeholder.
const PLACEHOLDER_LABELS: &[&str] = &[
    "foo",
    "bar",
    "baz",
    "qux",
    "x",
    "y",
    "z",
    "c",
    "xx",
    "xxx",
    "nnn",
    "abc",
    "acct",
    "account",
    "example",
    "sample",
    "placeholder",
    "test",
    "localhost",
    "myaccount",
    "devstoreaccount1",
    "contoso",
    "dummy",
    "fake",
    "name",
    "host",
];

/// Prefixes Azure resource names conventionally carry, stripped before the
/// placeholder check.
const RESOURCE_PREFIXES: &[&str] = &[
    "sbns-", "func-", "kv-", "cosmos-", "rg-", "st-", "logic-", "app-", "api-",
];

/// Real hostnames that belong to Microsoft rather than to a tenant.
const ALLOWED_HOSTS: &[&str] = &["azcliprod.blob.core.windows.net"];

const CLOUD_SUFFIXES: &[&str] = &[
    ".servicebus.windows.net",
    ".vault.azure.net",
    ".documents.azure.com",
    ".blob.core.windows.net",
    ".queue.core.windows.net",
    ".table.core.windows.net",
    ".azurewebsites.net",
    ".azure-api.net",
];

/// Keys whose value is a credential wherever it appears.
const SECRET_ASSIGNMENTS: &[&str] = &["SharedAccessKey=", "AccountKey=", "AccountSecret="];

/// Paths exempt from the scan: this file states the patterns it forbids.
fn is_exempt(path: &str) -> bool {
    path == "tests/no_tenant_secrets.rs"
}

fn is_base64ish(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' || b == b'-' || b == b'_'
}

/// A credential value assigned to one of [`SECRET_ASSIGNMENTS`].
fn credential_in(line: &str) -> Option<String> {
    for marker in SECRET_ASSIGNMENTS {
        let mut from = 0;
        while let Some(rel) = line[from..].find(marker) {
            let start = from + rel + marker.len();
            let end = line[start..]
                .find(|c: char| !is_base64ish(c as u8))
                .map(|o| start + o)
                .unwrap_or(line.len());
            let value = &line[start..end];
            // Short values are placeholders, template variables, or the name of
            // the setting rather than its content.
            if value.len() >= 20 && !PUBLIC_EMULATOR_SECRETS.contains(&value) {
                return Some(format!("{marker}{}…", &value[..8.min(value.len())]));
            }
            from = start.max(from + rel + 1);
        }
    }
    None
}

/// A cloud hostname whose leftmost label names something real.
fn tenant_host_in(line: &str) -> Option<String> {
    for suffix in CLOUD_SUFFIXES {
        let mut from = 0;
        while let Some(rel) = line[from..].find(suffix) {
            let at = from + rel;
            // Walk back over the label.
            let bytes = line.as_bytes();
            let mut start = at;
            while start > 0 && {
                let b = bytes[start - 1];
                b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
            } {
                start -= 1;
            }
            let label = &line[start..at];
            let host = format!("{label}{suffix}");
            if !label.is_empty() && !ALLOWED_HOSTS.contains(&host.as_str()) {
                let bare = RESOURCE_PREFIXES
                    .iter()
                    .find_map(|p| label.strip_prefix(p))
                    .unwrap_or(label)
                    .to_ascii_lowercase();
                if !PLACEHOLDER_LABELS.contains(&bare.as_str()) {
                    return Some(host);
                }
            }
            from = at + suffix.len();
        }
    }
    None
}

/// A bare Azure resource name that identifies a real environment.
///
/// `rg-tom-dev-chn-001` carries no hostname suffix and no credential, so
/// nothing else here sees it — but it names a tenant's resource group just as
/// plainly. Matched by Azure's own naming convention: a resource prefix, then
/// labels, then a numeric suffix. A name whose first label is a placeholder
/// (`rg-sample-local-001`, `rg-<project>-<env>-001`) is what this asks for
/// instead.
fn tenant_resource_name_in(line: &str) -> Option<String> {
    for prefix in RESOURCE_PREFIXES {
        let mut from = 0;
        while let Some(rel) = line[from..].find(prefix) {
            let at = from + rel;
            from = at + prefix.len();
            // Must start a word, or `logic-` matches inside `mylogic-foo`.
            if at > 0 && {
                let b = line.as_bytes()[at - 1];
                b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
            } {
                continue;
            }
            let rest = &line[at + prefix.len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(rest.len());
            let labels: Vec<&str> = rest[..end].split('-').filter(|l| !l.is_empty()).collect();
            // The convention: at least two labels and a numeric suffix. Fewer
            // than that is an ordinary hyphenated word, not an environment.
            if labels.len() < 3
                || !labels
                    .last()
                    .is_some_and(|l| l.chars().all(|c| c.is_ascii_digit()))
            {
                continue;
            }
            let first = labels[0].to_ascii_lowercase();
            if !PLACEHOLDER_LABELS.contains(&first.as_str()) {
                return Some(format!("{prefix}{}", &rest[..end]));
            }
        }
    }
    None
}

fn guid_at(bytes: &[u8], i: usize) -> bool {
    const SHAPE: [usize; 5] = [8, 4, 4, 4, 12];
    let mut at = i;
    for (n, len) in SHAPE.iter().enumerate() {
        if n > 0 {
            if bytes.get(at) != Some(&b'-') {
                return false;
            }
            at += 1;
        }
        for _ in 0..*len {
            match bytes.get(at) {
                Some(b) if b.is_ascii_hexdigit() => at += 1,
                _ => return false,
            }
        }
    }
    // A longer hex run is a hash, not a GUID.
    !bytes.get(at).is_some_and(|b| b.is_ascii_hexdigit())
}

/// A non-nil GUID that an identity word introduces.
///
/// Proximity, not line membership: the word has to appear in the run-up to the
/// GUID. `const DEFAULT_SUBSCRIPTION: &str = "b4c0de7e-…"` is caught (and its
/// name has no `_ID` suffix, so a token rule missed it), while an Azurite log
/// sample whose correlation GUID is followed much later by `ClientRequestId`
/// is not.
fn identity_guid_in(line: &str) -> Option<String> {
    /// How far back an identity word still counts as introducing the GUID.
    /// Wide enough for `"WORKFLOWS_SUBSCRIPTION_ID": "…"` and a `const … = "…"`
    /// declaration, far short of a log line's worth of unrelated text.
    const LOOK_BACK: usize = 60;
    const IDENTITY_WORDS: [&str; 5] = ["subscription", "tenant", "client", "object", "principal"];

    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        if !line.is_char_boundary(i) || !guid_at(bytes, i) {
            continue;
        }
        let guid = &line[i..i + 36];
        if guid.chars().all(|c| c == '0' || c == '-') {
            continue;
        }
        let mut from = i.saturating_sub(LOOK_BACK);
        while !line.is_char_boundary(from) {
            from += 1;
        }
        let run_up = line[from..i].to_ascii_lowercase();
        if IDENTITY_WORDS.iter().any(|w| run_up.contains(w)) {
            return Some(guid.to_string());
        }
    }
    None
}

#[test]
fn no_credential_or_tenant_identity_in_any_tracked_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");

    let mut findings: BTreeSet<String> = BTreeSet::new();
    for path in String::from_utf8_lossy(&out.stdout).split('\0') {
        if path.is_empty() || is_exempt(path) {
            continue;
        }
        // Binary and non-UTF-8 files carry nothing this can read.
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let n = n + 1;
            if let Some(hit) = credential_in(line) {
                findings.insert(format!("{path}:{n}  credential: {hit}"));
            }
            if let Some(hit) = tenant_host_in(line) {
                findings.insert(format!("{path}:{n}  tenant hostname: {hit}"));
            }
            if let Some(hit) = identity_guid_in(line) {
                findings.insert(format!("{path}:{n}  identity GUID: {hit}"));
            }
            if let Some(hit) = tenant_resource_name_in(line) {
                findings.insert(format!("{path}:{n}  tenant resource name: {hit}"));
            }
        }
    }

    assert!(
        findings.is_empty(),
        "\n{} tracked file(s) carry a credential or a tenant's identity:\n\n{}\n\n\
         Nothing here belongs in this repository. Use an emulator value, a \
         placeholder hostname (foo/bar/example, optionally prefixed sbns-/func-/rg-), \
         or the nil GUID 00000000-0000-0000-0000-000000000000.\n\
         Real values belong in an untracked local.settings.json.\n",
        findings.len(),
        findings.into_iter().collect::<Vec<_>>().join("\n")
    );
}
