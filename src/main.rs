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
                MainScreen { logic_apps_dir: dir, on_back }
            },
        }
    }
}
