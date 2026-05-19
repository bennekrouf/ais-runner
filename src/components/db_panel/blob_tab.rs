use dioxus::prelude::*;
use std::collections::HashSet;
use std::collections::HashMap;
use crate::services::{
    azure_cli::BlobInfo,
    azurite_client,
};

async fn do_create_container_or_folder(input: String) -> Result<(), String> {
    let (container, folder) = match input.find('/') {
        None => (input.clone(), None),
        Some(pos) => {
            let c = input[..pos].trim().to_string();
            let f = input[pos + 1..].trim().trim_end_matches('/').to_string();
            (c, if f.is_empty() { None } else { Some(f) })
        }
    };
    let c2 = container.clone();
    tokio::task::spawn_blocking(move || azurite_client::create_container(&c2))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
        .map_err(|e| format!("Create container failed: {}", e))?;
    if let Some(prefix) = folder {
        let c3 = container.clone();
        tokio::task::spawn_blocking(move || {
            azurite_client::create_virtual_folder(&c3, &prefix)
        })
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
        .map_err(|e| format!("Create folder failed: {}", e))?;
    }
    Ok(())
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
    pub blob_edits:      Signal<HashMap<String, String>>,
    pub webjobs_edit:    Signal<String>,
    pub is_open:         Signal<bool>,
    pub active_tab:      Signal<&'static str>,
    pub status:          Signal<Option<(String, bool)>>,
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
                    span { class: "db-section-title", "🗄 Blob Storage" }
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
                        placeholder: "container  or  container/folder/subfolder",
                        value: "{new_container_name.read()}",
                        oninput: move |e| new_container_name.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                let name = new_container_name.read().trim().to_string();
                                if !name.is_empty() && !*blob_creating.read() {
                                    blob_creating.set(true);
                                    status.set(None);
                                    spawn(async move {
                                        if let Err(msg) = do_create_container_or_folder(name).await {
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
                                    if let Err(msg) = do_create_container_or_folder(name).await {
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
                                let cname3    = cname.clone();
                                let cname4    = cname.clone();
                                let cname_exp = cname.clone();
                                let is_expanded  = blob_expanded.read().contains(&cname);
                                let is_clearing  = blob_clearing.read().contains(&cname);
                                let is_uploading = blob_uploading.read().contains(&cname);
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
                                                class: "btn btn-small",
                                                disabled: is_uploading || is_clearing,
                                                title: "Upload a file into this container",
                                                onclick: move |_| {
                                                    let c = cname3.clone();
                                                    blob_uploading.write().insert(c.clone());
                                                    spawn(async move {
                                                        if let Some(file) = rfd::AsyncFileDialog::new().pick_file().await {
                                                            let path = file.path().to_string_lossy().to_string();
                                                            let name = file.file_name();
                                                            let c2   = c.clone();
                                                            let _ = tokio::task::spawn_blocking(move || {
                                                                azurite_client::upload_blob(&c2, &path, &name)
                                                            }).await;
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
                                                        }
                                                        blob_uploading.write().remove(&c);
                                                    });
                                                },
                                                if is_uploading { "↑ …" } else { "↑ Upload" }
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
                                                            let _ = tokio::task::spawn_blocking(move || {
                                                                azurite_client::clear_container(&c2)
                                                            }).await;
                                                            if let Some(ref mut list) = *blob_containers.write() {
                                                                if let Some(entry) = list.iter_mut().find(|(n, _)| n == &c) {
                                                                    entry.1.clear();
                                                                }
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
                                    if is_expanded {
                                        if blobs.is_empty() {
                                            div { class: "blob-list",
                                                div { class: "blob-empty", style: "padding:6px 0",
                                                    "Container is empty"
                                                }
                                            }
                                        } else {
                                            div { class: "blob-list",
                                                for b in &blobs {
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
                                                        let folder_prefix = full.clone();
                                                        let ct_for_folder = cname.clone();
                                                        let ct_for_dl = cname.clone();
                                                        let blob_name_dl = full.clone();
                                                        let display_dl = display.clone();
                                                        rsx! {
                                                            div { class: "{row_cls}", title: "{full}",
                                                                span { class: "blob-row-icon", "{icon}" }
                                                                span { class: "blob-name", "{display}" }
                                                                span { class: "blob-size", "{size_str}" }
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
