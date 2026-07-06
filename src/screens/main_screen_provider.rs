use dioxus::prelude::*;
use crate::screens::MainContext;

#[derive(Props, Clone, PartialEq)]
pub struct MainScreenWithContextProps {
    pub logic_apps_dir: String,
    pub on_back: EventHandler<()>,
}

/// Wrapper that provides MainContext to MainScreen
/// Creates context once at this level so it persists across re-renders
#[component]
pub fn MainScreenWithContext(props: MainScreenWithContextProps) -> Element {
    // Create context once and provide it to descendants
    let ctx = use_hook(|| MainContext::new());
    provide_context(ctx.clone());

    rsx! {
        super::MainScreen {
            logic_apps_dir: props.logic_apps_dir,
            on_back: props.on_back,
        }
    }
}
