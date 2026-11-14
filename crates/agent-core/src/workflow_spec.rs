//! Workflow specification (agent groups) and prompt templating.
//!
//! A workflow is a small DAG of agents with optional JSON Schemas on its input
//! and output. The tool layer executes it; the UI renders it as a group frame.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::truncate_chars;
use crate::network::AgentVisibility;

// ---------------------------------------------------------------------------
// Workflow specification (agent groups)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Option<Value>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub nodes: Vec<WorkflowNodeSpec>,
    #[serde(default)]
    pub edges: Vec<WorkflowEdge>,
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,
}

fn default_max_parallel() -> usize {
    2
}

fn default_profile() -> String {
    "general-purpose".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowNodeSpec {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub can_spawn: bool,
    #[serde(default)]
    pub read_only: bool,
    /// Group members are public by default so siblings can reuse them; pass
    /// `visibility: "private"` to scope a node to the group run's launcher.
    #[serde(default = "default_public_visibility")]
    pub visibility: AgentVisibility,
}

fn default_public_visibility() -> AgentVisibility {
    AgentVisibility::Public
}

impl WorkflowNodeSpec {
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct WorkflowEdge {
    pub from: String,
    pub to: String,
}

impl WorkflowSpec {
    /// Validate ids, edges and acyclicity. `@output` may only be an edge
    /// target; it marks an explicit group output.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("workflow name is required".to_string());
        }
        if self.nodes.is_empty() {
            return Err("a workflow needs at least one node".to_string());
        }
        if self.nodes.len() > 32 {
            return Err(format!(
                "a workflow can contain at most 32 nodes (got {})",
                self.nodes.len()
            ));
        }
        let mut ids = HashSet::new();
        for node in &self.nodes {
            let id = node.id.trim();
            if id.is_empty() {
                return Err("every workflow node needs a non-empty id".to_string());
            }
            if id == "@output" {
                return Err("`@output` is reserved and cannot be a node id".to_string());
            }
            if !ids.insert(id.to_string()) {
                return Err(format!("duplicate workflow node id `{id}`"));
            }
        }
        let mut edges = HashSet::new();
        for edge in &self.edges {
            if edge.from == "@output" {
                return Err("`@output` cannot be an edge source".to_string());
            }
            if !ids.contains(&edge.from) {
                return Err(format!("edge references unknown node `{}`", edge.from));
            }
            if edge.to != "@output" && !ids.contains(&edge.to) {
                return Err(format!("edge references unknown node `{}`", edge.to));
            }
            if edge.from == edge.to {
                return Err(format!("node `{}` cannot depend on itself", edge.from));
            }
            if !edges.insert((edge.from.clone(), edge.to.clone())) {
                return Err(format!("duplicate edge {} -> {}", edge.from, edge.to));
            }
        }
        self.topological_layers()?;
        Ok(())
    }

    /// Kahn layering. Each layer can run in parallel. Returns `Err` on a cycle.
    pub fn topological_layers(&self) -> Result<Vec<Vec<String>>, String> {
        let mut in_degree: HashMap<&str, usize> = self
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), 0))
            .collect();
        let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &self.edges {
            if edge.to == "@output" {
                continue;
            }
            *in_degree
                .get_mut(edge.to.as_str())
                .ok_or_else(|| format!("unknown node `{}`", edge.to))? += 1;
            outgoing
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
        let mut ready: Vec<&str> = in_degree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| *id)
            .collect();
        ready.sort_unstable();
        let mut layers = Vec::new();
        let mut visited = 0;
        while !ready.is_empty() {
            let mut next = Vec::new();
            let layer: Vec<String> = ready.iter().map(|id| id.to_string()).collect();
            for id in &layer {
                visited += 1;
                if let Some(children) = outgoing.get(id.as_str()) {
                    for child in children {
                        if let Some(degree) = in_degree.get_mut(child) {
                            *degree = degree.saturating_sub(1);
                            if *degree == 0 {
                                next.push(*child);
                            }
                        }
                    }
                }
            }
            next.sort_unstable();
            layers.push(layer);
            ready = next;
        }
        if visited != self.nodes.len() {
            return Err("workflow edges contain a cycle".to_string());
        }
        Ok(layers)
    }

    pub fn node(&self, id: &str) -> Option<&WorkflowNodeSpec> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn upstream(&self, id: &str) -> Vec<&str> {
        self.edges
            .iter()
            .filter(|edge| edge.to == id)
            .map(|edge| edge.from.as_str())
            .collect()
    }

    /// Nodes that declare an explicit `@output` edge, or, when there is none,
    /// every node with no outgoing data edge.
    pub fn output_node_ids(&self) -> Vec<String> {
        let explicit: Vec<String> = self
            .edges
            .iter()
            .filter(|edge| edge.to == "@output")
            .map(|edge| edge.from.clone())
            .collect();
        if !explicit.is_empty() {
            return explicit;
        }
        if self.node("output").is_some() {
            return vec!["output".to_string()];
        }
        let sources: HashSet<&str> = self.edges.iter().map(|edge| edge.from.as_str()).collect();
        self.nodes
            .iter()
            .filter(|node| !sources.contains(node.id.as_str()))
            .map(|node| node.id.clone())
            .collect()
    }
}

/// Render a node prompt. Supports `{{input}}`, `{{input.field}}`,
/// `{{node_id}}` and `{{node_id.field}}`. When no placeholder is present, an
/// explicit input/upstream section is appended so the model always sees the
/// hand-off data.
pub fn render_node_prompt(
    node: &WorkflowNodeSpec,
    spec: &WorkflowSpec,
    input: &Value,
    outputs: &BTreeMap<String, Value>,
) -> String {
    let (mut prompt, replaced) = render_template(&node.prompt, input, outputs);
    if prompt.trim().is_empty() {
        prompt = format!(
            "Complete workflow node `{}` ({}) using the provided input.",
            node.id,
            node.display_name()
        );
        if !node.description.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(node.description.trim());
        }
    }
    if !replaced {
        prompt.push_str("\n\n# Input\n");
        prompt.push_str(&value_text(input, 12_000));
        let upstream = spec.upstream(&node.id);
        if !upstream.is_empty() {
            prompt.push_str("\n\n# Upstream results");
            for id in upstream {
                if let Some(value) = outputs.get(id) {
                    prompt.push_str(&format!("\n\n## {id}\n"));
                    prompt.push_str(&value_text(value, 12_000));
                }
            }
        }
    }
    prompt
}

pub fn render_template(
    template: &str,
    input: &Value,
    outputs: &BTreeMap<String, Value>,
) -> (String, bool) {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut replaced = false;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        let key = after[..end].trim();
        match resolve_template_key(key, input, outputs) {
            Some(value) => {
                out.push_str(&value_text(&value, 24_000));
                replaced = true;
            }
            None => {
                out.push_str("{{");
                out.push_str(&after[..end]);
                out.push_str("}}");
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    (out, replaced)
}

fn resolve_template_key(
    key: &str,
    input: &Value,
    outputs: &BTreeMap<String, Value>,
) -> Option<Value> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    if key == "input" {
        return Some(input.clone());
    }
    if let Some(path) = key.strip_prefix("input.") {
        return resolve_path(input, path);
    }
    let (root, path) = match key.split_once('.') {
        Some((root, path)) => (root, path),
        None => (key, ""),
    };
    let value = outputs.get(root)?;
    if path.is_empty() {
        Some(value.clone())
    } else {
        resolve_path(value, path)
    }
}

fn resolve_path(value: &Value, path: &str) -> Option<Value> {
    let mut current = value;
    for part in path.split('.').filter(|part| !part.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(items) => {
                let index: usize = part.parse().ok()?;
                items.get(index)?
            }
            _ => return None,
        };
    }
    Some(current.clone())
}

fn value_text(value: &Value, max_chars: usize) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    };
    truncate_chars(&text, max_chars)
}
