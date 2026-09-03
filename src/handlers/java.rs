use dioxus::prelude::*;
use std::sync::Arc;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::process::{rich_path, ManagedProcess, ServiceState};
use crate::utils::make_push;

pub fn handle_start(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
    mut java_lines: Signal<Vec<String>>,
    func_apps_dir: &str,
) {
    let mut push = make_push(log_lines);
    let dir = func_apps_dir.to_string();

    // Check the directory exists before doing anything
    if !std::path::Path::new(&dir).exists() {
        state.set(ServiceState::Stopped);
        push(
            format!("⚠ function_apps directory not found: {}", dir),
            LogLevel::Error,
        );
        push(
            "  hint: create a Maven Azure Functions project in that folder.".into(),
            LogLevel::Warn,
        );
        return;
    }

    state.set(ServiceState::Starting);

    spawn(async move {
        let mut push = make_push(log_lines);

        // ── Port 7072 already in use? Reclaim only if it's OUR project. ───
        if tokio::net::TcpStream::connect("127.0.0.1:7072")
            .await
            .is_ok()
        {
            // Identify the owner so we don't silently kill a *different*
            // project's function host (the exact multi-clone footgun: one repo
            // holds :7072 while you're trying to run another).
            let owner_dir = dir.clone();
            let foreign = tokio::task::spawn_blocking(move || {
                crate::services::port_owner::owner(7072).map(|o| {
                    let ours = crate::services::port_owner::belongs_to(&o, &owner_dir);
                    (ours, o.pid, o.detail)
                })
            })
            .await
            .ok()
            .flatten();

            if let Some((false, pid, detail)) = &foreign {
                // A different project owns 7072 — do NOT kill it.
                push(
                    format!(
                    "⛔ Port 7072 is held by a DIFFERENT project's function host (PID {pid}){}. \
                     Stop that one first (or run this project's functions there) — \
                     ais-runner won't kill another project's host for you.",
                    if detail.is_empty() { String::new() } else { format!(":\n     {}", detail) }
                ),
                    LogLevel::Error,
                );
                state.set(ServiceState::Stopped);
                return;
            }

            push(
                "Port 7072 in use by a stale instance of this project — reclaiming…".into(),
                LogLevel::Warn,
            );
            // Listener-only, never our own pid — the previous `lsof -ti :7072`
            // form also matched this app's client connections to the Java host
            // and could SIGKILL ais-runner itself.
            tokio::task::spawn_blocking(|| crate::services::port_owner::kill_listener(7072))
                .await
                .ok();
            // Wait for the port to be released
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                if tokio::net::TcpStream::connect("127.0.0.1:7072")
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }

        // ── Step 0: seed local.settings.json ─────────────────────────────
        // Before packaging: the Maven plugin stages this file into target/, and
        // without it the host dies on "Missing value for AzureWebJobsStorage".
        {
            let d = dir.clone();
            match tokio::task::spawn_blocking(move || {
                crate::services::function_app_settings::ensure_settings(&d)
            })
            .await
            {
                Ok(Ok(added)) if !added.is_empty() => push(
                    format!("🔧 Seeded local.settings.json: {}", added.join(", ")),
                    LogLevel::Ok,
                ),
                Ok(Err(e)) => push(
                    format!("⚠ Could not write function_apps/local.settings.json: {e}"),
                    LogLevel::Warn,
                ),
                _ => {}
            }
        }

        // ── Step 1: mvn package -DskipTests ──────────────────────────────
        push(
            format!("$ cd {} && mvn package -DskipTests", dir),
            LogLevel::Info,
        );

        let pkg_result = tokio::task::spawn_blocking({
            let d = dir.clone();
            move || {
                std::process::Command::new("mvn")
                    .args(["package", "-DskipTests"])
                    .current_dir(&d)
                    .env("PATH", rich_path())
                    .output()
            }
        })
        .await;

        let pkg_ok = match pkg_result {
            Ok(Ok(out)) => {
                // Stream notable lines from package output (errors/warnings only)
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                );
                for line in combined.lines() {
                    let l = line.trim();
                    if l.is_empty() {
                        continue;
                    }
                    // Show errors and build summary, suppress download noise
                    if l.starts_with("[ERROR]") {
                        push(l.to_string(), LogLevel::Error);
                    } else if l.starts_with("[WARNING]") && !l.contains("deprecated") {
                        push(l.to_string(), LogLevel::Warn);
                    } else if l.contains("BUILD SUCCESS") || l.contains("BUILD FAILURE") {
                        push(
                            l.to_string(),
                            if l.contains("SUCCESS") {
                                LogLevel::Ok
                            } else {
                                LogLevel::Error
                            },
                        );
                    }
                }
                if out.status.success() {
                    push(
                        "✅ mvn package complete — starting Java Function App…".into(),
                        LogLevel::Ok,
                    );
                    true
                } else {
                    push(
                        "❌ mvn package failed — fix the errors above before retrying.".into(),
                        LogLevel::Error,
                    );
                    false
                }
            }
            Ok(Err(e)) => {
                push(format!("❌ Could not run mvn: {}", e), LogLevel::Error);
                push(
                    "  hint: install Maven — brew install maven".into(),
                    LogLevel::Warn,
                );
                false
            }
            Err(e) => {
                push(format!("❌ spawn error: {}", e), LogLevel::Error);
                false
            }
        };

        if !pkg_ok {
            state.set(ServiceState::Stopped);
            return;
        }

        // ── Step 2: func host start --port 7072 ──────────────────────────
        // Run func directly in the staging dir to avoid the Maven plugin's
        // hardcoded port 7071 which conflicts with Logic Apps func start.
        let staging = format!("{}/target/azure-functions/ais-functions", dir);
        let func = crate::services::runtime_manager::resolve_tool("func");
        // Pick the JDK *before* starting the host: an unsupported one makes the
        // worker die silently and the host time out 40s later with nothing but
        // "Failed to start a new language worker for runtime: java".
        let jdk = match select_jdk(&dir) {
            Ok(jdk) => jdk,
            Err(e) => {
                for line in e.lines() {
                    push(line.to_string(), LogLevel::Error);
                    java_lines.write().push(line.to_string());
                }
                state.set(ServiceState::Stopped);
                return;
            }
        };
        {
            let msg = format!("JAVA_HOME={} (Java {})", jdk.home, jdk.major);
            push(msg.clone(), LogLevel::Info);
            java_lines.write().push(msg);
        }
        let cmd_msg = format!("$ cd {} && {} host start --port 7072", staging, func);
        push(cmd_msg.clone(), LogLevel::Info);
        java_lines.write().push(cmd_msg);

        // Both are needed: the host reads JAVA_HOME to build the worker command,
        // and a keg-only JDK is on no PATH the app inherits, so `java` itself
        // has to be findable too.
        let cmd_env: Vec<(String, String)> = vec![
            ("JAVA_HOME".into(), jdk.home.clone()),
            (
                "PATH".into(),
                format!("{}/bin:{}", jdk.home, crate::services::process::rich_path()),
            ),
        ];

        match proc.read().start_with_env(
            &func,
            &["host", "start", "--port", "7072"],
            Some(&staging),
            &cmd_env,
        ) {
            Ok((stdout, stderr)) => {
                state.set(ServiceState::Running);
                let start_msg = "Java Function App starting on port 7072…".to_string();
                push(start_msg.clone(), LogLevel::Ok);
                java_lines.write().push(start_msg);

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
                crate::services::process::stream_output(stdout, stderr, tx, true);

                spawn(async move {
                    while let Some((line, _is_err)) = rx.recv().await {
                        if !crate::components::log_panel::is_mvn_noise(&line) {
                            let mut w = java_lines.write();
                            w.push(line);
                            let len = w.len();
                            if len > 2000 {
                                let d = len - 2000;
                                w.drain(..d);
                            }
                        }
                    }
                });
            }
            Err(e) => {
                state.set(ServiceState::Stopped);
                push(format!("Java Function App error: {}", e), LogLevel::Error);
            }
        }
    });
}

/// JDK major versions the Azure Functions Java worker can run on (Core Tools
/// v4). Anything newer starts and dies immediately, which the host reports only
/// as "Failed to start a new language worker for runtime: java".
const SUPPORTED_JAVA: [u32; 4] = [8, 11, 17, 21];

pub struct Jdk {
    pub home: String,
    pub major: u32,
}

/// Choose the JDK to run the function host with.
///
/// Preference order: the version the project's pom targets, then the newest
/// version the worker supports. A machine with only unsupported JDKs gets an
/// error naming what it found and what to install — not a 40s host timeout.
fn select_jdk(func_apps_dir: &str) -> Result<Jdk, String> {
    let found = discover_jdks();
    if found.is_empty() {
        return Err("❌ No JDK found — the Java worker cannot start.\n  \
             hint: brew install openjdk@17 (or openjdk@21)"
            .into());
    }
    let wanted = project_java_version(func_apps_dir);
    let mut supported: Vec<Jdk> = found
        .into_iter()
        .filter(|j| SUPPORTED_JAVA.contains(&j.major))
        .collect();
    if supported.is_empty() {
        return Err(format!(
            "❌ No JDK the Azure Functions Java worker supports (needs {}).\n  \
             hint: brew install openjdk@{} — then start again",
            SUPPORTED_JAVA
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("/"),
            wanted.unwrap_or(17)
        ));
    }
    // Newest first, so the fallback is the most capable supported JDK.
    supported.sort_by(|a, b| b.major.cmp(&a.major));
    let pick = wanted
        .and_then(|w| supported.iter().position(|j| j.major == w))
        .unwrap_or(0);
    Ok(supported.swap_remove(pick))
}

/// `<java.version>` from the function app's pom, when it declares one.
fn project_java_version(func_apps_dir: &str) -> Option<u32> {
    let pom = std::fs::read_to_string(std::path::Path::new(func_apps_dir).join("pom.xml")).ok()?;
    let raw = pom
        .split("<java.version>")
        .nth(1)?
        .split("</java.version>")
        .next()?
        .trim()
        .to_string();
    // "1.8" is how Java 8 is spelled in a pom.
    raw.strip_prefix("1.").unwrap_or(&raw).parse().ok()
}

/// Every JDK we can find, newest-first order not guaranteed.
///
/// GUI apps on macOS inherit neither the shell PATH nor SDKMAN, and a
/// Homebrew `openjdk@N` is keg-only — so nothing shows up unless we look in
/// the install locations directly.
fn discover_jdks() -> Vec<Jdk> {
    let mut homes: Vec<String> = Vec::new();
    let mut add = |p: String| {
        if !p.is_empty()
            && !homes.contains(&p)
            && std::path::Path::new(&p).join("bin/java").exists()
        {
            homes.push(p);
        }
    };

    if let Ok(jh) = std::env::var("JAVA_HOME") {
        add(jh);
    }
    // macOS registry (only lists JDKs installed under /Library/Java).
    if let Ok(out) = std::process::Command::new("/usr/libexec/java_home").output() {
        if out.status.success() {
            add(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }
    }
    // Whatever `java` on PATH resolves to (…/Home/bin/java → …/Home).
    let java = crate::services::runtime_manager::resolve_tool("java");
    if let Ok(canonical) = std::fs::canonicalize(&java) {
        if let Some(home) = canonical.parent().and_then(|bin| bin.parent()) {
            add(home.to_string_lossy().to_string());
        }
    }
    // Homebrew kegs (openjdk, openjdk@17, openjdk@21, …), system JDKs, SDKMAN.
    let globs = [
        "/opt/homebrew/opt",
        "/usr/local/opt",
        "/Library/Java/JavaVirtualMachines",
    ];
    for base in globs {
        if let Ok(entries) = std::fs::read_dir(base) {
            for e in entries.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("openjdk") && !p.join("Contents/Home").exists() {
                    continue;
                }
                // Homebrew keg layout, then the .jdk bundle layout.
                add(p
                    .join("libexec/openjdk.jdk/Contents/Home")
                    .to_string_lossy()
                    .to_string());
                add(p.join("Contents/Home").to_string_lossy().to_string());
            }
        }
    }
    if let Some(home) = dirs_next_home() {
        if let Ok(entries) = std::fs::read_dir(home.join(".sdkman/candidates/java")) {
            for e in entries.flatten() {
                add(e.path().to_string_lossy().to_string());
            }
        }
    }

    homes
        .into_iter()
        .filter_map(|home| jdk_major(&home).map(|major| Jdk { home, major }))
        .collect()
}

fn dirs_next_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

/// Major version of the JDK at `home`, read from its `release` file — cheaper
/// and more reliable than spawning `java -version` for every candidate.
fn jdk_major(home: &str) -> Option<u32> {
    let release = std::fs::read_to_string(std::path::Path::new(home).join("release")).ok()?;
    let v = release
        .lines()
        .find_map(|l| l.strip_prefix("JAVA_VERSION="))?
        .trim()
        .trim_matches('"');
    let first = v.split('.').next()?;
    // 1.8.0_392 → 8; 21.0.12.1 → 21.
    if first == "1" {
        v.split('.').nth(1)?.parse().ok()
    } else {
        first.parse().ok()
    }
}

pub fn handle_stop(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    match proc.read().stop() {
        Ok(_) => {
            state.set(ServiceState::Stopped);
            push("Java Function App stopped.".into(), LogLevel::Warn);
        }
        Err(e) => push(format!("Error: {}", e), LogLevel::Error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pom_java_version_is_read_in_both_spellings() {
        let dir = std::env::temp_dir().join(format!("ais-pom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pom = dir.join("pom.xml");
        std::fs::write(&pom, "<project><java.version>17</java.version></project>").unwrap();
        assert_eq!(project_java_version(&dir.to_string_lossy()), Some(17));
        // Java 8 is spelled 1.8 in a pom.
        std::fs::write(&pom, "<project><java.version>1.8</java.version></project>").unwrap();
        assert_eq!(project_java_version(&dir.to_string_lossy()), Some(8));
        std::fs::write(&pom, "<project/>").unwrap();
        assert_eq!(project_java_version(&dir.to_string_lossy()), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bug this replaced: with only Java 21 and Java 26 installed, the old
    /// probe returned the first *existing* path (Homebrew's plain `openjdk`,
    /// i.e. 26), whose worker dies on startup.
    #[test]
    fn only_worker_supported_jdks_are_ever_selected() {
        for jdk in discover_jdks() {
            if !SUPPORTED_JAVA.contains(&jdk.major) {
                continue;
            }
            assert!(std::path::Path::new(&jdk.home).join("bin/java").exists());
        }
        // Whatever this machine has, a selection is never an unsupported major.
        if let Ok(picked) = select_jdk(".") {
            assert!(
                SUPPORTED_JAVA.contains(&picked.major),
                "picked {}",
                picked.major
            );
        }
    }
}
