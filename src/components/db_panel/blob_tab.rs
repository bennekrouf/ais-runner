use dioxus::prelude::*;
use std::collections::HashSet;
use std::collections::HashMap;
use crate::services::{
    azure_cli::BlobInfo,
    azurite_client,
};

/// Create a container. Folders are deliberately NOT created here: an empty
/// folder is just a `prefix/.keep` marker blob, and a container-watching
/// workflow fires on that marker before the user can add the real file. Folders
/// instead come into existence atomically when a file is imported into them
/// (see `join_blob_path` / the per-container Import form).
async fn do_create_container(name: String) -> Result<(), String> {
    let container = name.trim().to_string();
    tokio::task::spawn_blocking(move || azurite_client::create_container(&container))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
        .map_err(|e| format!("Create container failed: {}", e))
}

/// Normalise a user-typed blob path. The user types the full name including any
/// (possibly nested) folder, e.g. `payments/PAYMENT_TEST.csv`. Leading/trailing
/// slashes and blank segments are stripped: `/ payments / 2026 / f.csv ` →
/// `payments/2026/f.csv`. A bare name → container root.
fn clean_blob_path(input: &str) -> String {
    input
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Blob storage has no real folders — they're just name prefixes. Now that we
/// no longer create `.keep` markers, a nested blob like `payments/f.csv` would
/// render with no visible folder header. This returns the blob list with a
/// synthetic `<prefix>/.keep` row inserted for every ancestor folder that lacks
/// a real marker, so the tree shows each folder even when it holds only real
/// files. Sorted so a folder header sorts immediately before its contents.
fn rows_with_folders(blobs: &[BlobInfo]) -> Vec<BlobInfo> {
    use std::collections::BTreeSet;
    let existing: BTreeSet<&str> = blobs.iter().map(|b| b.name.as_str()).collect();
    let mut markers: BTreeSet<String> = BTreeSet::new();
    for b in blobs {
        let parts: Vec<&str> = b.name.split('/').collect();
        // every ancestor prefix (exclude the file/leaf itself)
        for i in 1..parts.len() {
            let marker = format!("{}/.keep", parts[..i].join("/"));
            if !existing.contains(marker.as_str()) {
                markers.insert(marker);
            }
        }
    }
    let mut out: Vec<BlobInfo> = blobs.to_vec();
    out.extend(markers.into_iter().map(|name| BlobInfo { name, size: 0 }));
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn blob_fetch_all() -> Result<Vec<(String, Vec<BlobInfo>)>, String> {
    let names = azurite_client::list_containers()
        .map_err(|e| format!("list containers: {}", e))?;
    let mut out = Vec::new();
    for name in names {
        let blobs = azurite_client::list_blobs(&name).unwrap_or_default();
        out.push((name, blobs));
    }
    Ok(out)
}

#[derive(Props, Clone, PartialEq)]
pub struct BlobTabProps {
    pub azurite_running: bool,
    /// Project path, used to discover `.ais-runner/message-templates`.
    pub logic_apps_dir:  String,
    pub blob_edits:      Signal<HashMap<String, String>>,
    pub webjobs_edit:    Signal<String>,
    pub is_open:         Signal<bool>,
    pub active_tab:      Signal<&'static str>,
    pub status:          Signal<Option<(String, bool)>>,
}

/// New full prefix when renaming only the last segment of a folder path.
/// Renaming keeps the folder where it is: `archive/payments` + `pay` →
/// `archive/pay`, not `pay`.
fn rename_last_segment(path: &str, new_name: &str) -> String {
    let new_name = new_name.trim().trim_matches('/');
    match path.trim_end_matches('/').rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/{new_name}"),
        None => new_name.to_string(),
    }
}

#[component]
pub fn BlobTab(props: BlobTabProps) -> Element {
    let mut blob_containers:  Signal<Option<Vec<(String, Vec<BlobInfo>)>>> = use_signal(|| None);
    let mut blob_loading:     Signal<bool>            = use_signal(|| false);
    let mut blob_clearing:    Signal<HashSet<String>> = use_signal(HashSet::new);
    let mut blob_uploading:   Signal<HashSet<String>> = use_signal(HashSet::new);
    let mut blob_downloading: Signal<HashSet<String>> = use_signal(HashSet::new);
    let mut blob_creating:    Signal<bool>            = use_signal(|| false);
    let mut blob_expanded:    Signal<HashSet<String>> = use_signal(HashSet::new);
    let mut new_container_name: Signal<String>        = use_signal(String::new);
    let mut blob_clear_confirm: Signal<Option<String>> = use_signal(|| None);
    // Message templates are read once per mount: they're small, project-local
    // files that change far less often than the blob list they decorate.
    let templates: Signal<Vec<crate::services::msg_template::MessageTemplate>> = {
        let dir = props.logic_apps_dir.clone();
        use_signal(move || crate::services::msg_template::discover(std::path::Path::new(&dir)).0)
    };
    let mut sending_event: Signal<HashSet<String>> = use_signal(HashSet::new);
    // Folder rename: which (container, folder-path) is being edited, the in-flight
    // set, and the text box contents.
    let mut folder_rename:  Signal<Option<(String, String)>> = use_signal(|| None);
    let mut rename_input:   Signal<String>                   = use_signal(String::new);
    let mut blob_renaming:  Signal<HashSet<String>>          = use_signal(HashSet::new);
    // Import file: which container's form is open, the chosen local file path,
    // and the target folder / file name the user is editing. A blob is written
    // atomically to `folder/filename` — no empty-folder marker, so a container
    // watcher only ever sees a complete file.
    let mut import_target: Signal<Option<String>> = use_signal(|| None);
    let mut import_path:   Signal<Option<String>> = use_signal(|| None);
    // Full target blob path the user is editing, e.g. "payments/PAYMENT_TEST.csv".
    let mut import_name:   Signal<String>         = use_signal(String::new);

    let mut webjobs_edit = props.webjobs_edit;
    let mut status       = props.status;
    let active_tab       = props.active_tab;
    let azurite_up       = props.azurite_running;

    // Auto-refresh blob list whenever the blob tab becomes active (or the panel opens)
    use_effect(move || {
        let _open = props.is_open.read(); // reactive: re-run when panel opens
        if *active_tab.read() == "blob" && azurite_up && !*blob_loading.peek() {
            blob_loading.set(true);
            blob_clear_confirm.set(None);
            spawn(async move {
                match tokio::task::spawn_blocking(blob_fetch_all).await {
                    Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                    Ok(Err(e))   => { status.set(Some((format!("Refresh failed: {}", e), true))); }
                    Err(_)       => {}
                }
                blob_loading.set(false);
            });
        }
    });

    // Auto-refresh expanded blob containers every 3 s while panel is open.
    use_effect(move || {
        let is_open = props.is_open;
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;

                if !*is_open.read()
                    || *active_tab.peek() != "blob"
                    || blob_expanded.peek().is_empty()
                {
                    continue;
                }

                let expanded: Vec<String> = blob_expanded.peek().iter().cloned().collect();
                for container in expanded {
                    let c  = container.clone();
                    let c2 = container.clone();
                    if let Ok(Ok(updated)) = tokio::task::spawn_blocking(move || {
                        azurite_client::list_blobs(&c)
                    }).await {
                        if let Some(ref mut list) = *blob_containers.write() {
                            if let Some(entry) = list.iter_mut().find(|(n, _)| n == &c2) {
                                entry.1 = updated;
                            }
                        }
                    }
                }
            }
        });
    });

    rsx! {
        // ── AzureWebJobsStorage ───────────────────────────────────────
        div { class: "db-card", style: "margin-bottom:12px",
            div { class: "db-card-header",
                span { class: "db-card-name", "AzureWebJobsStorage" }
                {
                    let v = webjobs_edit.read().clone();
                    let is_azurite = v == "UseDevelopmentStorage=true"
                        || v.contains("127.0.0.1:10000")
                        || v.contains("localhost:10000");
                    if is_azurite {
                        rsx! { span { class: "db-auth-badge", style: "color:var(--green);border-color:var(--green)", "✅ Azurite" } }
                    } else if v.is_empty() {
                        rsx! { span { class: "db-auth-badge db-badge-missing", "not set" } }
                    } else {
                        rsx! { span { class: "db-auth-badge db-badge-missing", "⚠ not Azurite" } }
                    }
                }
            }
            div { class: "db-field-row",
                input {
                    class: "db-field-input",
                    placeholder: "UseDevelopmentStorage=true",
                    value: "{webjobs_edit.read()}",
                    oninput: move |e| webjobs_edit.set(e.value()),
                }
            }
        }

        div { class: "db-section",
            // ── Header row ───────────────────────────────────────────
            div { class: "db-section-header",
                div { class: "db-section-title-row",
                    div { class: "db-section-title-right",
                        if *blob_loading.read() {
                            span { class: "db-fetching", "loading…" }
                        } else if azurite_up {
                            button {
                                class: "btn btn-small",
                                onclick: move |_| {
                                    blob_loading.set(true);
                                    blob_clear_confirm.set(None);
                                    status.set(None);
                                    spawn(async move {
                                        match tokio::task::spawn_blocking(blob_fetch_all).await {
                                            Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                                            Ok(Err(e))   => {
                                                status.set(Some((format!("Refresh failed: {}", e), true)));
                                                blob_containers.set(Some(vec![]));
                                            }
                                            Err(e) => {
                                                status.set(Some((format!("Task panicked: {}", e), true)));
                                            }
                                        }
                                        blob_loading.set(false);
                                    });
                                },
                                "⟳ Refresh"
                            }
                        }
                    }
                }
                span { class: "db-section-sub", "http://127.0.0.1:10000/devstoreaccount1" }
            }

            if !azurite_up {
                div { class: "blob-offline", "⚠ Start Azurite to manage local storage" }
            } else {
                // ── New container form ────────────────────────────────
                div { class: "blob-new-row",
                    input {
                        r#type: "text",
                        placeholder: "new container name",
                        value: "{new_container_name.read()}",
                        oninput: move |e| new_container_name.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                let name = new_container_name.read().trim().to_string();
                                if !name.is_empty() && !*blob_creating.read() {
                                    blob_creating.set(true);
                                    status.set(None);
                                    spawn(async move {
                                        if let Err(msg) = do_create_container(name).await {
                                            status.set(Some((msg, true)));
                                            blob_creating.set(false);
                                            return;
                                        }
                                        match tokio::task::spawn_blocking(blob_fetch_all).await {
                                            Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                                            Ok(Err(e)) => { status.set(Some((format!("List failed: {}", e), true))); }
                                            Err(_) => {}
                                        }
                                        new_container_name.set(String::new());
                                        blob_creating.set(false);
                                    });
                                }
                            }
                        },
                    }
                    button {
                        class: "btn btn-small btn-run",
                        disabled: new_container_name.read().trim().is_empty() || *blob_creating.read(),
                        onclick: move |_| {
                            let name = new_container_name.read().trim().to_string();
                            if !name.is_empty() && !*blob_creating.read() {
                                blob_creating.set(true);
                                status.set(None);
                                spawn(async move {
                                    if let Err(msg) = do_create_container(name).await {
                                        status.set(Some((msg, true)));
                                        blob_creating.set(false);
                                        return;
                                    }
                                    match tokio::task::spawn_blocking(blob_fetch_all).await {
                                        Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                                        Ok(Err(e)) => { status.set(Some((format!("List failed: {}", e), true))); }
                                        Err(_) => {}
                                    }
                                    new_container_name.set(String::new());
                                    blob_creating.set(false);
                                });
                            }
                        },
                        if *blob_creating.read() { "Creating…" } else { "+ Create" }
                    }
                }

                // ── Container tree ────────────────────────────────────
                div { class: "blob-tree",
                    if let Some(containers) = blob_containers.read().clone() {
                        if containers.is_empty() {
                            div { class: "blob-empty", "No containers — create one above or click ⟳ Refresh" }
                        }
                        for (cname, blobs) in containers {
                            {
                                let cname2    = cname.clone();
                                let cname4    = cname.clone();
                                let cname_exp = cname.clone();
                                let is_expanded  = blob_expanded.read().contains(&cname);
                                let is_clearing  = blob_clearing.read().contains(&cname);
                                let is_uploading = blob_uploading.read().contains(&cname);
                                let import_open  = import_target.read().as_deref() == Some(&cname);
                                let cname_imp    = cname.clone();  // for the Import toggle button
                                let cname_form   = cname.clone();  // for the inline import form
                                let is_empty     = blobs.iter().all(|b| b.name.ends_with("/.keep"));
                                let confirm_pending = blob_clear_confirm.read().as_deref() == Some(&cname);
                                let blob_count_label = if blobs.is_empty() {
                                    "empty".to_string()
                                } else {
                                    let s = if blobs.len() == 1 { "" } else { "s" };
                                    format!("{} blob{}", blobs.len(), s)
                                };
                                let expand_icon = if is_expanded { "▼" } else { "▶" };
                                rsx! {
                                    div { class: "blob-container-wrapper",
                                    div { class: "blob-container-row",
                                        button {
                                            class: "blob-expand-btn",
                                            title: if is_expanded { "Collapse" } else { "Expand blob list" },
                                            onclick: move |_| {
                                                let mut exp = blob_expanded.write();
                                                if exp.contains(&cname_exp) {
                                                    exp.remove(&cname_exp);
                                                } else {
                                                    exp.insert(cname_exp.clone());
                                                }
                                            },
                                            "{expand_icon}"
                                        }
                                        div { class: "blob-container-info",
                                            span { class: "blob-container-name", "{cname}" }
                                            span { class: "blob-count", "{blob_count_label}" }
                                        }
                                        div { class: "blob-container-actions",
                                            button {
                                                class: if import_open { "btn btn-small btn-run" } else { "btn btn-small" },
                                                disabled: is_uploading || is_clearing,
                                                title: "Import a file into this container (choose the folder + name)",
                                                onclick: move |_| {
                                                    if import_target.read().as_deref() == Some(&cname_imp) {
                                                        import_target.set(None);
                                                    } else {
                                                        // Open a fresh form for this container.
                                                        import_target.set(Some(cname_imp.clone()));
                                                        import_path.set(None);
                                                        import_name.set(String::new());
                                                    }
                                                },
                                                if is_uploading { "↑ …" } else { "↑ Import" }
                                            }
                                            if confirm_pending {
                                                button {
                                                    class: "btn btn-small btn-danger",
                                                    disabled: is_clearing,
                                                    title: "Confirm — delete ALL blobs in this container",
                                                    onclick: move |_| {
                                                        let c = cname2.clone();
                                                        blob_clear_confirm.set(None);
                                                        blob_clearing.write().insert(c.clone());
                                                        spawn(async move {
                                                            let c2 = c.clone();
                                                            let res = tokio::task::spawn_blocking(move || {
                                                                azurite_client::clear_container(&c2)
                                                            }).await;
                                                            // Only empty the displayed list when the delete
                                                            // actually succeeded — blindly clearing it used to
                                                            // make a failed clear look like it had worked.
                                                            match res {
                                                                Ok(Ok(n)) => {
                                                                    if let Some(ref mut list) = *blob_containers.write() {
                                                                        if let Some(entry) = list.iter_mut().find(|(n, _)| n == &c) {
                                                                            entry.1.clear();
                                                                        }
                                                                    }
                                                                    status.set(Some((format!("🗑 {c}: {n} blob(s) deleted"), false)));
                                                                }
                                                                Ok(Err(e)) => status.set(Some((format!("❌ {c}: {e}"), true))),
                                                                Err(e)     => status.set(Some((format!("❌ {c}: clear task failed: {e}"), true))),
                                                            }
                                                            blob_clearing.write().remove(&c);
                                                        });
                                                    },
                                                    if is_clearing { "🗑 …" } else { "🗑 Confirm?" }
                                                }
                                                button {
                                                    class: "btn btn-small",
                                                    onclick: move |_| blob_clear_confirm.set(None),
                                                    "Cancel"
                                                }
                                            } else {
                                                button {
                                                    class: "btn btn-small",
                                                    disabled: is_clearing || is_uploading || is_empty,
                                                    title: if is_empty { "Container is empty" } else { "Delete all blobs in this container" },
                                                    onclick: move |_| {
                                                        blob_clear_confirm.set(Some(cname4.clone()));
                                                    },
                                                    if is_clearing { "🗑 …" } else { "🗑 Clear" }
                                                }
                                            }
                                        }
                                    }
                                    if import_open {
                                        {
                                            let picked = import_path.read().clone();
                                            let has_file = picked.is_some();
                                            let file_label = picked.as_deref()
                                                .and_then(|p| p.rsplit('/').next())
                                                .unwrap_or("Choose file…")
                                                .to_string();
                                            let preview = clean_blob_path(&import_name.read());
                                            let ready = has_file && !preview.is_empty();
                                            let c_up = cname_form.clone();
                                            rsx! {
                                                div { class: "blob-import-form",
                                                    div { class: "blob-import-row",
                                                        button {
                                                            class: "btn btn-small",
                                                            title: "Pick a local file to import",
                                                            onclick: move |_| {
                                                                spawn(async move {
                                                                    if let Some(f) = rfd::AsyncFileDialog::new().pick_file().await {
                                                                        let p = f.path().to_string_lossy().to_string();
                                                                        // Prefill the target with the picked file name; the user
                                                                        // prepends a folder (e.g. "payments/") if they want one.
                                                                        if import_name.read().trim().is_empty() {
                                                                            import_name.set(f.file_name());
                                                                        }
                                                                        import_path.set(Some(p));
                                                                    }
                                                                });
                                                            },
                                                            "📄 {file_label}"
                                                        }
                                                    }
                                                    div { class: "blob-import-row",
                                                        input {
                                                            class: "blob-import-input",
                                                            placeholder: "path/name  e.g.  payments/PAYMENT_TEST.csv",
                                                            value: "{import_name}",
                                                            oninput: move |e| import_name.set(e.value()),
                                                        }
                                                    }
                                                    div { class: "blob-import-row",
                                                        span { class: "blob-import-preview",
                                                            if ready { "→ {c_up}/{preview}" } else { "" }
                                                        }
                                                        button {
                                                            class: "btn btn-small btn-run",
                                                            disabled: !ready || is_uploading,
                                                            title: "Upload the file to this path in one operation",
                                                            onclick: move |_| {
                                                                let Some(src) = import_path.read().clone() else { return };
                                                                let blob_name = clean_blob_path(&import_name.read());
                                                                if blob_name.is_empty() { return; }
                                                                let c = cname_form.clone();
                                                                blob_uploading.write().insert(c.clone());
                                                                import_target.set(None);
                                                                spawn(async move {
                                                                    let (c2, bn) = (c.clone(), blob_name.clone());
                                                                    let res = tokio::task::spawn_blocking(move || {
                                                                        azurite_client::upload_blob(&c2, &src, &bn)
                                                                    }).await;
                                                                    match res {
                                                                        Ok(Ok(())) => status.set(Some((format!("↑ {c}/{blob_name} imported"), false))),
                                                                        Ok(Err(e)) => status.set(Some((format!("❌ import: {e}"), true))),
                                                                        Err(e)     => status.set(Some((format!("❌ import task failed: {e}"), true))),
                                                                    }
                                                                    blob_expanded.write().insert(c.clone());
                                                                    let c3 = c.clone();
                                                                    if let Ok(Ok(updated)) = tokio::task::spawn_blocking(move || {
                                                                        azurite_client::list_blobs(&c3)
                                                                    }).await {
                                                                        if let Some(ref mut list) = *blob_containers.write() {
                                                                            if let Some(entry) = list.iter_mut().find(|(n, _)| n == &c) {
                                                                                entry.1 = updated;
                                                                            }
                                                                        }
                                                                    }
                                                                    blob_uploading.write().remove(&c);
                                                                });
                                                            },
                                                            "↑ Upload"
                                                        }
                                                        button {
                                                            class: "btn btn-small",
                                                            title: "Cancel",
                                                            onclick: move |_| import_target.set(None),
                                                            "✗"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if is_expanded {
                                        if blobs.is_empty() {
                                            div { class: "blob-list",
                                                div { class: "blob-empty", style: "padding:6px 0",
                                                    "Container is empty"
                                                }
                                            }
                                        } else {
                                            div { class: "blob-list",
                                                for b in &rows_with_folders(&blobs) {
                                                    {
                                                        let is_folder = b.name.ends_with("/.keep");
                                                        let (icon, display, full) = if is_folder {
                                                            let folder_path = b.name
                                                                .strip_suffix("/.keep")
                                                                .unwrap_or(&b.name);
                                                            let folder_name = folder_path
                                                                .rsplit('/')
                                                                .next()
                                                                .unwrap_or(folder_path);
                                                            ("📁", folder_name.to_string(), folder_path.to_string())
                                                        } else {
                                                            let name = b.name
                                                                .rsplit('/')
                                                                .next()
                                                                .unwrap_or(&b.name)
                                                                .to_string();
                                                            ("📄", name, b.name.clone())
                                                        };
                                                        let size_str = if is_folder {
                                                            String::new()
                                                        } else {
                                                            let kb = b.size as f64 / 1024.0;
                                                            if b.size < 1024 {
                                                                format!("{} B", b.size)
                                                            } else if kb < 1024.0 {
                                                                format!("{:.1} KB", kb)
                                                            } else {
                                                                format!("{:.1} MB", kb / 1024.0)
                                                            }
                                                        };
                                                        let is_nested = !is_folder && b.name.contains('/');
                                                        let row_cls = if is_folder {
                                                            "blob-row blob-folder-row"
                                                        } else if is_nested {
                                                            "blob-row blob-nested-row"
                                                        } else {
                                                            "blob-row"
                                                        };
                                                        let upload_key = format!("{}/{}", cname, full);
                                                        let is_folder_uploading = blob_uploading.read().contains(&upload_key);
                                                        let download_key = format!("{}/{}", cname, full);
                                                        let is_downloading = blob_downloading.read().contains(&download_key);
                                                        let rename_key    = format!("{}/{}", cname, full);
                                                        let is_renaming   = blob_renaming.read().contains(&rename_key);
                                                        let editing_name  = folder_rename.read().as_ref()
                                                            .map(|(c, p)| c == &cname && p == &full)
                                                            .unwrap_or(false);
                                                        let ct_for_rn     = cname.clone();
                                                        let path_for_rn   = full.clone();
                                                        let display_for_rn = display.clone();
                                                        let folder_prefix = full.clone();
                                                        let ct_for_folder = cname.clone();
                                                        let ct_for_dl = cname.clone();
                                                        let blob_name_dl = full.clone();
                                                        let display_dl = display.clone();
                                                        // A blob is "announceable" when some template's regex claims its
                                                        // name. Most blobs match nothing, so the button stays hidden
                                                        // rather than being shown-but-disabled on every row.
                                                        let event_key = format!("{}/{}", cname, full);
                                                        let is_sending_event = sending_event.read().contains(&event_key);
                                                        let matched: Option<(String, String)> = templates
                                                            .read()
                                                            .iter()
                                                            .find(|t| t.matches(&display))
                                                            .map(|t| (t.name.clone(), t.queue.clone()));
                                                        let tpl_name_for_send = matched.as_ref().map(|(n, _)| n.clone());
                                                        let send_display = display.clone();
                                                        rsx! {
                                                            div { class: "{row_cls}", title: "{full}",
                                                                span { class: "blob-row-icon", "{icon}" }
                                                                span { class: "blob-name", "{display}" }
                                                                span { class: "blob-size", "{size_str}" }
                                                                if let Some((tpl_name, queue)) = matched.clone() {
                                                                    button {
                                                                        class: "btn btn-small blob-event-btn",
                                                                        disabled: is_sending_event,
                                                                        title: "Send \"{tpl_name}\" event for this file to {queue}",
                                                                        onclick: move |_| {
                                                                            let key   = event_key.clone();
                                                                            let fname = send_display.clone();
                                                                            let name  = tpl_name_for_send.clone().unwrap_or_default();
                                                                            let mut status = status;
                                                                            sending_event.write().insert(key.clone());
                                                                            spawn(async move {
                                                                                let rendered = templates.read().iter()
                                                                                    .find(|t| t.name == name)
                                                                                    .ok_or_else(|| "template disappeared".to_string())
                                                                                    .and_then(|t| {
                                                                                        let ctx = crate::services::msg_template::RenderContext {
                                                                                            env: "DEV".into(),
                                                                                            blob_endpoint: String::new(),
                                                                                        };
                                                                                        t.render(&fname, &ctx).map(|b| (b, t.queue.clone()))
                                                                                    });
                                                                                match rendered {
                                                                                    Err(e) => status.set(Some((format!("Template '{name}': {e}"), true))),
                                                                                    Ok((body, queue)) => {
                                                                                        match crate::services::sb_amqp::send_amqp_message("localhost", &queue, &body).await {
                                                                                            Ok(()) => status.set(Some((format!("✅ Sent '{name}' for {fname} → {queue}"), false))),
                                                                                            Err(e) => status.set(Some((format!("Send to {queue} failed: {e}"), true))),
                                                                                        }
                                                                                    }
                                                                                }
                                                                                sending_event.write().remove(&key);
                                                                            });
                                                                        },
                                                                        if is_sending_event { "⚡ …" } else { "⚡" }
                                                                    }
                                                                }
                                                                if !is_folder {
                                                                    button {
                                                                        class: "btn btn-small blob-dl-btn",
                                                                        disabled: is_downloading,
                                                                        title: "Download blob",
                                                                        onclick: move |_| {
                                                                            let dk  = download_key.clone();
                                                                            let ct  = ct_for_dl.clone();
                                                                            let bn  = blob_name_dl.clone();
                                                                            let dn  = display_dl.clone();
                                                                            blob_downloading.write().insert(dk.clone());
                                                                            spawn(async move {
                                                                                if let Some(dest) = rfd::AsyncFileDialog::new()
                                                                                    .set_file_name(&dn)
                                                                                    .save_file().await
                                                                                {
                                                                                    let path = dest.path().to_string_lossy().to_string();
                                                                                    let _ = tokio::task::spawn_blocking(move || {
                                                                                        azurite_client::download_blob(&ct, &bn, &path)
                                                                                    }).await;
                                                                                }
                                                                                blob_downloading.write().remove(&dk);
                                                                            });
                                                                        },
                                                                        if is_downloading { "↓ …" } else { "↓" }
                                                                    }
                                                                }
                                                                if is_folder {
                                                                    button {
                                                                        class: "btn btn-small blob-folder-upload-btn",
                                                                        disabled: is_folder_uploading,
                                                                        title: "Upload a file into this folder",
                                                                        onclick: move |_| {
                                                                            let uk  = upload_key.clone();
                                                                            let ct  = ct_for_folder.clone();
                                                                            let pfx = folder_prefix.clone();
                                                                            blob_uploading.write().insert(uk.clone());
                                                                            spawn(async move {
                                                                                if let Some(file) = rfd::AsyncFileDialog::new().pick_file().await {
                                                                                    let path = file.path().to_string_lossy().to_string();
                                                                                    let blob_name = format!("{}/{}", pfx, file.file_name());
                                                                                    let ct2 = ct.clone();
                                                                                    let _ = tokio::task::spawn_blocking(move || {
                                                                                        azurite_client::upload_blob(&ct2, &path, &blob_name)
                                                                                    }).await;
                                                                                    blob_expanded.write().insert(ct.clone());
                                                                                    let ct3 = ct.clone();
                                                                                    if let Ok(Ok(updated)) = tokio::task::spawn_blocking(move || {
                                                                                        azurite_client::list_blobs(&ct3)
                                                                                    }).await {
                                                                                        if let Some(ref mut list) = *blob_containers.write() {
                                                                                            if let Some(entry) = list.iter_mut().find(|(n, _)| n == &ct) {
                                                                                                entry.1 = updated;
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                }
                                                                                blob_uploading.write().remove(&uk);
                                                                            });
                                                                        },
                                                                        if is_folder_uploading { "↑ …" } else { "↑ Upload" }
                                                                    }
                                                                    if editing_name {
                                                                        input {
                                                                            class: "blob-rename-input",
                                                                            value: "{rename_input}",
                                                                            oninput: move |e| rename_input.set(e.value()),
                                                                        }
                                                                        button {
                                                                            class: "btn btn-small",
                                                                            disabled: is_renaming,
                                                                            title: "Apply rename",
                                                                            onclick: move |_| {
                                                                                let ct   = ct_for_rn.clone();
                                                                                let old  = path_for_rn.clone();
                                                                                let key  = rename_key.clone();
                                                                                let newp = rename_last_segment(&old, &rename_input.read());
                                                                                folder_rename.set(None);
                                                                                blob_renaming.write().insert(key.clone());
                                                                                spawn(async move {
                                                                                    let (c2, o2, n2) = (ct.clone(), old.clone(), newp.clone());
                                                                                    let res = tokio::task::spawn_blocking(move || {
                                                                                        azurite_client::rename_virtual_folder(&c2, &o2, &n2)
                                                                                    }).await;
                                                                                    match res {
                                                                                        Ok(Ok(n)) => status.set(Some((
                                                                                            format!("✏️ {old} → {newp} ({n} blob(s) moved)"), false))),
                                                                                        Ok(Err(e)) => status.set(Some((format!("❌ rename: {e}"), true))),
                                                                                        Err(e)     => status.set(Some((format!("❌ rename task failed: {e}"), true))),
                                                                                    }
                                                                                    // Re-list so the tree reflects reality either way.
                                                                                    let c3 = ct.clone();
                                                                                    if let Ok(Ok(updated)) = tokio::task::spawn_blocking(move || {
                                                                                        azurite_client::list_blobs(&c3)
                                                                                    }).await {
                                                                                        if let Some(ref mut list) = *blob_containers.write() {
                                                                                            if let Some(entry) = list.iter_mut().find(|(n, _)| n == &ct) {
                                                                                                entry.1 = updated;
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                    blob_renaming.write().remove(&key);
                                                                                });
                                                                            },
                                                                            if is_renaming { "…" } else { "✓" }
                                                                        }
                                                                        button {
                                                                            class: "btn btn-small",
                                                                            title: "Cancel rename",
                                                                            onclick: move |_| folder_rename.set(None),
                                                                            "✗"
                                                                        }
                                                                    } else {
                                                                        button {
                                                                            class: "btn btn-small blob-rename-btn",
                                                                            disabled: is_renaming,
                                                                            title: "Rename this folder (copies then deletes — blob storage has no real folders)",
                                                                            onclick: move |_| {
                                                                                rename_input.set(display_for_rn.clone());
                                                                                folder_rename.set(Some((ct_for_rn.clone(), path_for_rn.clone())));
                                                                            },
                                                                            if is_renaming { "✏️ …" } else { "✏️" }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    } // end blob-container-wrapper
                                }
                            }
                        }
                    } else {
                        div { class: "blob-empty", "Click ⟳ Refresh to list containers" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod rename_segment_tests {
    use super::rename_last_segment;

    #[test]
    fn renames_only_the_last_segment_keeping_the_parent() {
        assert_eq!(rename_last_segment("archive/payments", "pay"), "archive/pay");
        assert_eq!(rename_last_segment("a/b/c", "z"), "a/b/z");
    }

    #[test]
    fn top_level_folder_has_no_parent_to_keep() {
        assert_eq!(rename_last_segment("payments", "pay"), "pay");
    }

    #[test]
    fn trims_whitespace_and_stray_slashes_from_input() {
        assert_eq!(rename_last_segment("archive/payments", "  pay  "), "archive/pay");
        assert_eq!(rename_last_segment("archive/payments", "/pay/"), "archive/pay");
        assert_eq!(rename_last_segment("payments/", "pay"), "pay");
    }
}

#[cfg(test)]
mod import_path_tests {
    use super::clean_blob_path;

    #[test]
    fn bare_name_lands_at_container_root() {
        assert_eq!(clean_blob_path("file.csv"), "file.csv");
        assert_eq!(clean_blob_path("  file.csv  "), "file.csv");
    }

    #[test]
    fn single_and_nested_folders_are_preserved() {
        assert_eq!(clean_blob_path("payments/f.csv"), "payments/f.csv");
        assert_eq!(clean_blob_path("a/b/c/f.csv"), "a/b/c/f.csv");
    }

    #[test]
    fn strips_stray_slashes_and_blank_segments() {
        assert_eq!(clean_blob_path("/payments/f.csv"), "payments/f.csv");
        assert_eq!(clean_blob_path("payments//f.csv"), "payments/f.csv");
        assert_eq!(clean_blob_path(" payments / f.csv "), "payments/f.csv");
    }

    #[test]
    fn empty_or_slashes_only_is_empty() {
        assert_eq!(clean_blob_path(""), "");
        assert_eq!(clean_blob_path("///"), "");
    }
}

#[cfg(test)]
mod folder_rows_tests {
    use super::rows_with_folders;
    use crate::services::azure_cli::BlobInfo;

    fn b(name: &str) -> BlobInfo { BlobInfo { name: name.into(), size: 1 } }
    fn names(v: Vec<BlobInfo>) -> Vec<String> { v.into_iter().map(|x| x.name).collect() }

    #[test]
    fn nested_file_gets_a_synthetic_folder_header() {
        let out = names(rows_with_folders(&[b("payments/PAYMENT_TEST.csv")]));
        assert_eq!(out, vec![
            "payments/.keep".to_string(),          // synthetic folder header
            "payments/PAYMENT_TEST.csv".to_string(),
        ]);
    }

    #[test]
    fn deep_nesting_gets_a_header_per_ancestor() {
        let out = names(rows_with_folders(&[b("a/b/c.csv")]));
        assert_eq!(out, vec![
            "a/.keep".to_string(),
            "a/b/.keep".to_string(),
            "a/b/c.csv".to_string(),
        ]);
    }

    #[test]
    fn real_keep_marker_is_not_duplicated() {
        let out = names(rows_with_folders(&[b("payments/.keep"), b("payments/f.csv")]));
        assert_eq!(out, vec!["payments/.keep".to_string(), "payments/f.csv".to_string()]);
    }

    #[test]
    fn root_files_are_untouched() {
        let out = names(rows_with_folders(&[b("root.csv"), b("other.csv")]));
        assert_eq!(out, vec!["other.csv".to_string(), "root.csv".to_string()]);
    }

    #[test]
    fn shared_folder_yields_one_header() {
        let out = names(rows_with_folders(&[b("p/a.csv"), b("p/b.csv")]));
        assert_eq!(out, vec!["p/.keep".to_string(), "p/a.csv".to_string(), "p/b.csv".to_string()]);
    }
}
