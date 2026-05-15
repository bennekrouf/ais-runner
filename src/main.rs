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
                .with_always_on_top(false),
        );
    LaunchBuilder::desktop().with_cfg(cfg).launch(App);
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

    // Apply system theme once at startup, then keep in sync every 2 s.
    let system_light = dark_light::detect() != dark_light::Mode::Dark;
    let mut is_light = use_signal(|| system_light);

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
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let light = dark_light::detect() != dark_light::Mode::Dark;
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
                MainScreen { logic_apps_dir: dir, on_back, is_light }
            },
        }
    }
}
