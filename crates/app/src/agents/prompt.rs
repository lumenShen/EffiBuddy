//! System prompt for a child agent.
//!
//! Kept separate from the runtime so the prompt can be read, reviewed and
//! tested without following the execution plumbing.

use effibuddy_agent_core::{MAX_AGENT_DEPTH, SubagentProfile, SubagentRequest};

pub(crate) fn child_prompt(
    profile: &SubagentProfile,
    request: &SubagentRequest,
    name: &str,
    allow_spawn: bool,
    project_instructions: &str,
) -> String {
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let parent_name = request.parent_agent_name.as_deref().unwrap_or("main");
    let parent_id = request.parent_agent_id.as_deref().unwrap_or("main");
    let additional = if request.additional_workspaces.is_empty() {
        String::new()
    } else {
        format!(
            "\n- Additional working directories: {}",
            request
                .additional_workspaces
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut prompt = format!(
        r#"You are EffiBuddy agent `{name}` — {description}.
Profile: {profile}

{body}

# Launcher context
- You were launched by `{parent_name}` (agent id `{parent_id}`), not by the user.
- Workspace root: {workspace}{additional}
- Platform: {os}
- Date: {date}
- Tree depth: {depth} / {max_depth}
- Visibility: {visibility}

# Agent network
- Use `agent_list` to see the agents you may address, then `agent_send` to message one by name or id.
- Use `main` to report to the main agent, or message a sibling/child directly when the task genuinely
  needs a hand-off. Messages are model-visible; send plain instructions or structured data.
- You cannot ask the user questions and must not wait for confirmation; report blockers instead.
- {spawn_rule}
- Only your final message is returned to your launcher. Write it as a compact report.
- Do not create report, summary, or findings Markdown files.
- Refer to files relative to the workspace root; absolute paths inside additional working
  directories are allowed."#,
        name = name,
        description = request.description,
        profile = profile.name,
        body = profile.prompt.trim(),
        workspace = request.workspace.display(),
        os = std::env::consts::OS,
        date = date,
        depth = request.depth,
        max_depth = MAX_AGENT_DEPTH,
        visibility = request.visibility.label(),
        spawn_rule = if allow_spawn {
            "You may create your own child agents with the `agent` tool, or run an agent group with \
`workflow_run`. Choose `visibility: \"private\"` for one-off helpers and `\"public\"` for reusable \
specialists. Keep the tree shallow and do not create chains just to pass messages."
        } else {
            "You cannot launch other agents in this run. Finish the work yourself or hand data \
back to your launcher."
        },
    );
    if let Some(schema) = &request.output_schema {
        prompt.push_str(
            "\n\n# Required output\nReturn ONLY one JSON value that validates against the JSON \
Schema below. Do not wrap it in prose, Markdown fences, or a code block.\n\n",
        );
        prompt
            .push_str(&serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string()));
    }
    let instructions = project_instructions.trim();
    if !instructions.is_empty() {
        prompt.push_str("\n\n# Project instructions\n");
        prompt.push_str(instructions);
    }
    prompt.push_str("\n\n# Task summary\n");
    prompt.push_str(&request.description);
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::{AgentVisibility, builtin_subagent_profiles};

    fn request() -> SubagentRequest {
        SubagentRequest {
            profile: "explore".to_string(),
            description: "find the parser".to_string(),
            prompt: "Find it.".to_string(),
            background: false,
            parent_session_id: "parent".to_string(),
            parent_call_id: "call".to_string(),
            workspace: std::env::temp_dir(),
            additional_workspaces: Vec::new(),
            agent_name: Some("scout".to_string()),
            parent_agent_id: Some("agent_owner".to_string()),
            parent_agent_name: Some("researcher".to_string()),
            depth: 2,
            system_prompt: None,
            tools: None,
            allow_spawn: true,
            read_only: Some(true),
            output_schema: Some(serde_json::json!({
                "type": "object",
                "required": ["facts"]
            })),
            visibility: AgentVisibility::Private,
            group_id: None,
            group_name: None,
        }
    }

    #[test]
    fn prompt_names_the_launcher_visibility_and_contract() {
        let profile = builtin_subagent_profiles()
            .into_iter()
            .find(|profile| profile.name == "explore")
            .unwrap();
        let prompt = child_prompt(&profile, &request(), "scout", true, "");
        assert!(prompt.contains("launched by `researcher`"));
        assert!(prompt.contains("Visibility: 私有"));
        assert!(prompt.contains("You may create your own child agents"));
        assert!(prompt.contains("Required output"));
        assert!(prompt.contains("\"facts\""));
        assert!(prompt.contains(&format!("Tree depth: 2 / {MAX_AGENT_DEPTH}")));
    }
}
