//! Generic, customer-agnostic outline of a Logic Apps workflow.
//!
//! Walks the workflow JSON and emits a flat list of "sections" that summarise
//! the phases the workflow goes through. The summary helps a reader navigate
//! large workflows; paired with line ranges, it also drives scroll-sync in the
//! Source tab (clicking a section scrolls to it; the section under the current
//! viewport highlights as the user scrolls).
//!
//! The rules are intentionally schema-driven — no naming conventions, no
//! customer-specific tokens. Any workflow.json produces a meaningful outline.
//!
//! ## Sectioning rules
//!
//! 1. The trigger becomes the first section.
//! 2. Each **container action** (`Scope`, `If`, `Switch`, `Foreach`, `Until`,
//!    `Try`, or any action with a child `actions` object) becomes one section,
//!    labelled with the author's own action name. Containers are walked
//!    recursively so nested phases appear as sub-sections.
//! 3. A run of consecutive non-container leaf actions at the same nesting
//!    level is collapsed into one synthetic "N steps" section listing the
//!    action names — so flat workflows still get a useful summary.
//!
//! ## Tree as a flat list with depth
//!
//! The outline is emitted in depth-first order as a flat `Vec<OutlineSection>`
//! with each entry carrying its `depth` and `parent` index. This makes the
//! rail trivial to render (just indent by depth) and active-section tracking
//! a simple linear scan, while still supporting collapse/expand because every
//! section knows its parent chain.
//!
//! ## Line ranges
//!
//! `serde_json` discards source spans, so we annotate ranges via a second
//! pass over the pretty-printed source: we look up each action name as a
//! key and use the next sibling at any nesting level as the end line.
//! Action names are unique within a workflow, so a first-occurrence lookup
//! is unambiguous.

use serde_json::Value;

/// Container action types in Logic Apps. These are stable across every
/// customer and every workflow — they are part of the Logic Apps schema, not
/// a naming convention. Anything with a child `actions` object behaves like
/// a container regardless of `type`, so we fall back to that check too.
const CONTAINER_TYPES: &[&str] = &[
    "Scope",
    "If",
    "Switch",
    "Foreach",
    "Until",
    "Try",
];

/// What kind of section this is — drives the icon shown next to it in the rail.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SectionKind {
    Trigger,
    /// A container action (Scope / If / Foreach / Until / Switch / Try).
    Container(String),
    /// A synthetic group of consecutive flat actions.
    Steps,
}

/// Recognisable Azure / 3rd-party services we can identify from action
/// signatures. Drives the chip icons shown on the outline rail so the user
/// can see at a glance "this scope hits SQL and Teams" without reading the
/// action names. The list is intentionally a closed enum — adding a service
/// is one match arm in `classify_action` and one in `chip_label`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ServiceTag {
    Sql,
    ServiceBus,
    Teams,
    Outlook,
    Blob,
    Cosmos,
    Http,
    Function,
    KeyVault,
    EventGrid,
    Sftp,
    Maps,
    AzureFile,
    /// Inline custom code (`JavaScriptCode`, `CSharpScriptCode`, …).
    /// Treated as "notable" so the action surfaces as its own rail row.
    Code,
}

impl ServiceTag {
    /// Short display label for the chip (3–4 chars to keep the rail compact).
    pub fn chip_label(&self) -> &'static str {
        match self {
            ServiceTag::Sql        => "SQL",
            ServiceTag::ServiceBus => "SB",
            ServiceTag::Teams      => "Teams",
            ServiceTag::Outlook    => "Mail",
            ServiceTag::Blob       => "Blob",
            ServiceTag::Cosmos     => "Cosmos",
            ServiceTag::Http       => "HTTP",
            ServiceTag::Function   => "ƒn",
            ServiceTag::KeyVault   => "KV",
            ServiceTag::EventGrid  => "EG",
            ServiceTag::Sftp       => "SFTP",
            ServiceTag::Maps       => "Maps",
            ServiceTag::AzureFile  => "File",
            ServiceTag::Code       => "Code",
        }
    }
    /// CSS class suffix — used as `chip chip-{slug}` for per-service color.
    pub fn css_slug(&self) -> &'static str {
        match self {
            ServiceTag::Sql        => "sql",
            ServiceTag::ServiceBus => "sb",
            ServiceTag::Teams      => "teams",
            ServiceTag::Outlook    => "mail",
            ServiceTag::Blob       => "blob",
            ServiceTag::Cosmos     => "cosmos",
            ServiceTag::Http       => "http",
            ServiceTag::Function   => "fn",
            ServiceTag::KeyVault   => "kv",
            ServiceTag::EventGrid  => "eg",
            ServiceTag::Sftp       => "sftp",
            ServiceTag::Maps       => "maps",
            ServiceTag::AzureFile  => "file",
            ServiceTag::Code       => "code",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineSection {
    /// Display label for the rail. For containers, the author's action name;
    /// for triggers and synthetic step groups, a generated label.
    pub label: String,
    pub kind:  SectionKind,
    /// Brief one-liner shown under the label (e.g. action type, step count).
    pub hint:  String,
    /// Nesting level. 0 = top-level (under `actions` or the trigger).
    pub depth: u8,
    /// Index in the outer `Vec` of this section's parent container, if any.
    /// `None` for top-level entries. Used to walk the ancestor chain when
    /// deciding whether a row is hidden under a collapsed parent.
    pub parent: Option<usize>,
    /// 1-based line numbers in the pretty-printed source. None when the
    /// action's key could not be located (defensive — should be rare).
    pub start_line: Option<u32>,
    pub end_line:   Option<u32>,
    /// Services this section touches (deduped, ordered by first appearance
    /// in the workflow). For a container, this is the union of every leaf
    /// action in its subtree. For a synthetic Steps group, it's the union
    /// of the listed leaves. The rail renders one chip per tag.
    pub tags: Vec<ServiceTag>,
}

/// Top-level entry point. Parses `source` as JSON, derives the outline, and
/// returns it. Returns an empty vec for malformed JSON — the caller can fall
/// back to "no outline available" without panicking.
pub fn build_outline(source: &str) -> Vec<OutlineSection> {
    let Ok(root) = serde_json::from_str::<Value>(source) else {
        return Vec::new();
    };

    // Logic Apps source layouts vary slightly between standalone designer
    // exports and the Standard runtime's workflow.json — accept both.
    let definition = root.get("definition").unwrap_or(&root);

    let mut sections: Vec<OutlineSection> = Vec::new();

    // ── 1. Trigger ────────────────────────────────────────────────────────
    if let Some(triggers) = definition.get("triggers").and_then(Value::as_object) {
        if let Some((name, body)) = triggers.iter().next() {
            let kind = body.get("type").and_then(Value::as_str).unwrap_or("Trigger");
            let kind_hint = body.get("kind").and_then(Value::as_str);
            let hint = match kind_hint {
                Some(k) => format!("{kind} · {k}"),
                None    => kind.to_string(),
            };
            let trigger_tags = classify_action(body).into_iter().collect::<Vec<_>>();
            sections.push(OutlineSection {
                label: name.clone(),
                kind:  SectionKind::Trigger,
                hint,
                depth: 0,
                parent: None,
                start_line: None,
                end_line:   None,
                tags: trigger_tags,
            });
        }
    }

    // ── 2. Walk top-level actions, recursing into containers ──────────────
    if let Some(actions) = definition.get("actions").and_then(Value::as_object) {
        emit_actions(actions, 0, None, &mut sections);
    }

    // ── 3. Annotate with line ranges from the pretty source ───────────────
    annotate_line_ranges(source, &mut sections);

    sections
}

/// Walk one `actions` object and emit sections into `out`. Recurses into
/// containers, increasing depth by one. Consecutive flat leaves at this
/// depth are collapsed into a single synthetic "Steps" section.
fn emit_actions(
    actions: &serde_json::Map<String, Value>,
    depth: u8,
    parent: Option<usize>,
    out: &mut Vec<OutlineSection>,
) {
    // (action_name, tags_from_classify) — we keep both so flush_flat can
    // build the synthetic Steps section's combined tag chip row.
    let mut flat_run: Vec<(String, Vec<ServiceTag>)> = Vec::new();

    // Hold a snapshot of the parent's actions so the flat-run flusher can
    // re-classify each named action. Captured by reference into the closure.
    let actions_ref = actions;
    let flush_flat = |run: &mut Vec<(String, Vec<ServiceTag>)>, out: &mut Vec<OutlineSection>| {
        if run.is_empty() { return; }
        let label = if run.len() == 1 {
            run[0].0.clone()
        } else {
            format!("{} steps", run.len())
        };
        let hint = run.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(" → ");
        // Union of the leaves' tags, in first-occurrence order.
        let mut tags: Vec<ServiceTag> = Vec::new();
        for (_, ts) in run.iter() {
            for t in ts {
                if !tags.contains(t) { tags.push(t.clone()); }
            }
        }
        out.push(OutlineSection {
            label,
            kind: SectionKind::Steps,
            hint,
            depth,
            parent,
            start_line: None,
            end_line:   None,
            tags,
        });
        run.clear();
    };
    let _ = actions_ref; // silence unused-binding if classifier needs no map access

    for (name, body) in actions.iter() {
        let ty = body.get("type").and_then(Value::as_str).unwrap_or("");
        let inner = body.get("actions").and_then(Value::as_object);
        // `Switch` actions hold their branches in `cases` and `default`,
        // each a sub-action map. Treat those as belonging to the Switch's
        // own indentation so the user can see which branches exist.
        let is_container = CONTAINER_TYPES.contains(&ty) || inner.is_some();

        if is_container {
            flush_flat(&mut flat_run, out);

            // Count inner actions across all the schemas a container can use.
            let inner_count = inner.map(|m| m.len()).unwrap_or(0)
                + count_switch_branches(body);
            let hint = if inner_count > 0 {
                format!("{ty} · {inner_count} step{}", if inner_count == 1 { "" } else { "s" })
            } else {
                ty.to_string()
            };
            out.push(OutlineSection {
                label: name.clone(),
                kind:  SectionKind::Container(ty.to_string()),
                hint,
                depth,
                parent,
                start_line: None,
                end_line:   None,
                tags: collect_tags(body),
            });
            let my_idx = out.len() - 1;

            // Recurse into the container's children. depth+1 unless we'd
            // overflow u8 (defensive — Logic Apps doesn't really go that deep).
            let child_depth = depth.saturating_add(1);
            if let Some(child) = inner {
                emit_actions(child, child_depth, Some(my_idx), out);
            }
            // Switch branches: `cases.<name>.actions` and `default.actions`
            if ty == "Switch" {
                if let Some(cases) = body.get("cases").and_then(Value::as_object) {
                    for (case_name, case_body) in cases {
                        if let Some(case_actions) = case_body.get("actions").and_then(Value::as_object) {
                            // Each case becomes a labelled mini-section at the
                            // child depth. Its hint says the branch is a "case".
                            out.push(OutlineSection {
                                label: case_name.clone(),
                                kind:  SectionKind::Container("Case".to_string()),
                                hint:  format!("case · {} step{}", case_actions.len(), if case_actions.len() == 1 { "" } else { "s" }),
                                depth: child_depth,
                                parent: Some(my_idx),
                                start_line: None,
                                end_line:   None,
                                tags: collect_tags(case_body),
                            });
                            let case_idx = out.len() - 1;
                            emit_actions(case_actions, child_depth.saturating_add(1), Some(case_idx), out);
                        }
                    }
                }
                if let Some(default_actions) = body.get("default").and_then(|d| d.get("actions")).and_then(Value::as_object) {
                    let default_node = body.get("default").cloned().unwrap_or(serde_json::Value::Null);
                    out.push(OutlineSection {
                        label: "default".to_string(),
                        kind:  SectionKind::Container("Case".to_string()),
                        hint:  format!("default · {} step{}", default_actions.len(), if default_actions.len() == 1 { "" } else { "s" }),
                        depth: child_depth,
                        parent: Some(my_idx),
                        start_line: None,
                        end_line:   None,
                        tags: collect_tags(&default_node),
                    });
                    let default_idx = out.len() - 1;
                    emit_actions(default_actions, child_depth.saturating_add(1), Some(default_idx), out);
                }
            }
            // `If` actions have separate `actions` (then-branch) and `else`
            // (else-branch). The `actions` branch was already recursed above
            // via `inner`. Add the `else` branch if present.
            if ty == "If" {
                if let Some(else_actions) = body.get("else").and_then(|e| e.get("actions")).and_then(Value::as_object) {
                    let else_node = body.get("else").cloned().unwrap_or(serde_json::Value::Null);
                    out.push(OutlineSection {
                        label: "else".to_string(),
                        kind:  SectionKind::Container("Case".to_string()),
                        hint:  format!("else · {} step{}", else_actions.len(), if else_actions.len() == 1 { "" } else { "s" }),
                        depth: child_depth,
                        parent: Some(my_idx),
                        start_line: None,
                        end_line:   None,
                        tags: collect_tags(&else_node),
                    });
                    let else_idx = out.len() - 1;
                    emit_actions(else_actions, child_depth.saturating_add(1), Some(else_idx), out);
                }
            }
        } else {
            // Classify the leaf. Recognised actions — anything that talks to
            // a backing service or runs custom code (SQL, Service Bus, Teams,
            // HTTP, Function, JavaScriptCode, etc.) — get their *own* rail
            // row so the action name is visible without scanning a long
            // "A → B → C" hint. Plumbing leaves (SetVariable, ParseJson,
            // Compose, InitializeVariable, …) keep collapsing into a
            // multi-step group with the previous boring neighbours.
            let leaf_tags = classify_action(body).into_iter().collect::<Vec<_>>();
            if !leaf_tags.is_empty() {
                flush_flat(&mut flat_run, out);
                let ty = body.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                out.push(OutlineSection {
                    label: name.clone(),
                    kind:  SectionKind::Steps,
                    hint:  ty,
                    depth,
                    parent,
                    start_line: None,
                    end_line:   None,
                    tags: leaf_tags,
                });
            } else {
                flat_run.push((name.clone(), Vec::new()));
            }
        }
    }
    flush_flat(&mut flat_run, out);
}

/// True if this section has at least one direct child in the flat list.
/// `O(n)` — fine because the rail is rendered once per outline change and
/// the outline is rarely longer than a few dozen entries.
pub fn has_children(sections: &[OutlineSection], idx: usize) -> bool {
    sections.iter().any(|s| s.parent == Some(idx))
}

/// Climb the parent chain and return true if any ancestor index is contained
/// in `collapsed`. Used to hide rows under a collapsed parent at render time.
pub fn is_under_collapsed(
    sections: &[OutlineSection],
    idx: usize,
    collapsed: &std::collections::HashSet<usize>,
) -> bool {
    let mut cur = sections[idx].parent;
    while let Some(p) = cur {
        if collapsed.contains(&p) { return true; }
        cur = sections[p].parent;
    }
    false
}

/// Inspect a single action body and decide which `ServiceTag` (if any) it
/// represents. Recognition is driven entirely by Logic Apps schema fields —
/// connector/serviceProvider IDs and a small set of stable action types —
/// so it works across every customer without any naming convention.
fn classify_action(body: &Value) -> Option<ServiceTag> {
    let ty = body.get("type").and_then(Value::as_str).unwrap_or("");

    // ── Built-in action types ─────────────────────────────────────────
    match ty {
        "Http"        => return Some(ServiceTag::Http),
        "Function"    => return Some(ServiceTag::Function),
        // The two ways Logic Apps Standard refers to nested function calls.
        "InvokeFunction" | "Workflow" => return Some(ServiceTag::Function),
        // Inline code actions — surfacing them on the rail matters because
        // they often hold business-critical validation / transform logic.
        "JavaScriptCode" | "CSharpScriptCode" | "ExecuteJavaScriptCode"
            => return Some(ServiceTag::Code),
        _ => {}
    }

    // ── Service-provider actions (Logic Apps Standard "built-in" connectors) ─
    // The id lives at inputs.serviceProviderConfiguration.serviceProviderId
    // and looks like `/serviceProviders/sql`, `/serviceProviders/serviceBus`,
    // etc. A simple substring match on the slug is enough.
    let sp_id = body
        .pointer("/inputs/serviceProviderConfiguration/serviceProviderId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if !sp_id.is_empty() {
        let tag = match_service_slug(&sp_id);
        if tag.is_some() { return tag; }
    }

    // ── ApiConnection actions (managed connectors) ──────────────────────
    // The connector lives at inputs.host.connection.referenceName which
    // points to a `connections.json` entry. Without that file in scope here,
    // we still get useful info from inputs.host.apiId or inputs.path:
    //   - apiId  = "/subscriptions/.../providers/Microsoft.Web/locations/.../managedApis/teams"
    //   - path   = "/v3/joinedTeams/{teamId}/channels/..."
    if ty == "ApiConnection" || ty == "ApiConnectionNotification" || ty == "ApiConnectionWebhook" {
        let api_id = body
            .pointer("/inputs/host/apiId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let path = body
            .pointer("/inputs/path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let conn_ref = body
            .pointer("/inputs/host/connection/referenceName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        for hay in [&api_id, &path, &conn_ref] {
            if let Some(t) = match_service_slug(hay) { return Some(t); }
        }
    }

    None
}

/// Match a lowercased "haystack" (a service-provider id, an api-id URL, a
/// connection reference name, or a request path) against the known Azure /
/// Microsoft 365 service slugs and return the corresponding `ServiceTag`.
///
/// All slugs here are stable identifiers used by Logic Apps itself, not
/// customer-chosen names. Order matters where one slug is a substring of
/// another (e.g. `azureblob` contains `blob`); we list the more-specific
/// matches first.
fn match_service_slug(hay: &str) -> Option<ServiceTag> {
    if hay.contains("servicebus")           { return Some(ServiceTag::ServiceBus); }
    if hay.contains("/sql")
        || hay.contains("sqlserver")
        || hay.contains("azuresql")          { return Some(ServiceTag::Sql); }
    if hay.contains("teams")                { return Some(ServiceTag::Teams); }
    if hay.contains("outlook")
        || hay.contains("office365")
        || hay.contains("sendgrid")
        || hay.contains("smtp")              { return Some(ServiceTag::Outlook); }
    if hay.contains("azureblob")
        || hay.contains("blob")              { return Some(ServiceTag::Blob); }
    if hay.contains("cosmosdb")
        || hay.contains("/cosmos")           { return Some(ServiceTag::Cosmos); }
    if hay.contains("keyvault")             { return Some(ServiceTag::KeyVault); }
    if hay.contains("eventgrid")            { return Some(ServiceTag::EventGrid); }
    if hay.contains("/sftp")                { return Some(ServiceTag::Sftp); }
    if hay.contains("azuremaps") || hay.contains("/maps") {
        return Some(ServiceTag::Maps);
    }
    if hay.contains("azurefile")            { return Some(ServiceTag::AzureFile); }
    None
}

/// Walk an action subtree and return every distinct `ServiceTag` found,
/// in first-occurrence order. For containers (`Scope`, `If`, etc.) this
/// recurses into the nested action maps as well as `cases.*`, `default`,
/// and `else` branches. Used to summarise "what does this section touch".
pub fn collect_tags(body: &Value) -> Vec<ServiceTag> {
    let mut out: Vec<ServiceTag> = Vec::new();
    visit_for_tags(body, &mut out);
    out
}

fn visit_for_tags(body: &Value, out: &mut Vec<ServiceTag>) {
    if let Some(tag) = classify_action(body) {
        if !out.contains(&tag) { out.push(tag); }
    }
    if let Some(actions) = body.get("actions").and_then(Value::as_object) {
        for (_, child) in actions { visit_for_tags(child, out); }
    }
    if let Some(else_node) = body.get("else") {
        if let Some(actions) = else_node.get("actions").and_then(Value::as_object) {
            for (_, child) in actions { visit_for_tags(child, out); }
        }
    }
    if let Some(cases) = body.get("cases").and_then(Value::as_object) {
        for (_, case_body) in cases {
            if let Some(actions) = case_body.get("actions").and_then(Value::as_object) {
                for (_, child) in actions { visit_for_tags(child, out); }
            }
        }
    }
    if let Some(default) = body.get("default") {
        if let Some(actions) = default.get("actions").and_then(Value::as_object) {
            for (_, child) in actions { visit_for_tags(child, out); }
        }
    }
}

fn count_switch_branches(body: &Value) -> usize {
    let cases = body.get("cases").and_then(Value::as_object).map(|m| m.len()).unwrap_or(0);
    let default = if body.get("default").and_then(|d| d.get("actions")).is_some() { 1 } else { 0 };
    cases + default
}

/// Find the first occurrence of `"<name>":` in the pretty-printed source and
/// use that as the section's start line. End line is the next section's
/// start (or EOF). Action names are unique within a workflow, so the first
/// occurrence is always the correct one.
fn annotate_line_ranges(source: &str, sections: &mut [OutlineSection]) {
    let mut lookup: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for (i, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('"') { continue; }
        let Some(rest) = trimmed.strip_prefix('"') else { continue };
        let Some(end) = rest.find("\":") else { continue };
        let key = &rest[..end];
        lookup.entry(key.to_string()).or_insert((i + 1) as u32);
    }

    for s in sections.iter_mut() {
        s.start_line = lookup.get(&s.label).copied();
    }
    for s in sections.iter_mut() {
        if matches!(s.kind, SectionKind::Steps) && s.start_line.is_none() {
            let first = s.hint.split(" → ").next().unwrap_or("");
            s.start_line = lookup.get(first).copied();
        }
    }

    // End lines: scan forward for the next start at any depth, defaulting to
    // EOF. This makes "is the cursor inside this section?" a simple range check.
    let total_lines = source.lines().count() as u32;
    let starts: Vec<Option<u32>> = sections.iter().map(|s| s.start_line).collect();
    for (i, s) in sections.iter_mut().enumerate() {
        let next_start = starts.iter().skip(i + 1).flatten().next().copied();
        s.end_line = Some(next_start.map(|n| n.saturating_sub(1)).unwrap_or(total_lines));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "definition": {
    "triggers": {
      "When_a_message_is_received": {
        "type": "ServiceProvider",
        "kind": "ServiceBus"
      }
    },
    "actions": {
      "Parse_Body": {
        "type": "ParseJson"
      },
      "Set_Status": {
        "type": "SetVariable"
      },
      "Scope_FetchData": {
        "type": "Scope",
        "actions": {
          "HTTP_Get": { "type": "Http" },
          "HTTP_Post": { "type": "Http" }
        }
      },
      "If_HasErrors": {
        "type": "If",
        "actions": {
          "Notify_OnError": { "type": "Http" }
        },
        "else": {
          "actions": {
            "Notify_OnOk": { "type": "Http" }
          }
        }
      },
      "Final_Step": {
        "type": "ComposeJson"
      }
    }
  }
}"#;

    #[test]
    fn trigger_is_first_section() {
        let o = build_outline(SAMPLE);
        assert_eq!(o[0].kind, SectionKind::Trigger);
        assert_eq!(o[0].label, "When_a_message_is_received");
        assert_eq!(o[0].depth, 0);
    }

    #[test]
    fn consecutive_leaves_collapse_into_one_steps_section() {
        let o = build_outline(SAMPLE);
        assert_eq!(o[1].kind, SectionKind::Steps);
        assert_eq!(o[1].label, "2 steps");
        assert_eq!(o[1].depth, 0);
        assert!(o[1].hint.contains("Parse_Body"));
        assert!(o[1].hint.contains("Set_Status"));
    }

    #[test]
    fn containers_become_their_own_sections_at_depth_zero() {
        let o = build_outline(SAMPLE);
        let scope = o.iter().find(|s| s.label == "Scope_FetchData").unwrap();
        assert!(matches!(scope.kind, SectionKind::Container(ref t) if t == "Scope"));
        assert_eq!(scope.depth, 0);
        assert!(scope.parent.is_none());
    }

    #[test]
    fn container_children_are_emitted_at_depth_one() {
        let o = build_outline(SAMPLE);
        let scope_idx = o.iter().position(|s| s.label == "Scope_FetchData").unwrap();
        // HTTP_Get + HTTP_Post are both classified as Http, so under the
        // "notable leaves stand alone" rule each becomes its own depth-1
        // section under the Scope.
        let children: Vec<&OutlineSection> = o.iter()
            .filter(|s| s.parent == Some(scope_idx))
            .collect();
        assert!(children.iter().any(|s| s.label == "HTTP_Get" && s.depth == 1),
            "HTTP_Get should appear as its own depth-1 section under the Scope");
        assert!(children.iter().any(|s| s.label == "HTTP_Post" && s.depth == 1),
            "HTTP_Post should appear as its own depth-1 section under the Scope");
    }

    #[test]
    fn if_else_branches_both_appear_as_sub_sections() {
        let o = build_outline(SAMPLE);
        let if_idx = o.iter().position(|s| s.label == "If_HasErrors").unwrap();
        let children: Vec<&OutlineSection> = o.iter()
            .filter(|s| s.parent == Some(if_idx))
            .collect();
        assert!(children.iter().any(|s| s.label == "else"),
            "If section should expose an else branch as a sub-section");
        // Notify_OnError is an Http action → notable leaf → its own section
        // labelled by the action name.
        assert!(children.iter().any(|s| s.label == "Notify_OnError"),
            "the then-branch's Http leaf should appear as its own section");
    }

    #[test]
    fn has_children_reflects_emit_order() {
        let o = build_outline(SAMPLE);
        let scope_idx = o.iter().position(|s| s.label == "Scope_FetchData").unwrap();
        let leaf_idx  = o.iter().position(|s| s.label == "Final_Step").unwrap();
        assert!(has_children(&o, scope_idx));
        assert!(!has_children(&o, leaf_idx));
    }

    #[test]
    fn collapsed_parent_hides_descendants() {
        let o = build_outline(SAMPLE);
        let scope_idx = o.iter().position(|s| s.label == "Scope_FetchData").unwrap();
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert(scope_idx);
        // Any descendant of scope_idx should be reported as hidden.
        for (i, s) in o.iter().enumerate() {
            if s.parent == Some(scope_idx) {
                assert!(is_under_collapsed(&o, i, &collapsed));
            }
        }
        // The Scope itself is not hidden — only its descendants are.
        assert!(!is_under_collapsed(&o, scope_idx, &collapsed));
    }

    #[test]
    fn line_ranges_increase_in_emit_order() {
        let o = build_outline(SAMPLE);
        let mut last = 0u32;
        for s in &o {
            if let Some(start) = s.start_line {
                assert!(start >= last, "section {} starts at line {} before previous {}", s.label, start, last);
                last = start;
            }
        }
    }

    #[test]
    fn service_provider_actions_get_tagged() {
        let sample = r#"{
          "definition": {
            "triggers": {
              "On_New_Msg": {
                "type": "ServiceProvider",
                "inputs": {
                  "serviceProviderConfiguration": {
                    "serviceProviderId": "/serviceProviders/serviceBus"
                  }
                }
              }
            },
            "actions": {
              "RunQuery": {
                "type": "ServiceProvider",
                "inputs": {
                  "serviceProviderConfiguration": {
                    "serviceProviderId": "/serviceProviders/sql"
                  }
                }
              }
            }
          }
        }"#;
        let o = build_outline(sample);
        assert_eq!(o[0].tags, vec![ServiceTag::ServiceBus]);
        assert_eq!(o[1].tags, vec![ServiceTag::Sql]);
    }

    #[test]
    fn api_connection_path_recognises_teams() {
        let sample = r#"{
          "definition": {
            "actions": {
              "PostToChannel": {
                "type": "ApiConnection",
                "inputs": {
                  "host": {
                    "apiId": "/subscriptions/x/providers/Microsoft.Web/locations/y/managedApis/teams"
                  },
                  "path": "/v3/joinedTeams/{id}/channels/abc/messages"
                }
              }
            }
          }
        }"#;
        let o = build_outline(sample);
        assert_eq!(o[0].tags, vec![ServiceTag::Teams]);
    }

    #[test]
    fn container_aggregates_descendant_tags_in_order() {
        let sample = r#"{
          "definition": {
            "actions": {
              "Process_Order": {
                "type": "Scope",
                "actions": {
                  "Lookup_Customer": {
                    "type": "ServiceProvider",
                    "inputs": { "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/sql" }}
                  },
                  "Notify_Team": {
                    "type": "ApiConnection",
                    "inputs": { "host": { "apiId": "/managedApis/teams" } }
                  },
                  "Call_API": { "type": "Http" }
                }
              }
            }
          }
        }"#;
        let o = build_outline(sample);
        let scope = o.iter().find(|s| s.label == "Process_Order").unwrap();
        assert_eq!(
            scope.tags,
            vec![ServiceTag::Sql, ServiceTag::Teams, ServiceTag::Http]
        );
    }

    #[test]
    fn notable_leaves_get_their_own_section_not_collapsed_into_steps() {
        // A flat run of three leaves where the middle one is a SetVariable
        // (plumbing, no tag) and the outer two are SQL + Service Bus
        // (notable). Notable leaves should each become their own section;
        // the SetVariable still collapses but stays adjacent.
        let sample = r#"{
          "definition": {
            "actions": {
              "RunQuery": {
                "type": "ServiceProvider",
                "inputs": { "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/sql" }}
              },
              "Set_flag": { "type": "SetVariable" },
              "SendMsg": {
                "type": "ServiceProvider",
                "inputs": { "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus" }}
              }
            }
          }
        }"#;
        let o = build_outline(sample);
        // 3 sections: RunQuery, Set_flag (singleton Steps), SendMsg.
        assert_eq!(o.len(), 3);
        assert_eq!(o[0].label, "RunQuery");
        assert_eq!(o[0].tags, vec![ServiceTag::Sql]);
        assert_eq!(o[1].label, "Set_flag");
        assert!(o[1].tags.is_empty(), "plumbing leaf should not carry a tag");
        assert_eq!(o[2].label, "SendMsg");
        assert_eq!(o[2].tags, vec![ServiceTag::ServiceBus]);
    }

    #[test]
    fn javascript_code_is_classified_as_code() {
        let sample = r#"{
          "definition": {
            "actions": {
              "ValidateRows": {
                "type": "JavaScriptCode",
                "inputs": { "code": "return {ok:true}" }
              }
            }
          }
        }"#;
        let o = build_outline(sample);
        assert_eq!(o[0].label, "ValidateRows");
        assert_eq!(o[0].tags, vec![ServiceTag::Code]);
    }

    #[test]
    fn malformed_json_returns_empty_outline() {
        assert!(build_outline("{not json").is_empty());
    }
}
