//! Session export: one self-contained zip per conversation.
//!
//! The archive keeps the *structured* data instead of a rendered transcript:
//!
//! - `session.json` — the main agent's full [`SessionState`],
//! - `agents/<n>-<name>-<id>/session.json` — every descendant subagent,
//! - `files/` — the files the conversation pulled in (composer attachments,
//!   images and `$skill` bodies, plus workspace files referenced with `@`),
//! - `manifest.json` — the agent tree and a file index,
//! - `README.md` — a short human-readable guide.
//!
//! Files are written as stored (uncompressed) zip entries; images are already
//! compressed and the JSON barely benefits, so this avoids pulling a
//! compression backend into the app.

use std::collections::{HashSet, VecDeque};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use chrono::{DateTime, Utc};
use effibuddy_agent_core::{AttachmentKind, Role, RunStatus, SessionState, UserAttachment};
use serde::Serialize;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::store::SessionStore;

const SCHEMA_VERSION: u32 = 1;

/// Result of one export, used for the status line.
pub struct ExportReport {
    pub path: PathBuf,
    pub subagents: usize,
    pub files: usize,
    pub missing: usize,
}

#[derive(Serialize)]
struct Manifest {
    schema: u32,
    app_version: String,
    exported_at: DateTime<Utc>,
    session: SessionMeta,
    agents: Vec<AgentMeta>,
    files: Vec<FileMeta>,
    missing: Vec<MissingFile>,
}

#[derive(Serialize)]
struct SessionMeta {
    id: String,
    title: String,
    workspace: String,
    model: String,
    provider_id: Option<String>,
    project: Option<String>,
    project_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    message_count: usize,
}

#[derive(Serialize)]
struct AgentMeta {
    id: String,
    name: String,
    description: Option<String>,
    parent_id: Option<String>,
    model: String,
    provider_id: Option<String>,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    message_count: usize,
    archive_dir: String,
}

#[derive(Serialize)]
struct FileMeta {
    zip_path: String,
    source: String,
    name: String,
    kind: String,
    bytes: u64,
    truncated: bool,
}

#[derive(Clone, Serialize)]
struct MissingFile {
    source: String,
    name: String,
    kind: String,
    reason: String,
}

enum Content {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

struct PendingFile {
    category: Category,
    /// Path shown in the manifest (workspace-relative where possible).
    display: String,
    name: String,
    kind: String,
    bytes: u64,
    truncated: bool,
    content: Content,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Attachment,
    Mention,
    Skill,
}

impl Category {
    fn directory(self) -> &'static str {
        match self {
            Category::Attachment => "files/attachments",
            Category::Mention => "files/mentions",
            Category::Skill => "files/skills",
        }
    }
}

/// All subagent transcripts below `root_id`, oldest first. Runs on the
/// background executor; `child_sessions` re-reads the session directory.
pub fn descendants(store: &SessionStore, root_id: &str) -> Vec<SessionState> {
    let mut children = Vec::new();
    let mut queue = VecDeque::from([root_id.to_string()]);
    let mut seen: HashSet<String> = HashSet::from([root_id.to_string()]);
    while let Some(parent) = queue.pop_front() {
        for child in store.child_sessions_raw(&parent) {
            if !seen.insert(child.id.clone()) {
                continue;
            }
            queue.push_back(child.id.clone());
            children.push(child);
        }
    }
    children.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    children
}

/// Write the zip. `project_roots` are the additional folders of the owning
/// project (may be empty for plain tasks); every session's own cwd is tried
/// first when resolving relative attachment/mention paths.
pub fn export(
    main: &SessionState,
    children: &[SessionState],
    project_roots: &[PathBuf],
    destination: &Path,
) -> Result<ExportReport, String> {
    let mut roots = vec![main.cwd.clone()];
    for root in project_roots {
        if !roots.iter().any(|existing| existing == root) {
            roots.push(root.clone());
        }
    }

    let mut pending: Vec<PendingFile> = Vec::new();
    let mut missing: Vec<MissingFile> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    collect_session_files(main, &roots, &mut pending, &mut missing, &mut seen);
    for child in children {
        let mut child_roots = vec![child.cwd.clone()];
        for root in project_roots {
            if !child_roots.iter().any(|existing| existing == root) {
                child_roots.push(root.clone());
            }
        }
        collect_session_files(child, &child_roots, &mut pending, &mut missing, &mut seen);
    }

    let mut used_names: HashSet<String> = HashSet::new();
    let mut file_meta: Vec<FileMeta> = Vec::with_capacity(pending.len());
    let mut entries: Vec<(String, Content, u64)> = Vec::with_capacity(pending.len());
    for file in pending {
        let candidate = match file.category {
            Category::Skill => format!(
                "{}/{}.md",
                file.category.directory(),
                safe_segment(file.name.trim_end_matches(".md"))
            ),
            _ => format!(
                "{}/{}",
                file.category.directory(),
                safe_relative(&file.display).unwrap_or_else(|| safe_segment(&file.name))
            ),
        };
        let zip_path = unique_entry_name(&mut used_names, candidate);
        file_meta.push(FileMeta {
            zip_path: zip_path.clone(),
            source: file.display.clone(),
            name: file.name.clone(),
            kind: file.kind.clone(),
            bytes: file.bytes,
            truncated: file.truncated,
        });
        entries.push((zip_path, file.content, file.bytes));
    }

    let manifest = Manifest {
        schema: SCHEMA_VERSION,
        app_version: crate::update::current_version().to_string(),
        exported_at: Utc::now(),
        session: SessionMeta {
            id: main.id.clone(),
            title: main.display_title(),
            workspace: main.cwd.display().to_string(),
            model: main.model.clone(),
            provider_id: main.provider_id.clone(),
            project: main.project.as_ref().map(|path| path.display().to_string()),
            project_id: main.project_id.clone(),
            created_at: main.created_at,
            updated_at: main.updated_at,
            message_count: main.messages.len(),
        },
        agents: children
            .iter()
            .enumerate()
            .map(|(index, child)| AgentMeta {
                id: child.id.clone(),
                name: child
                    .agent_name
                    .clone()
                    .unwrap_or_else(|| "agent".to_string()),
                description: child.agent_description.clone(),
                parent_id: child.parent_id.clone(),
                model: child.model.clone(),
                provider_id: child.provider_id.clone(),
                status: run_status_label(child.status).to_string(),
                created_at: child.created_at,
                updated_at: child.updated_at,
                message_count: child.messages.len(),
                archive_dir: agent_dir(index, child),
            })
            .collect(),
        files: file_meta,
        missing: missing.clone(),
    };

    let temporary = destination.with_extension("zip.part");
    if let Err(error) = write_archive(&temporary, main, children, &manifest, entries) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if destination.exists() {
        let _ = std::fs::remove_file(destination);
    }
    if let Err(error) = std::fs::rename(&temporary, destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("无法保存 {}：{error}", destination.display()));
    }

    Ok(ExportReport {
        path: destination.to_path_buf(),
        subagents: children.len(),
        files: manifest.files.len(),
        missing: manifest.missing.len(),
    })
}

fn write_archive(
    destination: &Path,
    main: &SessionState,
    children: &[SessionState],
    manifest: &Manifest,
    entries: Vec<(String, Content, u64)>,
) -> Result<(), String> {
    let reader = std::fs::File::create(destination)
        .map_err(|error| format!("无法创建 {}：{error}", destination.display()))?;
    let mut zip = ZipWriter::new(BufWriter::new(reader));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o644);

    write_bytes(
        &mut zip,
        "README.md",
        readme(main, children.len(), manifest.files.len(), manifest.missing.len()).as_bytes(),
        options,
    )?;
    write_json(&mut zip, "manifest.json", manifest, options)?;
    write_json(&mut zip, "session.json", main, options)?;
    for (index, child) in children.iter().enumerate() {
        write_json(
            &mut zip,
            &format!("{}/session.json", agent_dir(index, child)),
            child,
            options,
        )?;
    }
    for (zip_path, content, _) in entries {
        zip.start_file(zip_path.clone(), options)
            .map_err(|error| format!("无法写入 zip 条目 {zip_path}：{error}"))?;
        match content {
            Content::Bytes(bytes) => zip
                .write_all(&bytes)
                .map_err(|error| format!("无法写入 {zip_path}：{error}"))?,
            Content::Path(path) => {
                let mut source = std::fs::File::open(&path)
                    .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
                std::io::copy(&mut source, &mut zip)
                    .map_err(|error| format!("无法写入 {zip_path}：{error}"))?;
            }
        }
    }
    let mut writer = zip
        .finish()
        .map_err(|error| format!("无法完成 zip：{error}"))?;
    writer
        .flush()
        .map_err(|error| format!("无法写入 {}：{error}", destination.display()))
}

fn collect_session_files(
    session: &SessionState,
    roots: &[PathBuf],
    pending: &mut Vec<PendingFile>,
    missing: &mut Vec<MissingFile>,
    seen: &mut HashSet<String>,
) {
    for message in &session.messages {
        for attachment in &message.attachments {
            let disk = resolve_attachment_path(attachment, roots);
            let key = attachment_key(attachment, disk.as_deref());
            if !seen.insert(key) {
                continue;
            }
            let payload = attachment_payload(attachment, disk);
            let category = match attachment.kind {
                AttachmentKind::Skill => Category::Skill,
                _ => Category::Attachment,
            };
            let kind = match attachment.kind {
                AttachmentKind::File => "file",
                AttachmentKind::Image => "image",
                AttachmentKind::Skill => "skill",
            }
            .to_string();
            match payload {
                Some((content, bytes, truncated)) => pending.push(PendingFile {
                    category,
                    display: attachment.path.clone(),
                    name: attachment.name.clone(),
                    kind,
                    bytes,
                    truncated,
                    content,
                }),
                None => missing.push(MissingFile {
                    source: attachment.path.clone(),
                    name: attachment.name.clone(),
                    kind,
                    reason: "附件内容不可用（文件已移动或内联内容缺失）".to_string(),
                }),
            }
        }

        // `@path` mentions are plain text in the transcript; pull the files
        // they pointed at into the archive as well. `@agent` references simply
        // fail to resolve and are ignored.
        if message.role == Role::User && !message.synthetic && message.agent_message.is_none() {
            for token in mention_tokens(&message.text()) {
                let Some(path) = resolve_mention(&token, roots) else {
                    // `@agent` references are not files; only report tokens
                    // that clearly look like paths.
                    if looks_like_file_mention(&token)
                        && seen.insert(format!("missing:mention:{token}"))
                    {
                        missing.push(MissingFile {
                            source: token.clone(),
                            name: token,
                            kind: "mentioned".to_string(),
                            reason: "未找到 @ 引用的文件".to_string(),
                        });
                    }
                    continue;
                };
                let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let key = format!("disk:{}", canonical.display());
                if !seen.insert(key) {
                    continue;
                }
                let Ok(metadata) = std::fs::metadata(&path) else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| token.clone());
                pending.push(PendingFile {
                    category: Category::Mention,
                    display: display_path(&path, roots),
                    name,
                    kind: "mentioned".to_string(),
                    bytes: metadata.len(),
                    truncated: false,
                    content: Content::Path(path),
                });
            }
        }
    }
}

fn attachment_key(attachment: &UserAttachment, disk: Option<&Path>) -> String {
    match disk {
        Some(path) => match std::fs::canonicalize(path) {
            Ok(canonical) => format!("disk:{}", canonical.display()),
            Err(_) => format!("disk:{}", path.display()),
        },
        None => format!(
            "attachment:{:?}:{}:{}:{}",
            attachment.kind, attachment.name, attachment.size, attachment.path
        ),
    }
}

fn attachment_payload(
    attachment: &UserAttachment,
    disk: Option<PathBuf>,
) -> Option<(Content, u64, bool)> {
    let disk_available = disk.as_deref().is_some_and(Path::is_file);
    let disk_len = disk
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len());

    if attachment.kind == AttachmentKind::Image {
        if let Some(image) = &attachment.image {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&image.data) {
                let len = bytes.len() as u64;
                return Some((Content::Bytes(bytes), len, false));
            }
        }
        return disk.filter(|path| path.is_file()).map(|path| {
            (
                Content::Path(path),
                disk_len.unwrap_or(attachment.size),
                false,
            )
        });
    }

    let text = attachment.text.as_ref();
    match text {
        Some(text) if attachment.kind == AttachmentKind::Skill => {
            if attachment.truncated && disk_available {
                return disk.map(|path| {
                    (
                        Content::Path(path),
                        disk_len.unwrap_or(attachment.size),
                        false,
                    )
                });
            }
            let bytes = text.as_bytes().to_vec();
            let len = bytes.len() as u64;
            Some((Content::Bytes(bytes), len, attachment.truncated))
        }
        Some(text) if !attachment.truncated => {
            let bytes = text.as_bytes().to_vec();
            let len = bytes.len() as u64;
            Some((Content::Bytes(bytes), len, false))
        }
        _ if disk_available => disk.map(|path| {
            (
                Content::Path(path),
                disk_len.unwrap_or(attachment.size),
                false,
            )
        }),
        Some(text) => {
            // The file vanished; keep the prefix that was captured at attach
            // time and mark it truncated in the manifest.
            let bytes = text.as_bytes().to_vec();
            let len = bytes.len() as u64;
            Some((Content::Bytes(bytes), len, true))
        }
        None => None,
    }
}

fn resolve_attachment_path(attachment: &UserAttachment, roots: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(&attachment.path);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    for root in roots {
        let candidate = root.join(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn looks_like_file_mention(token: &str) -> bool {
    token.contains('/') || token.contains('\\') || Path::new(token).extension().is_some()
}

fn resolve_mention(token: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    if let Some(path) = resolve_mention_exact(token, roots) {
        return Some(path);
    }
    // Sentence punctuation directly after a mention (`@src/main.rs.`) is not
    // part of the path. Only retry when trimming actually changed the token.
    let trimmed = token.trim_end_matches(['.', '!', '?']);
    (trimmed != token)
        .then(|| resolve_mention_exact(trimmed, roots))
        .flatten()
}

fn resolve_mention_exact(token: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(token);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    for root in roots {
        let candidate = root.join(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn display_path(path: &Path, roots: &[PathBuf]) -> String {
    for root in roots {
        if let Ok(relative) = path.strip_prefix(root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    path.display().to_string()
}

fn write_json<T: Serialize + ?Sized>(
    zip: &mut ZipWriter<BufWriter<std::fs::File>>,
    name: &str,
    value: &T,
    options: SimpleFileOptions,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("无法序列化 {name}：{error}"))?;
    write_bytes(zip, name, &bytes, options)
}

fn write_bytes(
    zip: &mut ZipWriter<BufWriter<std::fs::File>>,
    name: &str,
    bytes: &[u8],
    options: SimpleFileOptions,
) -> Result<(), String> {
    zip.start_file(name.to_string(), options)
        .map_err(|error| format!("无法写入 zip 条目 {name}：{error}"))?;
    zip.write_all(bytes)
        .map_err(|error| format!("无法写入 {name}：{error}"))
}

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Idle => "idle",
        RunStatus::Running => "running",
        RunStatus::WaitingApproval => "waiting_approval",
    }
}

fn agent_dir(index: usize, child: &SessionState) -> String {
    let name = child.agent_name.as_deref().unwrap_or("agent");
    let id: String = child.id.chars().take(8).collect();
    format!(
        "agents/{:02}-{}-{}",
        index + 1,
        safe_segment(name),
        safe_segment(&id)
    )
}

fn readme(main: &SessionState, agents: usize, files: usize, missing: usize) -> String {
    format!(
        "# EffiBuddy 会话导出\n\n\
         会话：{title}\n\n\
         导出时间：{time}\n\n\
         ## 内容\n\n\
         - `session.json`：主代理的完整结构化会话（消息、工具调用、计划、用量、关系图、检查点元数据）。\n\
         - `agents/`：{agents} 个子代理会话，每个目录下是同一结构的 `session.json`。\n\
         - `files/`：会话中拖入、`$` 激活技能、以及 `@` 引用的文件，共 {files} 个。\n\
         - `manifest.json`：代理树、文件索引与缺失文件列表。\n\n\
         缺失文件：{missing} 个（详见 `manifest.json`）。\n",
        title = main.display_title(),
        time = Utc::now().to_rfc3339(),
    )
}

fn mention_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        if ch == '@' {
            let previous = text[..index].chars().next_back();
            let boundary = previous.is_none_or(|prev| prev.is_whitespace() || !prev.is_ascii());
            if boundary {
                let start = index + ch.len_utf8();
                let mut end = start;
                for (offset, candidate) in text[start..].char_indices() {
                    if candidate.is_whitespace() || is_token_end(candidate) {
                        break;
                    }
                    end = start + offset + candidate.len_utf8();
                }
                if end > start {
                    let token = text[start..end].trim_end_matches(is_token_end);
                    if !token.is_empty() {
                        tokens.push(token.to_string());
                    }
                }
                index = end;
                continue;
            }
        }
        index += ch.len_utf8();
    }
    tokens
}

fn is_token_end(ch: char) -> bool {
    matches!(
        ch,
        ',' | ';'
            | ':'
            | '"'
            | '\''
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
            | '|'
            | '`'
            | '。'
            | '，'
            | '；'
            | '：'
            | '（'
            | '）'
            | '【'
            | '】'
            | '《'
            | '》'
            | '、'
    )
}

fn safe_relative(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => return None,
            part => parts.push(safe_segment(part)),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn safe_segment(segment: &str) -> String {
    let mut safe = String::new();
    for ch in segment.chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            safe.push('_');
        } else {
            safe.push(ch);
        }
        if safe.chars().count() >= 80 {
            break;
        }
    }
    let safe = safe.trim().trim_matches('.').to_string();
    if safe.is_empty() {
        "file".to_string()
    } else {
        safe
    }
}

fn unique_entry_name(used: &mut HashSet<String>, candidate: String) -> String {
    if used.insert(candidate.clone()) {
        return candidate;
    }
    let path = Path::new(&candidate);
    let parent = path.parent().map(|parent| parent.to_string_lossy().into_owned());
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned());
    for counter in 2..10_000u32 {
        let name = match (&parent, &extension) {
            (Some(parent), Some(extension)) => {
                format!("{parent}/{stem}-{counter}.{extension}")
            }
            (Some(parent), None) => format!("{parent}/{stem}-{counter}"),
            (None, Some(extension)) => format!("{stem}-{counter}.{extension}"),
            (None, None) => format!("{stem}-{counter}"),
        };
        if used.insert(name.clone()) {
            return name;
        }
    }
    format!("{stem}-{}", Utc::now().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::{AttachmentKind, Message, UserAttachment};

    fn attachment(name: &str, path: &str, text: &str) -> UserAttachment {
        UserAttachment {
            name: name.to_string(),
            path: path.to_string(),
            kind: AttachmentKind::File,
            mime_type: "text/plain".to_string(),
            size: text.len() as u64,
            text: Some(text.to_string()),
            image: None,
            truncated: false,
        }
    }

    #[test]
    fn mention_tokens_only_start_at_boundaries_and_stop_at_punctuation() {
        let tokens = mention_tokens("see @src/main.rs, and 看@笔记.md (not@this)");
        assert_eq!(tokens, vec!["src/main.rs", "笔记.md"]);
    }

    #[test]
    fn safe_relative_rejects_traversal() {
        assert_eq!(safe_relative("src/main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(safe_relative("../secret"), None);
        assert_eq!(safe_relative("C:\\Users\\a.txt").as_deref(), Some("C_/Users/a.txt"));
    }

    #[test]
    fn image_attachments_are_written_as_decoded_bytes() {
        use effibuddy_agent_core::ImageAttachment;
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let mut main = SessionState::new(directory.path().to_path_buf(), "test-model");
        let mut message = Message::user("look");
        message.attachments = vec![UserAttachment {
            name: "shot.png".to_string(),
            path: "shot.png".to_string(),
            kind: AttachmentKind::Image,
            mime_type: "image/png".to_string(),
            size: 3,
            text: None,
            image: Some(ImageAttachment {
                mime_type: "image/png".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
                width: 1,
                height: 1,
            }),
            truncated: false,
        }];
        main.push_message(message);

        let destination = directory.path().join("image.zip");
        export(&main, &[], &[], &destination).unwrap();
        let file = std::fs::File::open(&destination).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entry = archive
            .by_name("files/attachments/shot.png")
            .expect("image entry");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
    }

    #[test]
    fn descendants_walk_the_tree_without_reopen_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::with_directory(directory.path());
        let main = SessionState::new(directory.path().to_path_buf(), "test-model");
        let mut child = SessionState::new(directory.path().to_path_buf(), "test-model");
        child.parent_id = Some(main.id.clone());
        child.agent_name = Some("explore".to_string());
        child.updated_at = "2026-11-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let child_id = child.id.clone();
        let mut grandchild = SessionState::new(directory.path().to_path_buf(), "test-model");
        grandchild.parent_id = Some(child.id.clone());
        store.save(&main).unwrap();
        store.save(&child).unwrap();
        store.save(&grandchild).unwrap();

        let agents = descendants(&store, &main.id);
        assert_eq!(agents.len(), 2);
        let restored = agents.iter().find(|agent| agent.id == child_id).unwrap();
        // Raw loading must not rewrite updated_at to the export time.
        assert_eq!(
            restored.updated_at,
            "2026-11-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn export_writes_structured_sessions_and_attachments() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("mentioned.md"), "# mentioned\n").unwrap();

        let mut main = SessionState::new(workspace.clone(), "test-model");
        let mut message = Message::user("please read @mentioned.md");
        message.attachments = vec![attachment("notes.md", "notes.md", "# notes\n")];
        main.push_message(message);

        let mut child = SessionState::new(workspace.clone(), "test-model");
        child.parent_id = Some(main.id.clone());
        child.agent_name = Some("explore".to_string());
        child.push_message(Message::user("look around"));

        let destination = directory.path().join("export.zip");
        let report = export(&main, &[child], &[], &destination).unwrap();
        assert_eq!(report.subagents, 1);
        assert_eq!(report.files, 2);
        assert_eq!(report.missing, 0);

        let file = std::fs::File::open(&destination).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"session.json".to_string()));
        assert!(names.iter().any(|name| name.starts_with("agents/01-explore-")));
        assert!(names.iter().any(|name| name.ends_with("files/attachments/notes.md")));
        assert!(names.iter().any(|name| name.ends_with("files/mentions/mentioned.md")));
        let manifest = archive.by_name("manifest.json").unwrap();
        let manifest: serde_json::Value = serde_json::from_reader(manifest).unwrap();
        assert_eq!(manifest["agents"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["files"].as_array().unwrap().len(), 2);

        let session = archive.by_name("session.json").unwrap();
        let session: serde_json::Value = serde_json::from_reader(session).unwrap();
        assert_eq!(
            session["messages"][0]["attachments"][0]["name"],
            "notes.md"
        );
        {
            let mut attachment = archive
                .by_name("files/attachments/notes.md")
                .expect("attachment entry");
            let mut contents = String::new();
            attachment.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "# notes\n");
        }
        {
            let mut mention = archive
                .by_name("files/mentions/mentioned.md")
                .expect("mention entry");
            let mut contents = String::new();
            mention.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "# mentioned\n");
        }
    }
}
