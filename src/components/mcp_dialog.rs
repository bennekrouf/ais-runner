//! How to use AIS Runner from Claude: what to paste, and where.
//!
//! The server is `ais-mcp`, a separate executable shipped next to the app
//! (inside the .app on macOS, in the install folder on Windows, in the tarball
//! on Linux). Its path is read from where this app runs rather than written
//! down, so the snippet is right wherever it was installed, and in a
//! development build once `ais-mcp` has been built next to it.

use dioxus::prelude::*;
use std::path::PathBuf;

/// `ais-mcp` next to this executable.
fn server() -> PathBuf {
    let name = format!("ais-mcp{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// The `mcpServers` entry for Claude Desktop's config file. Built with serde
/// so a Windows path's backslashes arrive escaped.
pub fn desktop_config(command: &str) -> String {
    let config = serde_json::json!({ "mcpServers": { "ais-runner": { "command": command } } });
    serde_json::to_string_pretty(&config).unwrap_or_default()
}

/// The one line that adds it to Claude Code, for every project.
pub fn code_command(command: &str) -> String {
    format!("claude mcp add ais-runner --scope user -- \"{command}\"")
}

/// The one line that adds it to Codex. Its CLI, IDE extension and app share
/// one list of servers, each one there for every project.
pub fn codex_command(command: &str) -> String {
    format!("codex mcp add ais-runner -- \"{command}\"")
}

/// Where Claude Desktop keeps its config.
fn desktop_config_path() -> &'static str {
    if cfg!(target_os = "windows") {
        "%APPDATA%\\Claude\\claude_desktop_config.json"
    } else if cfg!(target_os = "macos") {
        "~/Library/Application Support/Claude/claude_desktop_config.json"
    } else {
        "~/.config/Claude/claude_desktop_config.json"
    }
}

#[component]
pub fn McpDialog(on_close: EventHandler<()>) -> Element {
    let server = server();
    let found = server.is_file();
    let command = server.to_string_lossy().into_owned();
    let desktop = desktop_config(&command);
    let code = code_command(&command);
    let codex = codex_command(&command);
    rsx! {
        div { id: "dialog-backdrop", onclick: move |_| on_close.call(()) }
        div { class: "mcp-dialog",
            div { class: "mcp-dialog-header",
                div {
                    h3 { "Use AIS Runner from Claude or Codex" }
                    p { class: "dialog-hint",
                        "Ask Claude about this project in chat: why the last run of a workflow failed, which \
                         workflows write to a queue, why last night's build broke. It reads only: it never starts \
                         the local stack, runs a workflow, sends a message or changes a file."
                    }
                }
                button { class: "view-btn", title: "Close", onclick: move |_| on_close.call(()), "✕" }
            }
            div { class: "mcp-dialog-body",
                if !found {
                    p { class: "mcp-missing",
                        "ais-mcp isn't next to this build of AIS Runner. Builds from mayorana.ch include it; \
                         from a checkout, run "
                        code { "cargo build --bin ais-mcp" }
                        " first."
                    }
                }
                p { class: "dialog-label", "1 · Claude Desktop" }
                p { class: "dialog-hint",
                    "Settings → Developer → Edit Config, paste this in and save, then quit Claude and open it \
                     again. If the file already has an mcpServers section, add just the ais-runner entry to it."
                }
                Snippet { text: desktop.clone() }
                p { class: "dialog-hint mcp-path", "The file: {desktop_config_path()}" }
                p { class: "dialog-label", "2 · Claude Code" }
                p { class: "dialog-hint", "Run this once in a terminal:" }
                Snippet { text: code.clone() }
                p { class: "dialog-label", "3 · Codex" }
                p { class: "dialog-hint", "Run this once in a terminal:" }
                Snippet { text: codex.clone() }
                p { class: "dialog-label", "4 · Try it" }
                p { class: "dialog-hint",
                    "Start Azurite and ▶ func here as usual, then ask: “Why did the last run of my order workflow \
                     fail?” Tools follow the project last opened here; add \"args\": [\"--project\", \"…\"] to \
                     the entry to pin one."
                }
                p { class: "dialog-hint mcp-note",
                    "Claude in the browser (claude.ai) can’t use this: it only connects to servers on the internet, \
                     and AIS Runner runs on your computer. Any other app that speaks MCP can, with the same command."
                }
            }
        }
    }
}

/// A block of text to paste somewhere else, with its own Copy button.
#[component]
fn Snippet(text: String) -> Element {
    let mut copied = use_signal(|| false);
    let to_copy = text.clone();
    rsx! {
        div { class: "mcp-snippet-wrap",
            pre { class: "mcp-snippet", "{text}" }
            button {
                class: "view-btn mcp-copy",
                onclick: move |_| {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        copied.set(cb.set_text(to_copy.clone()).is_ok());
                    }
                },
                if copied() { "Copied ✓" } else { "Copy" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_entry_starts_the_server_next_to_the_app() {
        let path = "/Applications/AIS Runner.app/Contents/MacOS/ais-mcp";
        let config: serde_json::Value = serde_json::from_str(&desktop_config(path)).unwrap();
        assert_eq!(config["mcpServers"]["ais-runner"]["command"], path);
        let win = r"C:\Users\me\AppData\Local\Programs\AIS Runner\ais-mcp.exe";
        let config: serde_json::Value = serde_json::from_str(&desktop_config(win)).unwrap();
        assert_eq!(config["mcpServers"]["ais-runner"]["command"], win);
        assert_eq!(
            code_command(path),
            "claude mcp add ais-runner --scope user -- \"/Applications/AIS Runner.app/Contents/MacOS/ais-mcp\""
        );
        assert!(server().ends_with(format!("ais-mcp{}", std::env::consts::EXE_SUFFIX)));
    }

    #[test]
    fn the_codex_command_quotes_a_path_with_spaces() {
        assert_eq!(
            codex_command("/A B/ais-runner"),
            "codex mcp add ais-runner -- \"/A B/ais-runner\""
        );
    }
}
