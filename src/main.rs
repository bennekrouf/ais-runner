mod components;
mod handlers;
mod screens;
mod services;
mod utils;

use dioxus::prelude::*;
use dioxus::desktop::LogicalSize;

use services::config;
use screens::{WelcomeScreen, MainScreen};

const MAIN_CSS: &str = include_str!("../assets/main.css");

fn main() {
    tracing_subscriber::fmt::init();
    let cfg = dioxus::desktop::Config::new()
        .with_window(
            dioxus::desktop::WindowBuilder::new()
                .with_title("AIS Local Runner")
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
    let lx = s * 0.25; let ly = s * 0.50;
    if (fx - lx).hypot(fy - ly) < s * 0.10 { return true; }

    // Right node: small filled circle at (75%, 50%)
    let rx = s * 0.75; let ry = s * 0.50;
    if (fx - rx).hypot(fy - ry) < s * 0.10 { return true; }

    // Top-center node: small circle at (50%, 28%)
    let tx = s * 0.50; let ty = s * 0.28;
    if (fx - tx).hypot(fy - ty) < s * 0.08 { return true; }

    // Connecting line left→right (thin horizontal bar)
    if fy > ly - s * 0.025 && fy < ly + s * 0.025 && fx > lx && fx < rx { return true; }

    // Connecting line left→top
    let dx = tx - lx; let dy = ty - ly;
    let len = dx.hypot(dy);
    let t = ((fx - lx) * dx + (fy - ly) * dy) / (len * len);
    if (0.15..=0.85).contains(&t) {
        let px = lx + t * dx; let py = ly + t * dy;
        if (fx - px).hypot(fy - py) < s * 0.025 { return true; }
    }

    false
}

// ── Screen enum ───────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum Screen {
    Welcome,
    Main(String),
}

// ── Root component ────────────────────────────────────────────────────────────

#[component]
fn App() -> Element {
    let saved   = config::load();
    let screen  = use_signal(|| Screen::Welcome);
    let app_cfg = use_signal(|| saved);

    // Apply system theme once at startup, then keep in sync — but stop
    // syncing once the user has manually toggled the theme button.
    let system_light = dark_light::detect() != dark_light::Mode::Dark;
    let mut is_light          = use_signal(|| system_light);
    let theme_overridden = use_signal(|| false);

    use_effect(move || {
        let cls = if *is_light.read() { "light" } else { "" };
        document::eval(&format!("document.body.className = '{}';", cls));
    });

    // Disable autocorrect / autocapitalize / spellcheck on every input and
    // textarea — current ones and any added later via MutationObserver.
    use_effect(move || {
        document::eval(r#"
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
        "#);
    });

    use_coroutine(move |_rx: dioxus::prelude::UnboundedReceiver<()>| async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
            // Stop following the system once the user has chosen manually.
            if *theme_overridden.read() { continue; }
            // dark_light reads NSUserDefaults — must run on a blocking thread on macOS.
            let light = tokio::task::spawn_blocking(|| {
                dark_light::detect() != dark_light::Mode::Dark
            }).await.unwrap_or(*is_light.read());
            if light != *is_light.read() {
                is_light.set(light);
            }
        }
    });

    let on_open = {
        let mut screen  = screen;
        let mut app_cfg = app_cfg;
        move |dir: String| {
            let mut cfg = app_cfg.read().clone();
            cfg.push_dir(dir.clone());
            config::save(&cfg);
            app_cfg.set(cfg);
            screen.set(Screen::Main(dir));
        }
    };

    let on_back = {
        let mut screen = screen;
        move |_| screen.set(Screen::Welcome)
    };

    rsx! {
        document::Style { "{MAIN_CSS}" }
        match screen.read().clone() {
            Screen::Welcome => rsx! {
                WelcomeScreen { app_cfg, on_open }
            },
            Screen::Main(dir) => rsx! {
                MainScreen { logic_apps_dir: dir, on_back, is_light, theme_overridden }
            },
        }
    }
}
