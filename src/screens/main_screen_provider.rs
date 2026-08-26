use crate::screens::MainContext;
use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct MainScreenWithContextProps {
    pub logic_apps_dir: String,
    pub on_back: EventHandler<()>,
    /// App-level theme handles — stored in the context so MainScreen's toggle
    /// drives the same signal as the body-class effect in main.rs.
    pub is_light: Signal<bool>,
    pub theme_overridden: Signal<bool>,
}

/// Wrapper that provides MainContext to MainScreen
/// Creates context once at this level so it persists across re-renders
#[component]
pub fn MainScreenWithContext(props: MainScreenWithContextProps) -> Element {
    // Create context once and provide it to descendants
    let ctx = use_hook(|| MainContext::new(props.is_light, props.theme_overridden));
    provide_context(ctx.clone());

    rsx! {
        super::MainScreen {
            logic_apps_dir: props.logic_apps_dir,
            on_back: props.on_back,
        }
    }
}
