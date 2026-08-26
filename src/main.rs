// The module tree lives in `src/lib.rs` so the headless runner
// (`src/bin/ais-test.rs`) shares one copy of the service layer.
use ais_runner::{screens, services, update_check, utils};

use dioxus::desktop::LogicalSize;
use dioxus::prelude::*;

use screens::{LoadingScreen, MainScreenWithContext, WelcomeScreen};
use services::config;

const MAIN_CSS: &str = include_str!("../assets/main.css");

/// On Windows, MSI installers (Node.js, Azure CLI, func) update PATH in the
/// registry but not in the current process. ais-runner is launched right after
/// the installer, so it inherits a stale PATH. Additionally, even a fresh
/// registry PATH often *omits* `%APPDATA%\npm` (where `func.cmd` / `azurite.cmd`
/// land after `npm install -g`), Maven (manual extract — never registered),
/// and Chocolatey/Scoop shim dirs. We do three things:
///   1. Read the combined Machine+User PATH from the registry via PowerShell.
///   2. Append known install dirs (npm-global, Azure CLI, Maven, Choco, Scoop).
///   3. Apply the result so every child process we spawn inherits it.
#[cfg(windows)]
fn refresh_windows_path() {
    let mut path = std::env::var("PATH").unwrap_or_default();

    // 1. Pull combined registry PATH (Machine ∪ User) — this catches everything
    //    MSI installers registered after our process started.
    if let Ok(out) = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Environment]::GetEnvironmentVariable('PATH','Machine') + ';' + \
             [Environment]::GetEnvironmentVariable('PATH','User')",
        ])
        .output()
    {
        if out.status.success() {
            let registry_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !registry_path.is_empty() {
                path = registry_path;
            }
        }
    }

    // 2. Append well-known dirs that aren't always registered in PATH —
    //    most notably %APPDATA%\npm for npm-global wrappers.
    let extras = services::runtime_manager::windows_extra_dirs();
    if !extras.is_empty() {
        let existing_lower: std::collections::HashSet<String> = path
            .split(';')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        for dir in extras {
            let s = dir.to_string_lossy().to_string();
            if !existing_lower.contains(&s.to_lowercase()) {
                path.push(';');
                path.push_str(&s);
            }
        }
    }

    std::env::set_var("PATH", path);
}

/// macOS/Linux GUI apps launched from Finder/Dock/`open` inherit a stripped
/// PATH (e.g. `/usr/bin:/bin:/usr/sbin:/sbin`) instead of the user's full
/// shell PATH. That breaks two things:
///   1. Shebang scripts like azurite (`#!/usr/bin/env node`) — `env` can't
///      find `node` and the spawn fails with exit 127.
///   2. Tools that live outside the standard probe list (Docker Desktop CLI,
///      nvm/volta/asdf-managed Node tools).
/// Prepend the known install dirs to PATH at startup so every child process
/// we spawn — including those run by shebang interpreters — sees them.
#[cfg(not(windows))]
fn refresh_unix_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    let existing: std::collections::HashSet<String> = current
        .split(':')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut prepend: Vec<String> = Vec::new();
    for dir in services::runtime_manager::unix_extra_dirs() {
        let s = dir.to_string_lossy().to_string();
        if !existing.contains(&s) {
            prepend.push(s);
        }
    }

    if prepend.is_empty() {
        return;
    }

    let new_path = if current.is_empty() {
        prepend.join(":")
    } else {
        format!("{}:{}", prepend.join(":"), current)
    };
    std::env::set_var("PATH", new_path);
}

fn main() {
    // Refresh PATH from the Windows registry so tools installed by the setup
    // script (Node, Azure CLI, func, azurite) are visible when we probe them.
    #[cfg(windows)]
    refresh_windows_path();

    // Same idea on macOS/Linux: prepend Homebrew, nvm, volta, asdf, Docker
    // Desktop, etc. so shebang scripts (`env node`) and child processes find
    // their tools when launched from Finder/Dock.
    #[cfg(not(windows))]
    refresh_unix_path();

    // ── Crash log — written before anything else so startup panics are visible ──
    // On Windows GUI apps there is no console; this file lets us diagnose crashes.
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::env::temp_dir())
        .join("AIS Runner");
    let _ = std::fs::create_dir_all(&log_dir);
    let crash_log = log_dir.join("crash.log");

    // Overwrite with a "started OK" marker; on crash the panic hook replaces it.
    let _ = std::fs::write(
        &crash_log,
        format!(
            "[{}] ais-runner starting\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        ),
    );

    {
        let crash_log2 = crash_log.clone();
        std::panic::set_hook(Box::new(move |info| {
            let msg = format!(
                "[{}] PANIC: {}\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                info
            );
            let _ = std::fs::write(&crash_log2, &msg);
            // Also try a Windows message box so non-technical users see something
            #[cfg(target_os = "windows")]
            {
                let title = "AIS Runner — Startup Error\0";
                let text = format!("{}\n\nSee crash.log in:\n{}\0", info, crash_log2.display());
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxA(
                        std::ptr::null_mut(),
                        text.as_ptr(),
                        title.as_ptr(),
                        windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                            | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                    );
                }
            }
        }));
    }

    // Suppress tiberius noise — the TLS warning is expected (SQL Edge uses
    // self-signed certs so we disable encryption intentionally).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::Level::INFO.into())
                .parse_lossy("tiberius=error"),
        )
        .init();

    // ── Azurite integrity gate ────────────────────────────────────────────
    // Before the window opens, not after: a workspace left corrupted by a
    // previous session otherwise comes up looking fine and only reveals itself
    // much later, as a test that times out with no run and a "runtime state is
    // missing" message. Repairing here costs milliseconds (local files only —
    // nothing is running yet to probe) and the findings are surfaced in the
    // log panel once the UI mounts.
    for line in services::azurite_health::startup_check(&utils::azurite_dir()) {
        let _ = std::fs::write(
            &crash_log,
            format!(
                "[{}] {}\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                line
            ),
        );
        services::azurite_health::record_startup_note(line);
    }

    let webview_data_dir = log_dir.clone();

    let _ = std::fs::write(
        &crash_log,
        format!(
            "[{}] WebView2 data dir: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            webview_data_dir.display()
        ),
    );

    let cfg = dioxus::desktop::Config::new()
        .with_data_directory(webview_data_dir)
        .with_window(
            dioxus::desktop::WindowBuilder::new()
                .with_title(concat!("AIS Local Runner ", env!("CARGO_PKG_VERSION")))
                .with_inner_size(LogicalSize::new(1280.0, 820.0))
                .with_maximized(true)
                .with_always_on_top(false)
                .with_window_icon(make_icon()),
        );
    LaunchBuilder::desktop().with_cfg(cfg).launch(App);
}

fn make_icon() -> Option<dioxus::desktop::tao::window::Icon> {
    const SIZE: u32 = 64;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            // Circular mask (rounded icon)
            let cx = x as f32 - SIZE as f32 / 2.0 + 0.5;
            let cy = y as f32 - SIZE as f32 / 2.0 + 0.5;
            let r_sq = cx * cx + cy * cy;
            let radius = SIZE as f32 / 2.0;
            let alpha = if r_sq > (radius * radius) { 0u8 } else { 255u8 };

            // Azure-inspired blue gradient: top-left lighter, bottom-right deeper
            let t = (x + y) as f32 / (SIZE * 2) as f32; // 0..1
            let r = (0.0_f32 + t * 20.0) as u8;
            let g = (120.0 - t * 30.0) as u8;
            let b = (212.0 - t * 40.0) as u8;

            // White "flow" connector shape: two nodes joined by a line
            let in_shape = is_flow_shape(x, y, SIZE);
            let (r, g, b) = if in_shape {
                (255u8, 255u8, 255u8)
            } else {
                (r, g, b)
            };

            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    dioxus::desktop::tao::window::Icon::from_rgba(rgba, SIZE, SIZE).ok()
}

fn is_flow_shape(x: u32, y: u32, size: u32) -> bool {
    let s = size as f32;
    let fx = x as f32;
    let fy = y as f32;

    // Left node: small filled circle at (22%, 50%)
    let lx = s * 0.25;
    let ly = s * 0.50;
    if (fx - lx).hypot(fy - ly) < s * 0.10 {
        return true;
    }

    // Right node: small filled circle at (75%, 50%)
    let rx = s * 0.75;
    let ry = s * 0.50;
    if (fx - rx).hypot(fy - ry) < s * 0.10 {
        return true;
    }

    // Top-center node: small circle at (50%, 28%)
    let tx = s * 0.50;
    let ty = s * 0.28;
    if (fx - tx).hypot(fy - ty) < s * 0.08 {
        return true;
    }

    // Connecting line left→right (thin horizontal bar)
    if fy > ly - s * 0.025 && fy < ly + s * 0.025 && fx > lx && fx < rx {
        return true;
    }

    // Connecting line left→top
    let dx = tx - lx;
    let dy = ty - ly;
    let len = dx.hypot(dy);
    let t = ((fx - lx) * dx + (fy - ly) * dy) / (len * len);
    if (0.15..=0.85).contains(&t) {
        let px = lx + t * dx;
        let py = ly + t * dy;
        if (fx - px).hypot(fy - py) < s * 0.025 {
            return true;
        }
    }

    false
}

// ── Screen enum ───────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum Screen {
    Welcome,
    Loading(String),
    Main(String),
}

// ── Root component ────────────────────────────────────────────────────────────

#[component]
fn App() -> Element {
    let saved = config::load();
    let screen = use_signal(|| Screen::Welcome);
    let app_cfg = use_signal(|| saved);

    // Apply system theme once at startup, then keep in sync — but stop
    // syncing once the user has manually toggled the theme button.
    let system_light = dark_light::detect() != dark_light::Mode::Dark;
    let is_light = use_signal(|| system_light);
    let theme_overridden = use_signal(|| false);

    use_effect(move || {
        let cls = if *is_light.read() { "light" } else { "" };
        document::eval(&format!("document.body.className = '{}';", cls));
    });

    // Disable autocorrect / autocapitalize / spellcheck on every input and
    // textarea — current ones and any added later via MutationObserver.
    use_effect(move || {
        document::eval(
            r#"
            (function () {
                function patch(el) {
                    el.setAttribute('autocorrect',    'off');
                    el.setAttribute('autocapitalize', 'none');
                    el.setAttribute('autocomplete',   'off');
                    el.setAttribute('spellcheck',     'false');
                }
                document.querySelectorAll('input, textarea').forEach(patch);
                new MutationObserver(function (mutations) {
                    mutations.forEach(function (m) {
                        m.addedNodes.forEach(function (node) {
                            if (node.nodeType !== 1) return;
                            if (node.matches('input, textarea')) patch(node);
                            node.querySelectorAll && node.querySelectorAll('input, textarea').forEach(patch);
                        });
                    });
                }).observe(document.body, { childList: true, subtree: true });
            })();
        "#,
        );
    });

    // DISABLED: Theme detection every 2s was causing MainScreen to re-mount repeatedly
    // (signals in props cause component recreation on every change).
    // Users can still toggle theme with the button — no need for auto-detection.
    // Re-enable this only after moving theme management to a context or MainScreen level.
    /*
    use_coroutine(move |_rx: dioxus::prelude::UnboundedReceiver<()>| async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
            if *theme_overridden.read() { continue; }
            let light = tokio::task::spawn_blocking(|| {
                dark_light::detect() != dark_light::Mode::Dark
            }).await.unwrap_or(*is_light.read());
            if light != *is_light.read() {
                is_light.set(light);
            }
        }
    });
    */

    let on_open = {
        let mut screen = screen;
        let mut app_cfg = app_cfg;
        move |dir: String| {
            let mut cfg = app_cfg.read().clone();
            cfg.push_dir(dir.clone());
            config::save(&cfg);
            app_cfg.set(cfg);
            screen.set(Screen::Loading(dir));
        }
    };

    let on_done_loading = {
        let mut screen = screen;
        move |dir: String| {
            screen.set(Screen::Main(dir));
        }
    };

    let on_back = {
        let mut screen = screen;
        move |_| {
            // Reset window title to the neutral form when returning to the picker.
            dioxus::desktop::window()
                .set_title(concat!("AIS Local Runner ", env!("CARGO_PKG_VERSION")));
            screen.set(Screen::Welcome);
        }
    };

    // ── Auto-update check ──────────────────────────────────────────────────
    let mut update_info = use_signal(|| Option::<update_check::UpdateInfo>::None);
    let mut update_dismissed = use_signal(|| false);
    use_coroutine(
        move |_rx: dioxus::prelude::UnboundedReceiver<()>| async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Some(info) = update_check::check().await {
                update_info.set(Some(info));
            }
        },
    );

    rsx! {
        document::Style { "{MAIN_CSS}" }

        // Update banner — fixed top, dismissable per session.
        if let (Some(info), false) = (update_info.read().clone(), *update_dismissed.read()) {
            div { class: "update-banner",
                span { class: "update-banner-text",
                    "ais-runner "
                    strong { "{info.latest_version}" }
                    " is available (you have {env!(\"CARGO_PKG_VERSION\")})."
                }
                a {
                    class: "update-banner-link",
                    href: "{info.release_url}",
                    target: "_blank",
                    "Download"
                }
                button {
                    class: "update-banner-dismiss",
                    onclick: move |_| update_dismissed.set(true),
                    "×"
                }
            }
        }

        match screen.read().clone() {
            Screen::Welcome => rsx! {
                WelcomeScreen { app_cfg, on_open }
            },
            Screen::Loading(dir) => rsx! {
                LoadingScreen { logic_apps_dir: dir, is_light, theme_overridden, on_done: on_done_loading, on_back }
            },
            Screen::Main(dir) => rsx! {
                MainScreenWithContext { logic_apps_dir: dir, on_back, is_light, theme_overridden }
            },
        }
    }
}
