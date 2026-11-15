//! Composer attachments: turn a dropped/picked path into a model-visible
//! [`UserAttachment`].
//!
//! Text-like files are inlined (bounded), images become provider image parts
//! (bounded, base64), and everything else is referenced by path and size so the
//! agent can inspect it with its own tools. Images from outside the workspace
//! also get a durable copy under `<workspace>/.effibuddy/attachments`, so a
//! later step can look at the pixels again or process them (resize/OCR/…)
//! without depending on the original location. Conversion is synchronous: the
//! composer only accepts small files, and a 128 KiB read is far cheaper than an
//! async round-trip that would complicate the send path.

use std::path::{Path, PathBuf};

use base64::Engine;
use effibuddy_agent_core::{AttachmentKind, ImageAttachment, UserAttachment};

use crate::skills::{self, Skill};

/// ZCode uses 8 for the same row; more chips than that stop being scannable.
pub(crate) const MAX_ATTACHMENTS: usize = 8;
const MAX_TEXT_BYTES: u64 = 128 * 1024;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// Workspace-internal folder for durable image copies. Hidden on purpose: it is
/// app state, not a project folder (and the `@` index skips it).
const ATTACHMENT_DIR: &str = ".effibuddy/attachments";

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Convert one filesystem path into an attachment. Directories and unreadable
/// files return an explanation for the composer's notice line.
pub(crate) fn attachment_from_path(
    path: &Path,
    workspace: &Path,
) -> Result<UserAttachment, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    if metadata.is_dir() {
        return Err(format!("{} 是文件夹，请拖入具体文件。", path.display()));
    }
    if !metadata.is_file() {
        return Err(format!("{} 不是普通文件。", path.display()));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let display_path = display_path(path, workspace);
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mime_type = mime_for_extension(&extension).to_string();

    if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "图片 {} 超过 {}，未添加。",
                name,
                format_bytes(MAX_IMAGE_BYTES)
            ));
        }
        let bytes =
            std::fs::read(path).map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        return Ok(UserAttachment {
            name: name.clone(),
            // The model-facing path is the durable workspace copy when one
            // could be written; the pixels also ride along as an image part.
            path: durable_image_path(path, workspace, &name, &bytes),
            kind: AttachmentKind::Image,
            mime_type: mime_type.clone(),
            size: metadata.len(),
            text: None,
            image: Some(ImageAttachment {
                mime_type,
                data,
                width: 0,
                height: 0,
            }),
            truncated: false,
        });
    }

    let (bytes, truncated) = read_prefix(path, MAX_TEXT_BYTES)?;
    let text = if looks_like_text(&bytes) {
        let mut end = bytes.len();
        while end > 0 && !is_boundary(&bytes, end) {
            end -= 1;
        }
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    } else {
        None
    };
    Ok(UserAttachment {
        name,
        path: display_path,
        kind: AttachmentKind::File,
        mime_type,
        size: metadata.len(),
        text,
        image: None,
        truncated,
    })
}

/// Path the model sees for an attached image. Images already inside the
/// workspace are referenced in place; anything else gets a content-addressed
/// copy under `.effibuddy/attachments` (a copy failure falls back to the
/// original path, which the shell tool can still reach).
fn durable_image_path(original: &Path, workspace: &Path, name: &str, bytes: &[u8]) -> String {
    if is_inside_workspace(original, workspace) {
        return display_path(original, workspace);
    }
    match write_image_copy(workspace, name, bytes) {
        Ok(path) => display_path(&path, workspace),
        Err(_) => display_path(original, workspace),
    }
}

fn is_inside_workspace(path: &Path, workspace: &Path) -> bool {
    let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(workspace)
}

/// Write (or reuse) a copy named `<content-hash>-<original name>`. The hash
/// makes repeated drops of the same image dedupe onto one file.
fn write_image_copy(workspace: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let directory = workspace.join(ATTACHMENT_DIR);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("无法创建 {}：{error}", directory.display()))?;
    // Copies are runtime artifacts. Ignore them locally without hiding the
    // user's own `.effibuddy/skills` / `.effibuddy/agents` from Git.
    let gitignore = directory.join(".gitignore");
    if !gitignore.is_file() {
        let _ = std::fs::write(&gitignore, "*\n!.gitignore\n");
    }
    let target = directory.join(format!(
        "{:016x}-{}",
        fnv1a(bytes),
        sanitize_file_name(name)
    ));
    if target.is_file() {
        return Ok(target);
    }
    let temporary = directory.join(format!(".{:016x}.tmp", fnv1a(bytes)));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("无法保存图片副本 {}：{error}", temporary.display()))?;
    std::fs::rename(&temporary, &target).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("无法保存图片副本 {}：{error}", target.display())
    })?;
    Ok(target)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn sanitize_file_name(name: &str) -> String {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut out = sanitize_component(&stem, 64);
    if !extension.is_empty() && extension.chars().all(|ch| ch.is_alphanumeric()) {
        out.push('.');
        out.extend(extension.chars().take(8));
    }
    if out.is_empty() {
        "image".to_string()
    } else {
        out
    }
}

fn sanitize_component(value: &str, max: usize) -> String {
    let mut out = String::with_capacity(value.len().min(max * 2));
    let mut last_dash = false;
    for ch in value.chars() {
        if out.chars().count() >= max {
            break;
        }
        if ch.is_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Read an activated skill as an attachment. The body is what the model needs;
/// the path stays for the UI chip and for follow-up reads.
pub(crate) fn skill_attachment(skill: &Skill, workspace: &Path) -> Result<UserAttachment, String> {
    let (body, truncated, size) = skills::read_skill_body(skill)?;
    Ok(UserAttachment {
        name: skill.name.clone(),
        path: skill.display_path(workspace),
        kind: AttachmentKind::Skill,
        mime_type: "text/markdown".to_string(),
        size,
        text: Some(body),
        image: None,
        truncated,
    })
}

pub(crate) fn format_bytes(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let size = size as f64;
    if size >= MB {
        format!("{:.1} MB", size / MB)
    } else if size >= KB {
        format!("{:.1} KB", size / KB)
    } else {
        format!("{size:.0} B")
    }
}

pub(crate) fn kind_label(kind: AttachmentKind) -> &'static str {
    match kind {
        AttachmentKind::File => "文件",
        AttachmentKind::Image => "图片",
        AttachmentKind::Skill => "技能",
    }
}

pub(crate) fn chip_title(attachment: &UserAttachment) -> String {
    if attachment.kind == AttachmentKind::Skill {
        return format!("${}", attachment.name);
    }
    attachment.name.clone()
}

pub(crate) fn display_path(path: &Path, workspace: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(relative) => {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.is_empty() {
                ".".to_string()
            } else {
                relative
            }
        }
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

fn read_prefix(path: &Path, limit: u64) -> Result<(Vec<u8>, bool), String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    let truncated = bytes.len() as u64 > limit;
    bytes.truncate(limit as usize);
    Ok((bytes, truncated))
}

fn is_boundary(bytes: &[u8], index: usize) -> bool {
    index >= bytes.len() || (bytes[index] & 0xC0) != 0x80
}

/// Cheap text sniff: NUL bytes mean binary; otherwise the prefix must decode as
/// UTF-8. Latin-1 logs get attached as binary on purpose — the agent can still
/// inspect them by path.
fn looks_like_text(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        return false;
    }
    match std::str::from_utf8(probe) {
        Ok(_) => true,
        // Only an incomplete trailing sequence is acceptable (the probe cut a
        // multi-byte character in half). Any actual invalid byte means binary.
        Err(error) => error.error_len().is_none(),
    }
}

fn mime_for_extension(extension: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "rs" => "text/rust",
        "md" | "markdown" => "text/markdown",
        "json" | "jsonc" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "jsx" => "text/javascript",
        "py" => "text/x-python",
        "sh" | "bash" | "zsh" => "text/x-shellscript",
        "c" | "h" => "text/x-c",
        "cc" | "cpp" | "hpp" => "text/x-c++",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "txt" | "log" | "csv" | "tsv" | "sql" | "ini" | "cfg" | "conf" => "text/plain",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Relative paths cross into the UI as `/`-separated strings.
pub(crate) fn normalize_relative(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_files_are_inlined_and_binary_files_are_referenced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let text = root.join("notes.md");
        std::fs::write(&text, "# hi\n").unwrap();
        let attachment = attachment_from_path(&text, root).unwrap();
        assert_eq!(attachment.kind, AttachmentKind::File);
        assert_eq!(attachment.path, "notes.md");
        assert_eq!(attachment.text.as_deref(), Some("# hi\n"));

        let binary = root.join("blob.bin");
        std::fs::write(&binary, [0u8, 159, 146, 150]).unwrap();
        let attachment = attachment_from_path(&binary, root).unwrap();
        assert_eq!(attachment.kind, AttachmentKind::File);
        assert!(attachment.text.is_none());
        assert_eq!(attachment.size, 4);
    }

    #[test]
    fn images_become_base64_parts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let image = root.join("shot.png");
        std::fs::write(&image, [137u8, 80, 78, 71]).unwrap();
        let attachment = attachment_from_path(&image, root).unwrap();
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.mime_type, "image/png");
        // Already in the workspace: referenced in place, no duplicate copy.
        assert_eq!(attachment.path, "shot.png");
        let image = attachment.image.unwrap();
        assert_eq!(image.data, "iVBORw==");
    }

    #[test]
    fn external_images_get_a_durable_workspace_copy() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let source = dir.path().join("Pictures").join("shot.png");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let bytes = [137u8, 80, 78, 71, 9, 9];
        std::fs::write(&source, bytes).unwrap();

        let attachment = attachment_from_path(&source, &workspace).unwrap();
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert!(
            attachment.path.starts_with(".effibuddy/attachments/"),
            "unexpected path: {}",
            attachment.path
        );
        let copy = workspace.join(&attachment.path);
        assert_eq!(std::fs::read(&copy).unwrap(), bytes);
        assert!(
            copy.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-shot.png")
        );

        // A second drop of the same bytes reuses the same copy.
        let again = attachment_from_path(&source, &workspace).unwrap();
        assert_eq!(again.path, attachment.path);
        let copies = std::fs::read_dir(workspace.join(ATTACHMENT_DIR))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name() != ".gitignore")
            .count();
        assert_eq!(copies, 1);
        // Runtime copies never pollute the project's Git status.
        assert!(workspace.join(ATTACHMENT_DIR).join(".gitignore").is_file());
    }

    #[test]
    fn sanitized_copy_names_stay_readable() {
        assert_eq!(
            sanitize_file_name("screen shot (1).png"),
            "screen-shot-1.png"
        );
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("截图 2026.png"), "截图-2026.png");
        assert_eq!(sanitize_file_name(""), "image");
    }

    #[test]
    fn directories_and_oversized_text_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(attachment_from_path(root, root).is_err());

        let big = root.join("big.txt");
        std::fs::write(&big, vec![b'a'; MAX_TEXT_BYTES as usize + 32]).unwrap();
        let attachment = attachment_from_path(&big, root).unwrap();
        assert!(attachment.truncated);
        assert_eq!(
            attachment.text.as_ref().unwrap().len(),
            MAX_TEXT_BYTES as usize
        );
    }

    #[test]
    fn formats_sizes_and_labels() {
        assert_eq!(format_bytes(900), "900 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(kind_label(AttachmentKind::Skill), "技能");
        assert_eq!(normalize_relative(Path::new("a/b")), "a/b");
    }
}
