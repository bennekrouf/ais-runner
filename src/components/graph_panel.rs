use dioxus::prelude::*;
use std::collections::HashSet;
use std::path::Path;
use std::collections::HashMap;
use crate::services::config;

#[derive(Props, Clone, PartialEq)]
pub struct GraphPanelProps {
    pub logic_apps_dir: String,
    pub is_light: Signal<bool>,
}

/// One discovered chain: label (root workflow name) + all workflow names in it
#[derive(Clone, Debug, PartialEq)]
struct ChainInfo {
    label: String,
    workflows: HashSet<String>,
    step_count: usize,
}

#[derive(Clone, PartialEq)]
struct GraphData {
    nodes_json: String,
    edges_json: String,
    node_objects: Vec<(String, String)>,
    edge_objects: Vec<(String, String, String)>,
    chains: Vec<ChainInfo>,
}

#[component]
pub fn GraphPanel(props: GraphPanelProps) -> Element {
    let dir = props.logic_apps_dir.clone();
    let is_light = props.is_light;

    // ── Restore saved preferences ────────────────────────────────────────────
    let saved = config::load().get_graph_prefs(&dir);

    let mut graph_data      = use_signal(|| Option::<GraphData>::None);
    let mut selected_chain  = use_signal(|| saved.selected_chain.clone().unwrap_or_else(|| "All".to_string()));
    let mut show_all_pills  = use_signal(|| false);
    let mut excluded_nodes: Signal<HashSet<String>> = use_signal(|| saved.excluded_nodes.clone());
    let mut filter_open     = use_signal(|| false);
    let mut filter_search   = use_signal(String::new);
    // Incrementing this re-runs the data-load effect
    let mut refresh_tick: Signal<u32> = use_signal(|| 0);

    // ── Persist preferences whenever they change ─────────────────────────────
    use_effect({
        let dir = dir.clone();
        move || {
            let sel  = selected_chain.read().clone();
            let excl = excluded_nodes.read().clone();
            let d    = dir.clone();
            spawn(async move {
                tokio::task::spawn_blocking(move || {
                    let mut cfg = config::load();
                    cfg.set_graph_prefs(d, config::GraphPrefs {
                        selected_chain: if sel == "All" { None } else { Some(sel) },
                        excluded_nodes: excl,
                    });
                    config::save(&cfg);
                }).await.ok();
            });
        }
    });

    use_effect({
        let dir = dir.clone();
        move || {
            let _tick = *refresh_tick.read(); // reactive dependency
            let d = dir.clone();
            graph_data.set(None);             // show "Loading…" while scanning
            spawn(async move {
                let data = tokio::task::spawn_blocking(move || {
                    build_graph_data(&d)
                }).await.ok().flatten();
                graph_data.set(data);
            });
        }
    });

    let light = *is_light.read();
    let data = graph_data.read();
    let theme = Theme::from_light(light);
    let sel = selected_chain.read().clone();

    match data.as_ref() {
        None => rsx! {
            div { id: "graph-panel",
                style: "display:flex; align-items:center; justify-content:center; flex:1; color:{theme.text_muted};",
                "Loading graph…"
            }
        },
        Some(data) if data.node_objects.is_empty() => rsx! {
            div { id: "graph-panel",
                style: "display:flex; align-items:center; justify-content:center; flex:1; color:{theme.text_muted};",
                "No workflows found"
            }
        },
        Some(data) => {
            // Clone data for the effect closure
            let eff_data = data.clone();
            use_effect(move || {
                let sel      = selected_chain.read().clone();
                let light    = *is_light.read();
                let excluded = excluded_nodes.read().clone();
                let data_ref = eff_data.clone();

                spawn(async move {
                    // Build the D3 script off the GUI thread — avoids freezing
                    // the render loop on large graphs or slow WebView JS injection.
                    let script = tokio::task::spawn_blocking(move || {
                        let theme = Theme::from_light(light);

                        let chain_ids: HashSet<String> = if sel == "All" {
                            data_ref.node_objects.iter().map(|(id, _)| id.clone()).collect()
                        } else if let Some(chain) = data_ref.chains.iter().find(|c| c.label == sel) {
                            chain.workflows.clone()
                        } else {
                            data_ref.node_objects.iter().map(|(id, _)| id.clone()).collect()
                        };

                        let visible: HashSet<&str> = chain_ids.iter()
                            .filter(|id| !excluded.contains(*id))
                            .map(|s| s.as_str())
                            .collect();

                        let nodes: Vec<&str> = data_ref.node_objects.iter()
                            .filter(|(id, _)| visible.contains(id.as_str()))
                            .map(|(_, json)| json.as_str())
                            .collect();
                        let edges: Vec<&str> = data_ref.edge_objects.iter()
                            .filter(|(from, to, _)| visible.contains(from.as_str()) && visible.contains(to.as_str()))
                            .map(|(_, _, json)| json.as_str())
                            .collect();

                        let fn_str = format!("[{}]", nodes.join(","));
                        let fe_str = format!("[{}]", edges.join(","));
                        build_d3_script(&fn_str, &fe_str, &theme)
                    }).await.unwrap_or_default();

                    document::eval(&script);
                });
            });

            let chains = data.chains.clone();

            rsx! {
                div { id: "graph-panel",
                    style: "display:flex; flex:1; flex-direction:column; position:relative; background:{theme.bg}; overflow:hidden;",

                    // ── Title + chain pills — top bar ─────────────────
                    div {
                        style: "display:flex; align-items:center; gap:8px; padding:10px 16px 8px 16px; z-index:6; position:relative; flex-shrink:0;",
                        // Title
                        span { style: "color:{theme.accent}; font-size:14px; font-weight:600; white-space:nowrap; margin-right:4px;",
                            "ais-chain"
                        }
                        // Refresh button
                        button {
                            title: "Re-scan workflows",
                            style: "padding:2px 7px; font-size:11px; border:1px solid {theme.border}; background:transparent; color:{theme.text_muted}; cursor:pointer; border-radius:10px; font-family:inherit; line-height:1.3; flex-shrink:0;",
                            onclick: move |_| {
                                selected_chain.set("All".into());
                                excluded_nodes.write().clear();
                                refresh_tick.set(refresh_tick() + 1);
                            },
                            "⟳"
                        }
                        // ── Filter toggle button ──────────────────────────
                        {
                            let excl_count = excluded_nodes.read().len();
                            let open = *filter_open.read();
                            let btn_bg  = if open || excl_count > 0 { theme.accent } else { "transparent" };
                            let btn_col = if open || excl_count > 0 {
                                if light { "#fff" } else { "#0d1117" }
                            } else { theme.text_muted };
                            rsx! {
                                button {
                                    title: "Show / hide nodes",
                                    style: "padding:2px 9px; font-size:11px; font-weight:500; border:1px solid {theme.border}; background:{btn_bg}; color:{btn_col}; cursor:pointer; border-radius:10px; font-family:inherit; line-height:1.3; flex-shrink:0;",
                                    onclick: move |_| filter_open.set(!filter_open()),
                                    if excl_count > 0 { "⊞ Filter ({excl_count} hidden)" } else { "⊞ Filter" }
                                }
                            }
                        }
                        // Pills
                        {
                            let max_visible: usize = 10;
                            let expanded = *show_all_pills.read();
                            let total = chains.len();
                            let visible_chains: Vec<&ChainInfo> = if expanded || total <= max_visible {
                                chains.iter().collect()
                            } else {
                                chains.iter().take(max_visible).collect()
                            };
                            let hidden_count = if expanded || total <= max_visible { 0 } else { total - max_visible };

                            // "All" pill
                            let all_active = sel == "All";
                            let (all_bg, all_fg, all_bdr) = if all_active {
                                (theme.accent, if light { "#fff" } else { "#0d1117" }, theme.accent)
                            } else {
                                (theme.panel_bg, theme.text_muted, theme.border)
                            };

                            rsx! {
                                div {
                                    style: "display:flex; flex-wrap:wrap; gap:5px; align-items:center;",
                                    button {
                                        style: "padding:2px 9px; font-size:11px; font-weight:500; border:1px solid {all_bdr}; background:{all_bg}; color:{all_fg}; cursor:pointer; border-radius:10px; font-family:inherit; line-height:1.3;",
                                        onclick: move |_| selected_chain.set("All".into()),
                                        "All"
                                    }
                                    for chain in visible_chains.iter() {
                                        {
                                            let label = chain.label.clone();
                                            let count = chain.step_count;
                                            let is_active = sel == label;
                                            let (bg, fg, bdr) = if is_active {
                                                (theme.accent, if light { "#fff" } else { "#0d1117" }, theme.accent)
                                            } else {
                                                (theme.panel_bg, theme.text_muted, theme.border)
                                            };
                                            let lbl = label.clone();
                                            rsx! {
                                                button {
                                                    key: "{label}",
                                                    style: "padding:2px 8px; font-size:10px; font-weight:400; border:1px solid {bdr}; background:{bg}; color:{fg}; cursor:pointer; border-radius:10px; font-family:inherit; line-height:1.3; max-width:170px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;",
                                                    title: "{label} — {count} workflows",
                                                    onclick: move |_| selected_chain.set(lbl.clone()),
                                                    "{label} ({count})"
                                                }
                                            }
                                        }
                                    }
                                    if hidden_count > 0 {
                                        button {
                                            style: "padding:2px 8px; font-size:10px; font-weight:500; border:1px solid {theme.border}; background:transparent; color:{theme.accent}; cursor:pointer; border-radius:10px; font-family:inherit; line-height:1.3;",
                                            onclick: move |_| show_all_pills.set(true),
                                            "+{hidden_count} more"
                                        }
                                    }
                                    if expanded && total > max_visible {
                                        button {
                                            style: "padding:2px 8px; font-size:10px; font-weight:500; border:1px solid {theme.border}; background:transparent; color:{theme.accent}; cursor:pointer; border-radius:10px; font-family:inherit; line-height:1.3;",
                                            onclick: move |_| show_all_pills.set(false),
                                            "less"
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div { style: "position:absolute; bottom:16px; left:16px; background:{theme.panel_bg}; border:1px solid {theme.border}; border-radius:8px; padding:12px 16px; color:{theme.text_muted}; font-size:11px; z-index:5;",
                        div { style: "margin:4px 0; display:flex; align-items:center; gap:8px;",
                            span { style: "width:12px; height:12px; border-radius:50%; display:inline-block; background:{theme.node_fill_http}; border:2px solid {theme.node_http}; box-sizing:border-box;" }
                            "HTTP trigger"
                        }
                        div { style: "margin:4px 0; display:flex; align-items:center; gap:8px;",
                            span { style: "width:12px; height:12px; border-radius:50%; display:inline-block; background:{theme.node_fill_queue}; border:2px solid {theme.node_queue}; box-sizing:border-box;" }
                            "Queue trigger"
                        }
                        div { style: "margin:4px 0; display:flex; align-items:center; gap:8px;",
                            span { style: "width:12px; height:12px; border-radius:50%; display:inline-block; background:{theme.node_fill_blob}; border:2px solid {theme.node_blob}; box-sizing:border-box;" }
                            "Blob trigger"
                        }
                        div { style: "margin:4px 0; display:flex; align-items:center; gap:8px;",
                            span { style: "width:12px; height:12px; border-radius:50%; display:inline-block; background:{theme.node_fill_recurrence}; border:2px solid {theme.node_recurrence}; box-sizing:border-box;" }
                            "Recurrence"
                        }
                        div { style: "margin:4px 0; display:flex; align-items:center; gap:8px;",
                            span { style: "width:24px; height:2px; display:inline-block; background:{theme.edge_queue};" }
                            "Queue link"
                        }
                        div { style: "margin:4px 0; display:flex; align-items:center; gap:8px;",
                            span { style: "width:24px; height:2px; display:inline-block; background:{theme.edge_eventgrid};" }
                            "EventGrid"
                        }
                        div { style: "margin:4px 0; display:flex; align-items:center; gap:8px;",
                            span { style: "width:24px; height:2px; display:inline-block; background:{theme.edge_invoke}; opacity:0.5;" }
                            "Invoke (child)"
                        }
                    }
                    // ── Filter panel (floating, top-right) ───────────────
                    if *filter_open.read() {
                        if let Some(gd) = graph_data.read().as_ref() {
                            {
                                let all_nodes: Vec<String> = {
                                    let mut v: Vec<String> = gd.node_objects.iter()
                                        .map(|(id, _)| id.clone()).collect();
                                    v.sort();
                                    v
                                };
                                // Build group map for trigger-type dots
                                let group_map: HashMap<String, String> = gd.node_objects.iter()
                                    .filter_map(|(id, json)| {
                                        let g = json.split(r#""group": ""#).nth(1)?
                                            .split('"').next()?.to_string();
                                        Some((id.clone(), g))
                                    }).collect();

                                let search = filter_search.read().to_lowercase();
                                let visible_nodes: Vec<String> = all_nodes.iter()
                                    .filter(|n| search.is_empty() || n.to_lowercase().contains(&search))
                                    .cloned().collect();

                                let excl_count = excluded_nodes.read().len();
                                let total = all_nodes.len();

                                let dot_color = |group: &str| match group {
                                    "http"       => theme.node_http,
                                    "queue"      => theme.node_queue,
                                    "blob"       => theme.node_blob,
                                    "recurrence" => theme.node_recurrence,
                                    _            => theme.node_generic,
                                };

                                rsx! {
                                    div {
                                        style: "position:absolute; top:44px; left:16px; z-index:20; \
                                                background:{theme.panel_bg}; border:1px solid {theme.border}; \
                                                border-radius:10px; width:260px; max-height:420px; \
                                                display:flex; flex-direction:column; \
                                                box-shadow: 0 4px 24px rgba(0,0,0,0.25);",

                                        // Header
                                        div {
                                            style: "padding:10px 12px 8px; border-bottom:1px solid {theme.border}; display:flex; align-items:center; justify-content:space-between; flex-shrink:0;",
                                            span { style: "font-size:12px; font-weight:600; color:{theme.text};",
                                                "{total} nodes"
                                                if excl_count > 0 { " · {excl_count} hidden" }
                                            }
                                            div { style: "display:flex; gap:6px;",
                                                if excl_count > 0 {
                                                    button {
                                                        style: "font-size:10px; color:{theme.accent}; background:none; border:none; cursor:pointer; padding:0; font-family:inherit;",
                                                        onclick: move |_| excluded_nodes.write().clear(),
                                                        "Show all"
                                                    }
                                                }
                                                button {
                                                    style: "font-size:10px; color:{theme.text_muted}; background:none; border:none; cursor:pointer; padding:0; font-family:inherit;",
                                                    onclick: {
                                                        let nodes = all_nodes.clone();
                                                        move |_| {
                                                            let mut ex = excluded_nodes.write();
                                                            if ex.len() == nodes.len() {
                                                                ex.clear();
                                                            } else {
                                                                *ex = nodes.iter().cloned().collect();
                                                            }
                                                        }
                                                    },
                                                    if excl_count == total { "Show all" } else { "Hide all" }
                                                }
                                            }
                                        }

                                        // Search
                                        div { style: "padding:7px 10px; flex-shrink:0;",
                                            input {
                                                style: "width:100%; box-sizing:border-box; background:{theme.bg}; border:1px solid {theme.border}; border-radius:6px; padding:4px 8px; font-size:11px; color:{theme.text}; font-family:inherit; outline:none;",
                                                placeholder: "Search nodes…",
                                                value: "{filter_search}",
                                                oninput: move |e| filter_search.set(e.value()),
                                            }
                                        }

                                        // Node list
                                        div { style: "overflow-y:auto; flex:1; padding:4px 0;",
                                            for node_id in visible_nodes.iter() {
                                                {
                                                    let nid    = node_id.clone();
                                                    let nid2   = nid.clone();
                                                    let is_vis = !excluded_nodes.read().contains(&nid);
                                                    let grp    = group_map.get(&nid).map(|s| s.as_str()).unwrap_or("generic");
                                                    let dot_c  = dot_color(grp);
                                                    let row_bg = if is_vis { "transparent" } else {
                                                        if light { "rgba(0,0,0,0.04)" } else { "rgba(255,255,255,0.03)" }
                                                    };
                                                    let lbl_op = if is_vis { "1" } else { "0.4" };
                                                    rsx! {
                                                        label {
                                                            key: "{nid}",
                                                            style: "display:flex; align-items:center; gap:8px; padding:4px 12px; cursor:pointer; background:{row_bg}; user-select:none;",
                                                            input {
                                                                r#type: "checkbox",
                                                                checked: is_vis,
                                                                style: "cursor:pointer; flex-shrink:0;",
                                                                onchange: move |e| {
                                                                    let mut ex = excluded_nodes.write();
                                                                    if e.checked() { ex.remove(&nid2); }
                                                                    else           { ex.insert(nid2.clone()); }
                                                                },
                                                            }
                                                            span { style: "width:8px; height:8px; border-radius:50%; background:{dot_c}; flex-shrink:0;" }
                                                            span {
                                                                style: "font-size:11px; color:{theme.text}; opacity:{lbl_op}; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;",
                                                                "{nid}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            if visible_nodes.is_empty() {
                                                div { style: "padding:12px; font-size:11px; color:{theme.text_muted}; text-align:center;",
                                                    "No matches"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    svg { id: "graph-svg", style: "width:100%; height:100%;" }
                    div { id: "graph-tooltip",
                        style: "position:fixed; background:{theme.panel_bg}; border:1px solid {theme.border}; color:{theme.text}; padding:8px 12px; border-radius:6px; font-size:12px; pointer-events:none; z-index:10; display:none;",
                    }
                }
            }
        }
    }
}

// ── Theme ────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
struct Theme {
    bg: &'static str,
    panel_bg: &'static str,
    border: &'static str,
    text: &'static str,
    text_muted: &'static str,
    accent: &'static str,
    node_http: &'static str,
    node_queue: &'static str,
    node_blob: &'static str,
    node_recurrence: &'static str,
    node_generic: &'static str,
    node_fill_http: &'static str,
    node_fill_queue: &'static str,
    node_fill_blob: &'static str,
    node_fill_recurrence: &'static str,
    node_fill_generic: &'static str,
    edge_queue: &'static str,
    edge_eventgrid: &'static str,
    edge_invoke: &'static str,
    edge_function: &'static str,
    edge_other: &'static str,
    label_color: &'static str,
}

impl Theme {
    fn from_light(light: bool) -> Self {
        if light {
            Theme {
                bg: "#ffffff",
                panel_bg: "#f6f8fa",
                border: "#d0d7de",
                text: "#1f2328",
                text_muted: "#656d76",
                accent: "#0969da",
                node_http: "#1a7f37",
                node_queue: "#0969da",
                node_blob: "#9a6700",
                node_recurrence: "#8250df",
                node_generic: "#656d76",
                node_fill_http: "#dafbe1",
                node_fill_queue: "#ddf4ff",
                node_fill_blob: "#fff8c5",
                node_fill_recurrence: "#fbefff",
                node_fill_generic: "#eaeef2",
                edge_queue: "#b08800",
                edge_eventgrid: "#8250df",
                edge_invoke: "#0969da",
                edge_function: "#1a7f37",
                edge_other: "#8c959f",
                label_color: "#656d76",
            }
        } else {
            Theme {
                bg: "#0d1117",
                panel_bg: "#161b22",
                border: "#30363d",
                text: "#c9d1d9",
                text_muted: "#8b949e",
                accent: "#58a6ff",
                node_http: "#3fb950",
                node_queue: "#58a6ff",
                node_blob: "#d29922",
                node_recurrence: "#bc8cff",
                node_generic: "#8b949e",
                node_fill_http: "#1a3a2a",
                node_fill_queue: "#1a2a3a",
                node_fill_blob: "#2a2a1a",
                node_fill_recurrence: "#2a1a2a",
                node_fill_generic: "#1a1a2a",
                edge_queue: "#f0c040",
                edge_eventgrid: "#c060e0",
                edge_invoke: "#4080d0",
                edge_function: "#40c080",
                edge_other: "#888",
                label_color: "#8b949e",
            }
        }
    }
}

// ── Data ─────────────────────────────────────────────────────────────────────

fn build_graph_data(logic_apps_dir: &str) -> Option<GraphData> {
    let base = Path::new(logic_apps_dir);
    let path = if base.join("logic_apps").exists() {
        base.join("logic_apps")
    } else if base.join("logic-apps").exists() {
        base.join("logic-apps")
    } else {
        base.to_path_buf()
    };

    let workflows = ais_chain::parser::parse_all(&path);
    if workflows.is_empty() { return None; }

    let mut manual_links = Vec::new();
    let auto_links_path = path.join(".ais-chain");
    if auto_links_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&auto_links_path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    manual_links.push(trimmed.to_string());
                }
            }
        }
    }

    let graph = ais_chain::graph::build(&workflows, &manual_links);
    let edges = graph.all_edges();

    let raw_chains = graph.find_chains();
    let chains: Vec<ChainInfo> = raw_chains.iter()
        .filter(|c| c.steps.len() > 1)
        .map(|c| ChainInfo {
            label: c.steps[0].workflow.clone(),
            workflows: c.steps.iter().map(|s| s.workflow.clone()).collect(),
            step_count: c.steps.len(),
        })
        .collect();

    let mut nodes_set = std::collections::HashSet::new();
    for (from, to, _) in &edges {
        nodes_set.insert(*from);
        nodes_set.insert(*to);
    }
    for wf in &workflows {
        nodes_set.insert(&wf.name);
    }

    let mut node_objects: Vec<(String, String)> = Vec::new();
    for name in &nodes_set {
        let wf = workflows.iter().find(|w| w.name == **name);
        let trigger = wf.map(|w| {
            w.triggers.iter()
                .map(|t| format!("{}:{}", t.kind, t.target))
                .collect::<Vec<_>>()
                .join(", ")
        }).unwrap_or_default();

        let group = if trigger.contains("http") { "http" }
            else if trigger.contains("queue") { "queue" }
            else if trigger.contains("blob") { "blob" }
            else if trigger.contains("recurrence") { "recurrence" }
            else { "generic" };

        let json = format!(r#"{{ "id": "{name}", "trigger": "{trigger}", "group": "{group}" }}"#);
        node_objects.push((name.to_string(), json));
    }

    let mut edge_objects: Vec<(String, String, String)> = Vec::new();
    for (from, to, link) in &edges {
        let kind = if link.starts_with("queue:") { "queue" }
            else if *link == "EventGrid" { "eventgrid" }
            else if *link == "invoke" { "invoke" }
            else if link.starts_with("function:") { "function" }
            else { "other" };
        let json = format!(r#"{{ "source": "{from}", "target": "{to}", "label": "{link}", "kind": "{kind}" }}"#);
        edge_objects.push((from.to_string(), to.to_string(), json));
    }

    let nodes_json = format!("[{}]", node_objects.iter().map(|(_, j)| j.as_str()).collect::<Vec<_>>().join(","));
    let edges_json = format!("[{}]", edge_objects.iter().map(|(_, _, j)| j.as_str()).collect::<Vec<_>>().join(","));

    Some(GraphData { nodes_json, edges_json, node_objects, edge_objects, chains })
}

// ── D3 script ────────────────────────────────────────────────────────────────

fn build_d3_script(nodes_json: &str, edges_json: &str, theme: &Theme) -> String {
    let nodes = nodes_json.replace('\\', "\\\\");
    let edges = edges_json.replace('\\', "\\\\");

    format!(r#"
(function() {{
    var oldSvg = document.getElementById('graph-svg');
    if (!oldSvg) return;
    oldSvg.innerHTML = '';

    function run() {{
        var nodes = {nodes};
        var links = {edges};

        var svg = d3.select('#graph-svg');
        var rect = oldSvg.getBoundingClientRect();
        var width = rect.width || 800;
        var height = rect.height || 600;

        var g = svg.append('g');

        svg.call(d3.zoom()
            .scaleExtent([0.1, 4])
            .on('zoom', function(e) {{ g.attr('transform', e.transform); }}));

        var defs = svg.append('defs');
        var edgeColors = {{
            queue:     '{edge_queue}',
            eventgrid: '{edge_eventgrid}',
            invoke:    '{edge_invoke}',
            'function':'{edge_function}',
            other:     '{edge_other}'
        }};
        ['queue','eventgrid','invoke','function','other'].forEach(function(kind) {{
            defs.append('marker')
                .attr('id', 'arrow-'+kind)
                .attr('viewBox', '0 -5 10 10')
                .attr('refX', 22).attr('refY', 0)
                .attr('markerWidth', 6).attr('markerHeight', 6)
                .attr('orient', 'auto')
                .append('path')
                .attr('d', 'M0,-5L10,0L0,5')
                .attr('fill', edgeColors[kind]);
        }});

        var simulation = d3.forceSimulation(nodes)
            .force('link', d3.forceLink(links).id(function(d) {{ return d.id; }}).distance(140))
            .force('charge', d3.forceManyBody().strength(-400))
            .force('center', d3.forceCenter(width / 2, height / 2))
            .force('collision', d3.forceCollide().radius(30));

        var link = g.append('g').selectAll('line').data(links).join('line')
            .attr('fill', 'none')
            .attr('stroke-opacity', 0.5)
            .attr('stroke', function(d) {{ return edgeColors[d.kind] || '{edge_other}'; }})
            .attr('stroke-width', function(d) {{ return d.kind === 'invoke' ? 1 : 2; }})
            .attr('stroke-dasharray', function(d) {{ return d.kind === 'invoke' ? '6 3' : d.kind === 'function' ? '4 4' : null; }})
            .attr('marker-end', function(d) {{ return 'url(#arrow-'+d.kind+')'; }});

        var linkLabel = g.append('g').selectAll('text')
            .data(links.filter(function(d) {{ return d.kind !== 'invoke'; }}))
            .join('text')
            .attr('font-size', '9px').attr('fill', '{label_color}').style('pointer-events', 'none')
            .text(function(d) {{ return d.label.replace('queue:', ''); }});

        var nodeFills = {{
            http:       ['{node_fill_http}',       '{node_http}'],
            queue:      ['{node_fill_queue}',      '{node_queue}'],
            blob:       ['{node_fill_blob}',       '{node_blob}'],
            recurrence: ['{node_fill_recurrence}', '{node_recurrence}'],
            generic:    ['{node_fill_generic}',    '{node_generic}']
        }};

        var node = g.append('g').selectAll('g').data(nodes).join('g')
            .call(d3.drag()
                .on('start', function(e, d) {{ if (!e.active) simulation.alphaTarget(0.3).restart(); d.fx = d.x; d.fy = d.y; }})
                .on('drag', function(e, d) {{ d.fx = e.x; d.fy = e.y; }})
                .on('end', function(e, d) {{ if (!e.active) simulation.alphaTarget(0); d.fx = null; d.fy = null; }}));

        node.append('circle')
            .attr('r', function(d) {{ return d.group === 'generic' ? 6 : 10; }})
            .attr('fill', function(d) {{ return (nodeFills[d.group] || nodeFills.generic)[0]; }})
            .attr('stroke', function(d) {{ return (nodeFills[d.group] || nodeFills.generic)[1]; }})
            .attr('stroke-width', 2)
            .style('cursor', 'pointer');

        node.append('text')
            .attr('dx', 14).attr('dy', 4)
            .attr('fill', '{text}').attr('font-size', '11px').style('pointer-events', 'none')
            .text(function(d) {{ return d.id; }});

        var tooltip = d3.select('#graph-tooltip');
        node.on('mouseover', function(e, d) {{
            tooltip.style('display', 'block')
                .html('<strong>'+d.id+'</strong><br>'+(d.trigger || 'no trigger'));
        }}).on('mousemove', function(e) {{
            tooltip.style('left', (e.clientX+12)+'px').style('top', (e.clientY-20)+'px');
        }}).on('mouseout', function() {{ tooltip.style('display', 'none'); }});

        node.on('click', function(e, d) {{
            var connected = new Set();
            connected.add(d.id);
            links.forEach(function(l) {{
                if (l.source.id === d.id) connected.add(l.target.id);
                if (l.target.id === d.id) connected.add(l.source.id);
            }});
            node.select('circle').attr('fill-opacity', function(n) {{ return connected.has(n.id) ? 1 : 0.15; }});
            node.select('text').attr('fill-opacity', function(n) {{ return connected.has(n.id) ? 1 : 0.2; }});
            link.attr('stroke-opacity', function(l) {{ return connected.has(l.source.id) && connected.has(l.target.id) ? 0.9 : 0.1; }});
            linkLabel.attr('fill-opacity', function(l) {{ return connected.has(l.source.id) && connected.has(l.target.id) ? 1 : 0.1; }});
        }});

        svg.on('dblclick', function() {{
            node.select('circle').attr('fill-opacity', 1);
            node.select('text').attr('fill-opacity', 1);
            link.attr('stroke-opacity', 0.5);
            linkLabel.attr('fill-opacity', 1);
        }});

        simulation.on('tick', function() {{
            link.attr('x1', function(d) {{ return d.source.x; }})
                .attr('y1', function(d) {{ return d.source.y; }})
                .attr('x2', function(d) {{ return d.target.x; }})
                .attr('y2', function(d) {{ return d.target.y; }});
            linkLabel.attr('x', function(d) {{ return (d.source.x + d.target.x) / 2; }})
                     .attr('y', function(d) {{ return (d.source.y + d.target.y) / 2; }});
            node.attr('transform', function(d) {{ return 'translate('+d.x+','+d.y+')'; }});
        }});
    }}

    if (typeof d3 !== 'undefined') {{
        run();
    }} else {{
        var script = document.createElement('script');
        script.src = 'https://d3js.org/d3.v7.min.js';
        script.onload = run;
        document.head.appendChild(script);
    }}
}})();
"#,
        nodes = nodes,
        edges = edges,
        text = theme.text,
        label_color = theme.label_color,
        node_http = theme.node_http,
        node_queue = theme.node_queue,
        node_blob = theme.node_blob,
        node_recurrence = theme.node_recurrence,
        node_generic = theme.node_generic,
        node_fill_http = theme.node_fill_http,
        node_fill_queue = theme.node_fill_queue,
        node_fill_blob = theme.node_fill_blob,
        node_fill_recurrence = theme.node_fill_recurrence,
        node_fill_generic = theme.node_fill_generic,
        edge_queue = theme.edge_queue,
        edge_eventgrid = theme.edge_eventgrid,
        edge_invoke = theme.edge_invoke,
        edge_function = theme.edge_function,
        edge_other = theme.edge_other,
    )
}
