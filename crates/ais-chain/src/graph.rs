use crate::parser::Workflow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug)]
pub struct Edge {
    pub to: String,
    pub link_type: String,
}

#[derive(Debug)]
pub struct ChainStep {
    pub workflow: String,
    pub link_type: String, // empty for root
}

#[derive(Debug)]
pub struct Chain {
    pub steps: Vec<ChainStep>,
}

pub struct Graph {
    edges: HashMap<String, Vec<Edge>>,
    all_workflows: HashSet<String>,
    queue_producers: BTreeMap<String, Vec<String>>,
    queue_consumers: BTreeMap<String, Vec<String>>,
}

impl Graph {
    pub fn find_chains(&self) -> Vec<Chain> {
        // Find nodes with incoming edges
        let mut has_incoming: HashSet<String> = HashSet::new();
        for edges in self.edges.values() {
            for e in edges {
                has_incoming.insert(e.to.clone());
            }
        }

        // Roots = workflows with no incoming edges
        let mut roots: Vec<&String> = self.all_workflows.iter()
            .filter(|w| !has_incoming.contains(*w))
            .collect();
        roots.sort();

        let mut chains = Vec::new();
        for root in roots {
            let chain = self.trace(root);
            chains.push(chain);
        }

        // Sort: multi-step chains first, then by name
        chains.sort_by(|a, b| {
            b.steps.len().cmp(&a.steps.len())
                .then_with(|| a.steps[0].workflow.cmp(&b.steps[0].workflow))
        });

        chains
    }

    fn trace(&self, start: &str) -> Chain {
        let mut steps = Vec::new();
        let mut visited = HashSet::new();
        self.trace_recursive(start, "", &mut steps, &mut visited);
        Chain { steps }
    }

    fn trace_recursive(&self, node: &str, link_type: &str, steps: &mut Vec<ChainStep>, visited: &mut HashSet<String>) {
        if visited.contains(node) {
            return;
        }
        visited.insert(node.to_string());

        steps.push(ChainStep {
            workflow: node.to_string(),
            link_type: link_type.to_string(),
        });

        if let Some(edges) = self.edges.get(node) {
            // Sort edges for deterministic output, prioritize queue edges over invoke
            let mut sorted: Vec<&Edge> = edges.iter().collect();
            sorted.sort_by(|a, b| {
                let priority = |e: &Edge| -> u8 {
                    if e.link_type.starts_with("queue:") { 0 }
                    else if e.link_type == "EventGrid" { 1 }
                    else if e.link_type.starts_with("function:") { 2 }
                    else { 3 } // invoke
                };
                priority(a).cmp(&priority(b)).then_with(|| a.to.cmp(&b.to))
            });

            for edge in sorted {
                self.trace_recursive(&edge.to, &edge.link_type, steps, visited);
            }
        }
    }

    /// Return all edges as (from, to, link_type) tuples
    pub fn all_edges(&self) -> Vec<(&str, &str, &str)> {
        let mut result = Vec::new();
        for (from, edges) in &self.edges {
            for e in edges {
                result.push((from.as_str(), e.to.as_str(), e.link_type.as_str()));
            }
        }
        result
    }

    pub fn queue_map(&self) -> Vec<(String, Vec<String>, Vec<String>)> {
        let all_queues: BTreeSet<&String> = self.queue_producers.keys()
            .chain(self.queue_consumers.keys())
            .collect();

        all_queues.into_iter().map(|q| {
            let producers = self.queue_producers.get(q).cloned().unwrap_or_default();
            let consumers = self.queue_consumers.get(q).cloned().unwrap_or_default();
            (q.clone(), producers, consumers)
        }).collect()
    }
}

/// Parse manual link string: "Source->Target:label"
fn parse_manual_link(s: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = s.splitn(2, "->").collect();
    if parts.len() != 2 {
        return None;
    }
    let from = parts[0].trim().to_string();
    let rest: Vec<&str> = parts[1].splitn(2, ':').collect();
    let to = rest[0].trim().to_string();
    let label = if rest.len() > 1 { rest[1].trim().to_string() } else { "manual".to_string() };
    Some((from, to, label))
}

pub fn build(workflows: &[Workflow], manual_links: &[String]) -> Graph {
    let all_workflows: HashSet<String> = workflows.iter().map(|w| w.name.clone()).collect();

    let mut queue_producers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut queue_consumers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut eg_producers: Vec<String> = Vec::new();
    let mut eg_consumers: Vec<String> = Vec::new();

    for wf in workflows {
        for link in &wf.sends {
            match link.kind.as_str() {
                "queue" => queue_producers.entry(link.target.clone()).or_default().push(wf.name.clone()),
                "eventgrid" => eg_producers.push(wf.name.clone()),
                _ => {}
            }
        }
        for link in &wf.triggers {
            match link.kind.as_str() {
                "queue" => queue_consumers.entry(link.target.clone()).or_default().push(wf.name.clone()),
                "eventgrid" => eg_consumers.push(wf.name.clone()),
                _ => {}
            }
        }
    }

    // Build edges
    let mut edges: HashMap<String, Vec<Edge>> = HashMap::new();

    // Queue edges
    let all_queues: BTreeSet<&String> = queue_producers.keys()
        .chain(queue_consumers.keys())
        .collect();

    for queue in all_queues {
        if let Some(producers) = queue_producers.get(queue) {
            if let Some(consumers) = queue_consumers.get(queue) {
                for prod in producers {
                    for cons in consumers {
                        if prod != cons {
                            edges.entry(prod.clone()).or_default().push(Edge {
                                to: cons.clone(),
                                link_type: format!("queue:{queue}"),
                            });
                        }
                    }
                }
            }
        }
    }

    // EventGrid edges
    for prod in &eg_producers {
        for cons in &eg_consumers {
            if prod != cons {
                edges.entry(prod.clone()).or_default().push(Edge {
                    to: cons.clone(),
                    link_type: "EventGrid".into(),
                });
            }
        }
    }

    // Direct workflow invocations
    for wf in workflows {
        for call in &wf.calls {
            if all_workflows.contains(&call.target) {
                edges.entry(wf.name.clone()).or_default().push(Edge {
                    to: call.target.clone(),
                    link_type: call.kind.clone(),
                });
            }
        }
    }

    // Manual links (for EventGrid subscriptions, dynamic routing, etc.)
    for link_str in manual_links {
        if let Some((from, to, label)) = parse_manual_link(link_str) {
            edges.entry(from).or_default().push(Edge {
                to,
                link_type: label,
            });
        }
    }

    // Deduplicate edges
    for edges_list in edges.values_mut() {
        edges_list.sort_by(|a, b| a.to.cmp(&b.to).then_with(|| a.link_type.cmp(&b.link_type)));
        edges_list.dedup_by(|a, b| a.to == b.to && a.link_type == b.link_type);
    }

    Graph {
        edges,
        all_workflows,
        queue_producers,
        queue_consumers,
    }
}
