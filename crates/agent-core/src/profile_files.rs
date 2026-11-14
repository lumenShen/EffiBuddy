//! Markdown-defined subagent profiles.
//!
//! Custom agent types live in Markdown files with a frontmatter header; the
//! body is the system prompt. Two scopes are loaded, later ones overriding
//! earlier ones by normalized name:
//!
//! 1. built-ins defined in code ([`crate::builtin_subagent_profiles`]);
//! 2. user scope, `<config>/EffiBuddy/agents/*.md`;
//! 3. project scope, `<workspace>/.effibuddy/agents/*.md`.
//!
//! The frontmatter parser is deliberately **loose**: a hand-rolled
//! key/value reader, not a YAML implementation. It accepts what profile
//! authors actually write (inline `[a, b]` lists, dash lists, quotes,
//! full-line comments) and normalizes invalid values instead of failing.
//! Only two problems are fatal to a file: missing `name`/`description` or
//! missing frontmatter/prompt. Everything else yields a diagnostic and the
//! file still loads where possible.

use std::collections::HashMap;
use std::path::Path;

use crate::subagent::{ProfileSource, SubagentColor, SubagentProfile, normalize_profile_name};

/// Why a Markdown profile file could not be used. Diagnostics surface in the
/// settings UI; they never abort startup or a spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileDiagnostic {
    /// File name (not full path) the problem was found in.
    pub file: String,
    pub message: String,
}

impl ProfileDiagnostic {
    fn new(file: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProfileDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file, self.message)
    }
}

/// Parse one Markdown profile document.
///
/// `label` identifies the file in error messages. The returned profile is
/// fully normalized; `Err` carries a human-readable reason.
pub fn parse_profile_markdown(
    source: ProfileSource,
    label: &str,
    text: &str,
) -> Result<SubagentProfile, ProfileDiagnostic> {
    let text = text.trim_start_matches('\u{feff}');
    let (frontmatter, body) = split_frontmatter(text)
        .ok_or_else(|| ProfileDiagnostic::new(label, "缺少 `---` frontmatter 头"))?;

    let fields = parse_frontmatter(&frontmatter);
    let name = field(&fields, "name")
        .ok_or_else(|| ProfileDiagnostic::new(label, "缺少必填字段 `name`"))?;
    let description = field(&fields, "description")
        .ok_or_else(|| ProfileDiagnostic::new(label, "缺少必填字段 `description`"))?;
    let prompt = body.trim().to_string();
    if prompt.is_empty() {
        return Err(ProfileDiagnostic::new(
            label,
            "frontmatter 之后没有 system prompt 正文",
        ));
    }
    if normalize_profile_name(&name) == "main" {
        return Err(ProfileDiagnostic::new(label, "代理名 `main` 被保留"));
    }

    let tools = optional_list(&fields, "tools");
    // `["*"]` explicitly asks for the inherited set, which is what an empty
    // allow-list means here.
    let tools: Vec<String> = if tools.iter().any(|tool| tool == "*") {
        Vec::new()
    } else {
        tools
    };
    let disallowed_tools = optional_list(&fields, "disallowed_tools");
    let color = match field(&fields, "color") {
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "green" => SubagentColor::Green,
            "purple" => SubagentColor::Purple,
            "amber" | "yellow" | "orange" => SubagentColor::Amber,
            // Unknown colors fall back silently instead of rejecting the file.
            _ => SubagentColor::Blue,
        },
        None => SubagentColor::Blue,
    };
    Ok(SubagentProfile {
        name: name.trim().to_string(),
        description: description.trim().to_string(),
        prompt,
        tools,
        disallowed_tools,
        model: field(&fields, "model").map(|value| value.trim().to_string()),
        read_only: bool_field(&fields, "read_only"),
        allow_spawn: bool_field(&fields, "allow_spawn"),
        color,
        source,
    })
}

/// Split `---`-delimited frontmatter from the body. Returns
/// `(frontmatter, body)`; `None` when the document has no frontmatter block.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let mut lines = text.lines();
    let opener = lines.next()?.trim_end();
    if opener != "---" {
        return None;
    }
    let rest = text.strip_prefix(opener)?.trim_start_matches(['\r', '\n']);
    let (frontmatter, body) = rest.split_once("\n---")?;
    // Anything directly after the closing fence must be a line break; a
    // second `---` run (horizontal rule) is not part of the fence.
    let body = body.strip_prefix('\n').unwrap_or(body);
    if let Some(rest) = body.strip_prefix("---") {
        if rest.starts_with(|c: char| !c.is_whitespace()) {
            return None;
        }
    }
    Some((frontmatter, body))
}

/// Loose frontmatter key/value reader.
///
/// Supports `key: value`, inline lists `[a, b]`, dash-list continuations and
/// `#` comments. Unknown keys are ignored; values keep their raw string form
/// (quotes stripped) under a lowercase key.
fn parse_frontmatter(frontmatter: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let mut current_key: Option<String> = None;
    for raw_line in frontmatter.lines() {
        let line = strip_comment(raw_line);
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim();
        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some(key) = &current_key {
                let item = strip_inline_comment(item.trim());
                let entry: &mut String = fields.entry(key.clone()).or_default();
                if !entry.is_empty() {
                    entry.push('\n');
                }
                entry.push_str(&normalize_value(item));
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = strip_inline_comment(value.trim());
        if value.is_empty() {
            // Value may arrive on following dash lines.
            fields.entry(key.clone()).or_default();
            current_key = Some(key);
        } else {
            current_key = Some(key.clone());
            fields.insert(key, normalize_value(value));
        }
    }
    fields
}

/// Normalize one raw value into the newline-joined item form: strip a
/// surrounding `[...]`, split on commas, drop `(...)` tool arguments
/// (`grep(foo)` → `grep`) and surrounding quotes.
fn normalize_value(value: &str) -> String {
    let value = value.trim();
    let value = match (value.starts_with('['), value.ends_with(']')) {
        (true, true) => &value[1..value.len() - 1],
        _ => value,
    };
    value
        .split(',')
        .map(|part| {
            let part = part.trim();
            let part = match part.find('(') {
                Some(open) if part.ends_with(')') => &part[..open],
                _ => part,
            };
            part.trim().trim_matches('"').trim_matches('\'').trim()
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_comment(line: &str) -> &str {
    let trimmed_start = line.trim_start();
    if trimmed_start.starts_with('#') {
        return "";
    }
    line
}

/// Drop a trailing `# comment`. Quoted values keep their `#`; unquoted values
/// cut at the first ` #` (YAML requires whitespace before an inline comment).
fn strip_inline_comment(value: &str) -> &str {
    match value.chars().next() {
        Some(quote @ ('"' | '\'')) => match value[1..].find(quote).map(|index| index + 1) {
            Some(close) => {
                if value[close..].trim_start().starts_with('#') {
                    &value[..close]
                } else {
                    value
                }
            }
            None => value,
        },
        _ => match value.find(" #") {
            Some(index) => value[..index].trim_end(),
            None => value,
        },
    }
}

fn field(fields: &HashMap<String, String>, key: &str) -> Option<String> {
    fields
        .get(key)
        .map(|value| {
            // Dash-list values are newline-joined; single values pass through.
            value.split('\n').collect::<Vec<_>>().join(", ")
        })
        .filter(|value| !value.trim().is_empty())
}

/// Read a list-valued field (`[a, b]`, dash lines or a single name).
fn optional_list(fields: &HashMap<String, String>, key: &str) -> Vec<String> {
    fields
        .get(key)
        .map(|value| value.split('\n').map(str::to_string).collect())
        .unwrap_or_default()
}

fn bool_field(fields: &HashMap<String, String>, key: &str) -> bool {
    field(fields, key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1"
            )
        })
        .unwrap_or(false)
}

/// Load every Markdown profile directly inside `dir` (one level, no
/// recursion), sorted by file name. A missing directory is normal and yields
/// nothing; unreadable or invalid files yield diagnostics.
pub fn load_profile_dir(
    dir: &Path,
    source: ProfileSource,
) -> (Vec<SubagentProfile>, Vec<ProfileDiagnostic>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Missing scope directories are the common case, not an error.
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(|extension| {
                        extension.eq_ignore_ascii_case("md")
                            || extension.eq_ignore_ascii_case("markdown")
                    })
                    .unwrap_or(false)
        })
        .collect();
    files.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });

    let mut profiles = Vec::new();
    let mut diagnostics = Vec::new();
    for path in files {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                diagnostics.push(ProfileDiagnostic::new(
                    file_name,
                    format!("无法读取文件：{error}"),
                ));
                continue;
            }
        };
        match parse_profile_markdown(source, &file_name, &text) {
            Ok(profile) => profiles.push(profile),
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    (profiles, diagnostics)
}

/// Merge a catalog by normalized profile name; later entries win, so files
/// can override built-ins and project files can override user files.
pub fn merge_profiles(
    catalogs: Vec<(ProfileSource, Vec<SubagentProfile>)>,
) -> Vec<SubagentProfile> {
    let mut by_name: HashMap<String, SubagentProfile> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (_, profiles) in catalogs {
        for profile in profiles {
            let key = normalize_profile_name(&profile.name);
            if !by_name.contains_key(&key) {
                order.push(key.clone());
            }
            by_name.insert(key, profile);
        }
    }
    order
        .into_iter()
        .filter_map(|key| by_name.remove(&key))
        .collect()
}

/// The effective catalog for one workspace: built-ins, then the user scope,
/// then the project scope (later scopes override by name).
pub fn merged_subagent_profiles(
    user_agents_dir: &Path,
    workspace: Option<&Path>,
) -> (Vec<SubagentProfile>, Vec<ProfileDiagnostic>) {
    let mut diagnostics = Vec::new();
    let mut catalogs = vec![(ProfileSource::BuiltIn, crate::builtin_subagent_profiles())];
    let (user, user_diagnostics) = load_profile_dir(user_agents_dir, ProfileSource::User);
    diagnostics.extend(user_diagnostics);
    catalogs.push((ProfileSource::User, user));
    if let Some(workspace) = workspace {
        let (project, project_diagnostics) = load_profile_dir(
            &workspace.join(".effibuddy").join("agents"),
            ProfileSource::Project,
        );
        diagnostics.extend(project_diagnostics);
        catalogs.push((ProfileSource::Project, project));
    }
    (merge_profiles(catalogs), diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<SubagentProfile, ProfileDiagnostic> {
        parse_profile_markdown(ProfileSource::Project, "test.md", text)
    }

    #[test]
    fn parses_inline_and_dash_lists() {
        let profile = parse(
            "---\nname: scout\ndescription: 快速侦察\ntools: [read_file, grep, shell(git:*)]\n---\nBody line.\n",
        )
        .unwrap();
        assert_eq!(profile.name, "scout");
        assert_eq!(profile.tools, vec!["read_file", "grep", "shell"]);
        assert!(profile.allows("grep"));
        assert_eq!(profile.prompt, "Body line.");
        assert_eq!(profile.source, ProfileSource::Project);
    }

    #[test]
    fn parses_dash_list_block() {
        let profile = parse(
            "---\nname: scout\ndescription: d\ntools:\n  - read_file\n  - grep\ndisallowed_tools:\n  - shell\n---\nBody.",
        )
        .unwrap();
        assert_eq!(profile.tools, vec!["read_file", "grep"]);
        assert_eq!(profile.disallowed_tools, vec!["shell"]);
        assert!(!profile.allows("shell"));
    }

    #[test]
    fn wildcard_tools_become_inherit() {
        let profile =
            parse("---\nname: scout\ndescription: d\ntools: [\"*\"]\n---\nBody.").unwrap();
        assert!(profile.tools.is_empty());
        assert!(profile.allows("write_file"));
    }

    #[test]
    fn disallowed_blocks_even_when_inheriting() {
        let profile = parse(
            "---\nname: scout\ndescription: d\ndisallowed_tools: [write_file, shell]\n---\nBody.",
        )
        .unwrap();
        assert!(profile.allows("read_file"));
        assert!(!profile.allows("write_file"));
        assert!(!profile.allows("shell"));
    }

    #[test]
    fn normalizes_invalid_values_silently() {
        let profile = parse(
            "---\nname: scout\ndescription: d\ncolor: neon\nread_only: maybe\n# comment\n---\nBody.",
        )
        .unwrap();
        assert_eq!(profile.color, SubagentColor::Blue);
        assert!(!profile.read_only);
    }

    #[test]
    fn model_field_preserved_and_inherit_detected() {
        let profile =
            parse("---\nname: scout\ndescription: d\nmodel: inherit\n---\nBody.").unwrap();
        assert!(profile.inherits_model());
        let profile =
            parse("---\nname: scout\ndescription: d\nmodel: deepseek/deepseek-chat\n---\nBody.")
                .unwrap();
        assert!(!profile.inherits_model());
        assert_eq!(profile.model.as_deref(), Some("deepseek/deepseek-chat"));
    }

    #[test]
    fn missing_name_or_description_or_body_is_fatal() {
        assert!(parse("no frontmatter at all").is_err());
        assert!(parse("---\ndescription: d\n---\nBody.").is_err());
        assert!(parse("---\nname: n\n---\nBody.").is_err());
        assert!(parse("---\nname: n\ndescription: d\n---\n   \n").is_err());
    }

    #[test]
    fn reserved_name_rejected() {
        let error = parse("---\nname: main\ndescription: d\n---\nBody.").unwrap_err();
        assert!(error.message.contains("保留"));
    }

    #[test]
    fn quotes_and_comments_stripped() {
        let profile = parse(
            "---\nname: \"quoted\"\ndescription: 'single'\n# full-line comment\ntools: [a] # inline comment\n---\nBody.",
        )
        .unwrap();
        assert_eq!(profile.name, "quoted");
        assert_eq!(profile.description, "single");
        assert_eq!(profile.tools, vec!["a"]);
    }

    #[test]
    fn handles_crlf_and_bom() {
        let profile = parse_profile_markdown(
            ProfileSource::User,
            "crlf.md",
            "\u{feff}---\r\nname: win\r\ndescription: d\r\n---\r\nBody.",
        )
        .unwrap();
        assert_eq!(profile.name, "win");
    }

    #[test]
    fn merge_orders_and_overrides_by_name() {
        let builtin = crate::builtin_subagent_profiles();
        let user = vec![SubagentProfile {
            name: "explore".to_string(),
            description: "user explore".to_string(),
            prompt: "p".to_string(),
            tools: Vec::new(),
            disallowed_tools: Vec::new(),
            model: None,
            read_only: true,
            allow_spawn: false,
            color: SubagentColor::Amber,
            source: ProfileSource::User,
        }];
        let merged = merge_profiles(vec![
            (ProfileSource::BuiltIn, builtin),
            (ProfileSource::User, user),
        ]);
        let explore = merged
            .iter()
            .find(|profile| profile.name == "explore")
            .unwrap();
        assert_eq!(explore.description, "user explore");
        assert_eq!(explore.source, ProfileSource::User);
        // Built-in order is preserved for the rest.
        assert_eq!(merged[0].name, "general-purpose");
    }

    #[test]
    fn missing_dir_is_silent() {
        let (profiles, diagnostics) = load_profile_dir(
            &std::path::PathBuf::from("Z:/definitely/missing"),
            ProfileSource::User,
        );
        assert!(profiles.is_empty());
        assert!(diagnostics.is_empty());
    }
}
