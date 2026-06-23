use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::LogLine;
use crate::handlers::java;
use crate::services::process::{ManagedProcess, ServiceState};

#[derive(Clone, PartialEq)]
pub struct FuncFile {
    pub name: String,
    pub path: String,
    pub lang: String,
}

#[derive(Props, Clone, PartialEq)]
pub struct FuncPanelProps {
    pub func_apps_dir: String,
    pub state:         Signal<ServiceState>,
    pub proc:          Signal<Arc<ManagedProcess>>,
    pub log_lines:     Signal<Vec<LogLine>>,
    pub java_lines:    Signal<Vec<String>>,
}

fn scan_func_files(func_apps_dir: &str) -> Vec<FuncFile> {
    let extensions = ["java", "cs", "py", "js", "ts"];
    let mut files  = Vec::new();
    let base       = std::path::Path::new(func_apps_dir);
    if !base.exists() { return files; }

    fn walk(dir: &std::path::Path, exts: &[&str], out: &mut Vec<FuncFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if matches!(name.as_ref(), "target" | "node_modules" | ".git" | "__pycache__") {
                    continue;
                }
                walk(&path, exts, out);
            } else {
                let fname = path.file_name().unwrap_or_default().to_string_lossy();
                let ext   = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if exts.contains(&ext) {
                    out.push(FuncFile {
                        name: fname.to_string(),
                        path: path.to_string_lossy().to_string(),
                        lang: ext.to_string(),
                    });
                }
            }
        }
    }

    walk(base, &extensions, &mut files);
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

fn lang_icon(lang: &str) -> &'static str {
    match lang { "java" => "☕", "cs" => "⬡", "py" => "🐍", "ts" | "js" => "⚡", "json" => "{ }", _ => "📄" }
}

fn hljs_lang(lang: &str) -> &'static str {
    match lang {
        "java" => "java",
        "cs"   => "csharp",
        "py"   => "python",
        "js"   => "javascript",
        "ts"   => "typescript",
        "json" => "json",
        _      => "plaintext",
    }
}

#[component]
pub fn FuncPanel(props: FuncPanelProps) -> Element {
    let dir              = props.func_apps_dir.clone();
    let files            = use_memo(move || scan_func_files(&props.func_apps_dir));
    let mut selected     = use_signal(|| Option::<FuncFile>::None);
    let mut content      = use_signal(String::new);
    let mut copied           = use_signal(|| false);
    let mut highlighted_html = use_signal(String::new);
    let state                = props.state;
    let is_running           = matches!(*state.read(), ServiceState::Running);
    let is_starting          = matches!(*state.read(), ServiceState::Starting);

    // Auto-select first file on mount
    use_effect(move || {
        if selected.read().is_none() {
            if let Some(first) = files.read().first().cloned() {
                selected.set(Some(first));
            }
        }
    });

    // Load file content when selection changes
    use_effect(move || {
        if let Some(f) = selected.read().clone() {
            let text = std::fs::read_to_string(&f.path)
                .unwrap_or_else(|e| format!("// Error: {e}"));
            content.set(text);
        } else {
            content.set(String::new());
            highlighted_html.set(String::new());
        }
    });

    // Highlight via JS in a throwaway element — result sent back via dioxus.send()
    // so Dioxus never touches the rendered HTML directly (no VDOM conflict).
    use_effect(move || {
        let raw  = content.read().clone();
        let lang = selected.read().as_ref()
            .map(|f| hljs_lang(&f.lang))
            .unwrap_or("plaintext");

        if raw.is_empty() { highlighted_html.set(String::new()); return; }

        // JSON-encode so newlines / quotes are safely embedded in JS
        let raw_json = serde_json::to_string(&raw).unwrap_or_else(|_| "\"\"".into());

        let script = format!(r#"
(function() {{
    var raw  = {raw_json};
    var lang = '{lang}';

    function doHighlight() {{
        var tmp   = document.createElement('code');
        tmp.textContent = raw;
        tmp.className   = 'language-' + lang;
        hljs.highlightElement(tmp);
        dioxus.send(tmp.innerHTML);
    }}

    var isDark   = !document.body.classList.contains('light');
    var theme    = isDark ? 'github-dark' : 'github';
    var wantHref = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/styles/' + theme + '.min.css';
    var cssEl = document.getElementById('hljs-css');
    if (!cssEl) {{
        cssEl = document.createElement('link');
        cssEl.id = 'hljs-css'; cssEl.rel = 'stylesheet';
        document.head.appendChild(cssEl);
    }}
    if (cssEl.href !== wantHref) cssEl.href = wantHref;

    if (typeof hljs !== 'undefined') {{
        doHighlight();
    }} else {{
        var s = document.createElement('script');
        s.src = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';
        s.onload = doHighlight;
        document.head.appendChild(s);
    }}
}})();
"#);
        spawn(async move {
            let mut eval = document::eval(&script);
            if let Ok(val) = eval.recv().await {
                let html = match &val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                highlighted_html.set(html);
            }
        });
    });

    let n_files    = files.read().len();
    let file_lbl   = if n_files == 1 { "Functions (1)".to_string() } else { format!("Functions ({n_files})") };
    let fname_opacity = if selected.read().is_none() { "0.35" } else { "1" };

    rsx! {
        div { id: "func-panel", style: "display:flex; flex:1; overflow:hidden; flex-direction:column;",

            // ── Main area: file list + resize handle + content ────────────
            div { style: "display:flex; flex:1; overflow:hidden;",

                // Left: file list — shares #workflows so the resize handle JS works
                div { id: "workflows",
                    // Header row — status indicator only, no run button
                    div { id: "wf-header",
                        div { id: "wf-title-row",
                            h2 { "{file_lbl}" }
                            span {
                                class: match *state.read() {
                                    ServiceState::Running  => "dot running",
                                    ServiceState::Starting => "dot starting",
                                    ServiceState::Stopped  => "dot stopped",
                                }
                            }
                        }
                        span { style: "font-size:11px; opacity:0.5;",
                            match *state.read() {
                                ServiceState::Running  => "mvn azure-functions:run · :7072",
                                ServiceState::Starting => "Starting…",
                                ServiceState::Stopped  => "Not running",
                            }
                        }
                    }

                    // File list
                    div { id: "workflow-list",
                        if files.read().is_empty() {
                            div { class: "empty-state", "No source files found." }
                        }
                        for file in files.read().iter() {
                            {
                                let f   = file.clone();
                                let f2  = file.clone();
                                let sel = selected.read().as_ref().map(|s| s.path == file.path).unwrap_or(false);
                                rsx! {
                                    div {
                                        key: "{f2.path}",
                                        class: if sel { "workflow-item selected" } else { "workflow-item" },
                                        onclick: move |_| selected.set(Some(f.clone())),
                                        span { class: "wf-trigger-icon", "{lang_icon(&f2.lang)}" }
                                        span { class: "workflow-name", "{f2.name}" }
                                    }
                                }
                            }
                        }
                    }
                }

                // Resize handle — same id/JS as the workflow panel
                div {
                    id: "wf-resize-handle",
                    onmousedown: move |e| {
                        let start_x = e.client_coordinates().x;
                        document::eval(&format!(r#"
                            (function() {{
                                var wp = document.getElementById('workflows'); if (!wp) return;
                                var h  = document.getElementById('wf-resize-handle'); if (h) h.classList.add('dragging');
                                var startX = {start_x};
                                var startW = wp.getBoundingClientRect().width;
                                document.body.style.cursor = 'ew-resize';
                                document.body.style.userSelect = 'none';
                                document.body.style.webkitUserSelect = 'none';
                                var onMove = function(ev) {{
                                    wp.style.width = Math.max(160, Math.min(520, startW + (ev.clientX - startX))) + 'px';
                                }};
                                var onUp = function() {{
                                    if (h) h.classList.remove('dragging');
                                    document.body.style.cursor = '';
                                    document.body.style.userSelect = '';
                                    document.body.style.webkitUserSelect = '';
                                    document.removeEventListener('mousemove', onMove);
                                    document.removeEventListener('mouseup', onUp);
                                }};
                                document.addEventListener('mousemove', onMove);
                                document.addEventListener('mouseup', onUp);
                            }})();
                        "#));
                    }
                }

                // Right: content
                div { style: "flex:1; display:flex; flex-direction:column; overflow:hidden;",

                    // ── Persistent toolbar — always visible ───────────────
                    div { style: "display:flex; align-items:center; gap:8px; padding:6px 12px; border-bottom:1px solid var(--border); flex-shrink:0;",

                        // File name (or placeholder when nothing selected)
                        span { style: "font-size:13px; font-weight:600; flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; opacity:{fname_opacity};",
                            {
                                match selected.read().clone() {
                                    Some(ref f) => format!("{} {}", lang_icon(&f.lang), f.name),
                                    None        => "No file selected".to_string(),
                                }
                            }
                        }

                        // ── Run / Stop ─────────────────────────────────────
                        if is_running {
                            button {
                                class: "btn btn-small btn-warn",
                                title: "Stop Java Functions",
                                onclick: move |_| java::handle_stop(state, props.proc, props.log_lines),
                                span { class: "wf-spinner" }
                                "⏹ Stop"
                            }
                        } else if is_starting {
                            button {
                                class: "btn btn-small",
                                disabled: true,
                                span { class: "wf-spinner" }
                                "Starting…"
                            }
                        } else {
                            button {
                                class: "btn btn-small btn-run",
                                title: "Start Java Functions (mvn azure-functions:run)",
                                onclick: {
                                    let d = dir.clone();
                                    move |_| java::handle_start(state, props.proc, props.log_lines, props.java_lines, &d)
                                },
                                "▶ Run"
                            }
                        }

                        // ── Copy + Edit — only when a file is open ─────────
                        if let Some(ref file) = selected.read().clone() {
                            {
                                let path_editor  = file.path.clone();
                                let content_copy = content.read().clone();
                                rsx! {
                                    button {
                                        class: "btn btn-small",
                                        title: "Copy to clipboard",
                                        onclick: move |_| {
                                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                                let _ = cb.set_text(content_copy.clone());
                                                copied.set(true);
                                                spawn(async move {
                                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                    copied.set(false);
                                                });
                                            }
                                        },
                                        if *copied.read() { "✅ Copied" } else { "⎘ Copy" }
                                    }
                                    button {
                                        class: "btn btn-small",
                                        title: "Open in editor",
                                        onclick: move |_| crate::utils::open_in_editor(&path_editor),
                                        "✎ Edit"
                                    }
                                }
                            }
                        }
                    }

                    // ── File content ──────────────────────────────────────
                    {
                        match selected.read().clone() {
                            None => rsx! {
                                div { class: "detail-empty",
                                    p { "Select a file to view its source" }
                                }
                            },
                            Some(_) => rsx! {
                                pre {
                                    id: "func-source-pre",
                                    style: "flex:1; overflow:auto; margin:0; padding:12px 16px;",
                                    dangerous_inner_html: "{highlighted_html.read()}",
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}
