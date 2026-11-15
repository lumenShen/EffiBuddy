//! Skill discovery for the composer.
//!
//! Skills are Markdown instruction files, mirroring the ZCode/Claude layout:
//!
//! - `<data-dir>/skills/<name>/SKILL.md` (user scope)
//! - `<workspace>/.effibuddy/skills/<name>/SKILL.md` (project scope)
//!
//! A plain `<name>.md` next to those directories is accepted too. Project
//! skills override user skills with the same normalized name. Only metadata is
//! read here; the body is loaded when the user activates a skill, so a huge
//! catalog never slows down typing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) const SKILL_FILE: &str = "SKILL.md";
const MAX_SKILL_BYTES: u64 = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkillScope {
    User,
    Project,
}

impl SkillScope {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SkillScope::User => "用户",
            SkillScope::Project => "项目",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
}

impl Skill {
    /// Path shown in the menu: workspace-relative for project skills.
    pub(crate) fn display_path(&self, workspace: &Path) -> String {
        match self.path.strip_prefix(workspace) {
            Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
            Err(_) => self.path.to_string_lossy().into_owned(),
        }
    }
}

/// Load the merged catalog for one workspace.
pub(crate) fn load_skills(workspace: &Path) -> Vec<Skill> {
    let user_dir = crate::config::base_dir().join("skills");
    let project_dir = workspace.join(".effibuddy").join("skills");
    load_skills_from(&user_dir, &project_dir)
}

pub(crate) fn load_skills_from(user_dir: &Path, project_dir: &Path) -> Vec<Skill> {
    let mut catalog: Vec<Skill> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (dir, scope) in [
        (user_dir, SkillScope::User),
        (project_dir, SkillScope::Project),
    ] {
        for skill in scan_dir(dir, scope) {
            let key = normalize_name(&skill.name);
            match index.get(&key) {
                Some(existing) => catalog[*existing] = skill,
                None => {
                    index.insert(key, catalog.len());
                    catalog.push(skill);
                }
            }
        }
    }
    catalog.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
    catalog
}

/// Read one skill's Markdown body for activation. Refuses oversized files so a
/// typo cannot dump a megabyte into the next request.
pub(crate) fn read_skill_body(skill: &Skill) -> Result<(String, bool, u64), String> {
    use std::io::Read;

    let metadata = std::fs::metadata(&skill.path)
        .map_err(|error| format!("无法读取技能 {}：{error}", skill.path.display()))?;
    let mut file = std::fs::File::open(&skill.path)
        .map_err(|error| format!("无法读取技能 {}：{error}", skill.path.display()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SKILL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取技能 {}：{error}", skill.path.display()))?;
    let truncated = metadata.len() > MAX_SKILL_BYTES;
    let mut end = bytes.len().min(MAX_SKILL_BYTES as usize);
    while end > 0 && !is_boundary(&bytes, end) {
        end -= 1;
    }
    Ok((
        String::from_utf8_lossy(&bytes[..end]).into_owned(),
        truncated,
        metadata.len(),
    ))
}

fn is_boundary(bytes: &[u8], index: usize) -> bool {
    index >= bytes.len() || (bytes[index] & 0xC0) != 0x80
}

pub(crate) fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_whitespace() || ch == '_' {
            if !last_dash && !out.is_empty() {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(ch);
            last_dash = false;
        }
    }
    out
}

fn scan_dir(dir: &Path, scope: SkillScope) -> Vec<Skill> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    let mut skills = Vec::new();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file = if path.is_dir() {
            let candidate = path.join(SKILL_FILE);
            if !candidate.is_file() {
                continue;
            }
            candidate
        } else if path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        }) {
            path.clone()
        } else {
            continue;
        };
        let fallback = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or(name);
        if let Some(skill) = parse_skill_file(&file, &fallback, scope) {
            skills.push(skill);
        }
    }
    skills
}

fn parse_skill_file(path: &Path, fallback_name: &str, scope: SkillScope) -> Option<Skill> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_SKILL_BYTES {
        // Metadata only: the menu still lists it, activation reports truncation.
        return Some(Skill {
            name: fallback_name.to_string(),
            description: String::new(),
            path: path.to_path_buf(),
            scope,
        });
    }
    let text = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&text);
    let name = frontmatter
        .get("name")
        .map(String::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name)
        .to_string();
    let description = frontmatter
        .get("description")
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| first_line(body));
    Some(Skill {
        name,
        description: truncate_chars(&description, 220),
        path: path.to_path_buf(),
        scope,
    })
}

fn split_frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut map = HashMap::new();
    let Some(rest) = text.strip_prefix("---") else {
        return (map, text);
    };
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest);
    let mut offset = text.len() - rest.len();
    let mut lines = rest.split_inclusive('\n');
    let mut consumed = 0usize;
    let mut close = None;
    for line in lines.by_ref() {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            close = Some(consumed + line.len());
            break;
        }
        consumed += line.len();
    }
    let Some(close) = close else {
        return (HashMap::new(), text);
    };
    let header = &rest[..close];
    map = parse_header(header);
    offset += close;
    (map, &text[offset.min(text.len())..])
}

fn parse_header(header: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut current: Option<String> = None;
    for line in header.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation of a folded value.
            if let Some(key) = current.clone()
                && let Some(value) = map.get_mut(&key)
            {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        if value == ">" || value == "|" {
            map.insert(key.clone(), String::new());
        } else {
            map.insert(key.clone(), value);
        }
        current = Some(key);
    }
    map
}

fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches(['#', '-', '*', '>']).trim())
        .unwrap_or("")
        .to_string()
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn parses_frontmatter_and_falls_back_to_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path();
        write(
            &skills.join("review").join(SKILL_FILE),
            "---\nname: Strict Reviewer\ndescription: >\n  Review code\n  with a strict bar\n---\n# Body\n",
        );
        write(
            &skills.join("plain.md"),
            "# Plain skill\n\nDoes plain things.\n",
        );
        let catalog = load_skills_from(&skills.join("missing-user"), skills);
        assert_eq!(catalog.len(), 2);
        let review = catalog
            .iter()
            .find(|skill| skill.name == "Strict Reviewer")
            .unwrap();
        assert_eq!(review.description, "Review code with a strict bar");
        let plain = catalog.iter().find(|skill| skill.name == "plain").unwrap();
        assert_eq!(plain.description, "Plain skill");
    }

    #[test]
    fn project_skills_override_user_skills_by_normalized_name() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let project = dir.path().join("project");
        write(
            &user.join("lint").join(SKILL_FILE),
            "---\nname: Lint\n---\nuser body\n",
        );
        write(
            &project.join("lint").join(SKILL_FILE),
            "---\nname: Lint\n---\nproject body\n",
        );
        let catalog = load_skills_from(&user, &project);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].scope, SkillScope::Project);
        assert_eq!(catalog[0].description, "project body");
    }

    #[test]
    fn read_body_reports_size_and_truncation_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SKILL_FILE);
        write(&path, "---\nname: x\n---\nhello\n");
        let skill = Skill {
            name: "x".to_string(),
            description: String::new(),
            path: path.clone(),
            scope: SkillScope::Project,
        };
        let (body, truncated, size) = read_skill_body(&skill).unwrap();
        assert!(!truncated);
        assert!(size > 0);
        assert!(body.contains("hello"));
        assert_eq!(normalize_name("  My  Skill_Name "), "my-skill-name");
    }
}
