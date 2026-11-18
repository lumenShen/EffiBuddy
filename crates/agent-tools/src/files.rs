//! File and directory tools.

use std::path::Path;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};

use effibuddy_agent_core::{
    ApprovalPolicy, ImageAttachment, Tool, ToolContext, ToolError, ToolOutput,
};

use crate::args::{optional_bool, optional_u64, required_string};
use crate::workspace::Workspace;

const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
const MAX_READ_OUTPUT_CHARS: usize = 60_000;
const MAX_WRITE_CHARS: usize = 1_000_000;
const MAX_LIST_ENTRIES: usize = 500;
/// Matches the composer's image ceiling: a raster bigger than this would bloat
/// the transcript and every subsequent request without adding detail.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone)]
pub struct ReadFileTool {
    workspace: Workspace,
    /// Whether the selected model can actually see images. Hosts enable it from
    /// the model's capability flag; when it is off the tool still describes an
    /// image file (dimensions, format) instead of failing as "binary".
    vision: bool,
}

impl ReadFileTool {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            vision: false,
        }
    }

    /// Let `read_file` return image files as model-visible image parts.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    /// Turn a recognized image file into either an image part or a description.
    fn image_output(
        &self,
        path: &Path,
        bytes: &[u8],
        header: ImageHeader,
    ) -> Result<ToolOutput, ToolError> {
        let display = self.workspace.display_path(path);
        let size = bytes.len() as u64;
        let dimensions = if header.width > 0 && header.height > 0 {
            format!("{}x{}", header.width, header.height)
        } else {
            "unknown dimensions".to_string()
        };

        if !self.vision {
            return Ok(ToolOutput::text(format!(
                "{display} is a {} image ({dimensions}, {size} bytes). The selected model cannot \
                 see images, so no image part was attached; inspect it with `shell` (for example \
                 ImageMagick or a short Python script) and switch to a vision-capable model to \
                 look at the pixels.",
                header.mime
            )));
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Ok(ToolOutput::text(format!(
            "{display} ({}, {dimensions}, {size} bytes) — attached to the session as an image \
             part you can see; call `read_file` on the same path to look at it again later.",
            header.mime
        ))
        .with_image(ImageAttachment {
            mime_type: header.mime.to_string(),
            data: encoded,
            width: header.width,
            height: header.height,
        }))
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        if self.vision {
            "Read a file from the workspace. Text files come back as numbered lines; use \
             `offset` (1-based line) and `limit` for large files. PNG/JPEG/GIF/WebP/BMP images \
             are attached to the session as image parts so you can look at them."
        } else {
            "Read a file from the workspace. Text files come back as numbered lines; use \
             `offset` (1-based line) and `limit` for large files. Image files \
             (PNG/JPEG/GIF/WebP/BMP) are described by format, dimensions and size."
        }
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace root."},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to read (1-based). Text files only."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Maximum number of lines. Defaults to 400. Text files only."}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path = required_string(&arguments, "path")?;
        let path = self.workspace.resolve_existing(&raw_path)?;

        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| ToolError::new(format!("cannot stat `{raw_path}`: {error}")))?;
        if metadata.is_dir() {
            return Err(ToolError::new(format!(
                "`{raw_path}` is a directory; use list_dir instead"
            )));
        }
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(ToolError::new(format!(
                "`{raw_path}` is {} bytes; read a smaller file (limit {MAX_IMAGE_BYTES} bytes)",
                metadata.len()
            )));
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| ToolError::new(format!("cannot read `{raw_path}`: {error}")))?;
        if ctx.cancelled() {
            return Err(ToolError::new("cancelled"));
        }
        // A recognized raster is handed to the model as an image part instead
        // of being mangled by the text path (or rejected as "binary").
        if let Some(header) = sniff_image(&bytes) {
            return self.image_output(&path, &bytes, header);
        }

        if metadata.len() > MAX_READ_BYTES {
            return Err(ToolError::new(format!(
                "`{raw_path}` is {} bytes; read a smaller file or use offset/limit",
                metadata.len()
            )));
        }
        if bytes.contains(&0) {
            return Err(ToolError::new(format!(
                "`{raw_path}` looks like a binary file; `read_file` only returns text and \
                 PNG/JPEG/GIF/WebP/BMP images"
            )));
        }
        let text = String::from_utf8_lossy(&bytes);

        let offset = optional_u64(&arguments, "offset")?.unwrap_or(1).max(1) as usize;
        let limit = optional_u64(&arguments, "limit")?
            .unwrap_or(400)
            .clamp(1, 2000) as usize;

        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len();
        if offset > total_lines && total_lines > 0 {
            return Err(ToolError::new(format!(
                "offset {offset} is past the end of `{raw_path}` ({total_lines} lines)"
            )));
        }

        let start = (offset - 1).min(total_lines);
        let end = (start + limit).min(total_lines);
        let mut output = format!(
            "{} ({} lines{})\n",
            self.workspace.display_path(&path),
            total_lines,
            if metadata.len() > 0 {
                format!(", {} bytes", metadata.len())
            } else {
                String::new()
            }
        );
        for (index, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>5} | {}\n", start + index + 1, line));
        }
        if end < total_lines {
            output.push_str(&format!(
                "\n[{} more lines; continue with offset={}]",
                total_lines - end,
                end + 1
            ));
        }
        if output.chars().count() > MAX_READ_OUTPUT_CHARS {
            output = output.chars().take(MAX_READ_OUTPUT_CHARS).collect();
            output.push_str("\n[output truncated]");
        }
        if ctx.cancelled() {
            return Err(ToolError::new("cancelled"));
        }
        Ok(ToolOutput::text(output))
    }
}

/// What a raster file says about itself, read from its header bytes.
struct ImageHeader {
    mime: &'static str,
    width: u32,
    height: u32,
}

/// Identify a raster image by its magic bytes and pull the pixel dimensions out
/// of the header.
///
/// No pixel data is decoded: `read_file` only needs the format (for the
/// provider) and a size (for the vision-token estimate and the UI), and a
/// header read keeps a 5 MB photo from being expanded in memory. A malformed
/// header still counts as an image — the dimensions fall back to 0.
fn sniff_image(bytes: &[u8]) -> Option<ImageHeader> {
    let (mime, dimensions) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("image/png", png_dimensions(bytes))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ("image/jpeg", jpeg_dimensions(bytes))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        ("image/gif", gif_dimensions(bytes))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        ("image/webp", webp_dimensions(bytes))
    } else if bytes.starts_with(b"BM") {
        ("image/bmp", bmp_dimensions(bytes))
    } else {
        return None;
    };
    let (width, height) = dimensions.unwrap_or((0, 0));
    Some(ImageHeader {
        mime,
        width,
        height,
    })
}

fn be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn be_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(..2)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn le_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(..2)?.try_into().ok()?))
}

/// PNG: the `IHDR` chunk always follows the 8-byte signature and its 4-byte
/// length, so width/height are at fixed offsets.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be_u32(bytes.get(16..20)?)?, be_u32(bytes.get(20..24)?)?))
}

/// GIF: the logical screen descriptor sits directly after the 6-byte signature.
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    Some((
        le_u16(bytes.get(6..8)?)? as u32,
        le_u16(bytes.get(8..10)?)? as u32,
    ))
}

/// BMP: `BITMAPINFOHEADER` keeps signed width/height at offsets 18 and 22; a
/// negative height means a top-down bitmap, so only the magnitude matters.
fn bmp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let width = i32::from_le_bytes(bytes.get(18..22)?.try_into().ok()?);
    let height = i32::from_le_bytes(bytes.get(22..26)?.try_into().ok()?);
    Some((width.unsigned_abs(), height.unsigned_abs()))
}

/// WebP stores dimensions differently per variant: `VP8X` (extended) keeps
/// canvas-minus-one as two 24-bit values, `VP8 ` (lossy) puts 14-bit values
/// after the start code, and `VP8L` (lossless) packs both into 28 bits.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let variant = bytes.get(12..16)?;
    match variant {
        b"VP8X" => {
            let le24 = |start: usize| {
                let chunk = bytes.get(start..start + 3)?;
                Some(1 + u32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]))
            };
            Some((le24(24)?, le24(27)?))
        }
        b"VP8 " => {
            // Frame tag (3 bytes) + the 9D 01 2A start code, then 14-bit sizes.
            if !bytes.get(23..26)?.starts_with(&[0x9D, 0x01, 0x2A]) {
                return None;
            }
            let width = (le_u16(bytes.get(26..28)?)? & 0x3FFF) as u32;
            let height = (le_u16(bytes.get(28..30)?)? & 0x3FFF) as u32;
            Some((width, height))
        }
        b"VP8L" => {
            if bytes.get(20).copied()? != 0x2F {
                return None;
            }
            let bits = le_u32(bytes.get(21..25)?)?;
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            Some((width, height))
        }
        _ => None,
    }
}

/// JPEG: walk the marker segments until a start-of-frame marker, whose payload
/// carries precision, height and width.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2; // past the SOI marker
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xFF {
            return None;
        }
        let marker = bytes[index + 1];
        // Fill bytes and marker-only segments carry no length field.
        if marker == 0xFF {
            index += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            index += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        let length = be_u16(bytes.get(index + 2..index + 4)?)? as usize;
        if length < 2 {
            return None;
        }
        let is_start_of_frame =
            (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_start_of_frame {
            let height = be_u16(bytes.get(index + 5..index + 7)?)? as u32;
            let width = be_u16(bytes.get(index + 7..index + 9)?)? as u32;
            return Some((width, height));
        }
        index += 2 + length;
    }
    None
}

#[derive(Clone)]
pub struct WriteFileTool {
    workspace: Workspace,
}

impl WriteFileTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a UTF-8 text file. Prefer `edit_file` for small \
         changes to existing files. Requires user approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace root."},
                "content": {"type": "string", "description": "Full file contents."}
            },
            "required": ["path", "content"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Ask
    }

    fn summarize(&self, arguments: &Value) -> String {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("(missing path)");
        let bytes = arguments
            .get("content")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        format!("Write {bytes} bytes to `{path}`")
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path = required_string(&arguments, "path")?;
        let content = required_string(&arguments, "content")?;
        if content.len() > MAX_WRITE_CHARS {
            return Err(ToolError::new(format!(
                "content is too large ({} characters, limit {MAX_WRITE_CHARS})",
                content.len()
            )));
        }
        let path = self.workspace.resolve_for_write(&raw_path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                ToolError::new(format!(
                    "cannot create parent directory `{}`: {error}",
                    parent.display()
                ))
            })?;
        }
        let relative = self.workspace.display_path(&path);
        let checkpoint = ctx.checkpoint_sink.clone();
        if let Some(sink) = &checkpoint {
            sink.before_write(&relative, &path, self.name());
        }
        if let Err(error) = tokio::fs::write(&path, content.as_bytes()).await {
            if let Some(sink) = &checkpoint {
                sink.write_failed(&relative, &path, self.name());
            }
            return Err(ToolError::new(format!(
                "cannot write `{raw_path}`: {error}"
            )));
        }
        if let Some(sink) = &checkpoint {
            sink.after_write(&relative, &path, self.name(), content.as_bytes());
        }
        Ok(ToolOutput::text(format!(
            "Wrote {} bytes to `{}`.",
            content.len(),
            self.workspace.display_path(&path)
        )))
    }
}

#[derive(Clone)]
pub struct EditFileTool {
    workspace: Workspace,
}

impl EditFileTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. Fails if `old_string` is missing \
         or appears more than once unless `replace_all` is true. Requires user approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace, including whitespace."},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence. Defaults to false."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Ask
    }

    fn summarize(&self, arguments: &Value) -> String {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("(missing path)");
        format!("Edit `{path}` in place")
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path = required_string(&arguments, "path")?;
        let old_string = required_string(&arguments, "old_string")?;
        let new_string = required_string(&arguments, "new_string")?;
        let replace_all = optional_bool(&arguments, "replace_all")?.unwrap_or(false);
        if old_string.is_empty() {
            return Err(ToolError::new("`old_string` must not be empty"));
        }

        let path = self.workspace.resolve_existing(&raw_path)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| ToolError::new(format!("cannot read `{raw_path}`: {error}")))?;

        let occurrences = content.matches(&old_string).count();
        if occurrences == 0 {
            return Err(ToolError::new(format!(
                "`old_string` was not found in `{raw_path}`. Read the file and copy the exact text."
            )));
        }
        if occurrences > 1 && !replace_all {
            return Err(ToolError::new(format!(
                "`old_string` appears {occurrences} times in `{raw_path}`; add surrounding context or set `replace_all`"
            )));
        }

        let updated = if replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };
        let relative = self.workspace.display_path(&path);
        let checkpoint = ctx.checkpoint_sink.clone();
        if let Some(sink) = &checkpoint {
            sink.before_write(&relative, &path, self.name());
        }
        if let Err(error) = tokio::fs::write(&path, updated.as_bytes()).await {
            if let Some(sink) = &checkpoint {
                sink.write_failed(&relative, &path, self.name());
            }
            return Err(ToolError::new(format!(
                "cannot write `{raw_path}`: {error}"
            )));
        }
        if let Some(sink) = &checkpoint {
            sink.after_write(&relative, &path, self.name(), updated.as_bytes());
        }

        Ok(ToolOutput::text(format!(
            "Replaced {} occurrence{} in `{}`.",
            if replace_all { occurrences } else { 1 },
            if replace_all && occurrences != 1 {
                "s"
            } else {
                ""
            },
            self.workspace.display_path(&path)
        )))
    }
}

#[derive(Clone)]
pub struct ListDirTool {
    workspace: Workspace,
}

impl ListDirTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List files and directories under a workspace path as JSON. \
         Skips common build/VCS directories and hidden entries by default."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path. Defaults to the workspace root."},
                "depth": {"type": "integer", "minimum": 1, "maximum": 3, "description": "Recursion depth. Defaults to 1."},
                "include_hidden": {"type": "boolean", "description": "Include dotfiles and ignored directories."}
            }
        })
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let raw_path =
            crate::args::optional_string(&arguments, "path")?.unwrap_or_else(|| ".".to_string());
        let root = self.workspace.resolve_existing(&raw_path)?;
        if !root.is_dir() {
            return Err(ToolError::new(format!("`{raw_path}` is not a directory")));
        }
        let depth = optional_u64(&arguments, "depth")?.unwrap_or(1).clamp(1, 3) as usize;
        let include_hidden = optional_bool(&arguments, "include_hidden")?.unwrap_or(false);

        let mut entries = Vec::new();
        let mut truncated = false;
        collect_entries(
            &self.workspace,
            &root,
            depth,
            include_hidden,
            &mut entries,
            &mut truncated,
            ctx.cancel.clone(),
        )?;

        let payload = json!({
            "path": self.workspace.display_path(&root),
            "entries": entries,
            "truncated": truncated,
        });
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        ))
    }
}

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".turbo",
    "__pycache__",
    ".venv",
    "venv",
];

pub(crate) fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name)
}

fn collect_entries(
    workspace: &Workspace,
    directory: &Path,
    depth: usize,
    include_hidden: bool,
    entries: &mut Vec<Value>,
    truncated: &mut bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError::new("cancelled"));
    }
    if *truncated {
        return Ok(());
    }
    let reader = std::fs::read_dir(directory).map_err(|error| {
        ToolError::new(format!(
            "cannot read directory `{}`: {error}",
            directory.display()
        ))
    })?;

    let mut items: Vec<_> = reader.filter_map(Result::ok).collect();
    items.sort_by_key(|entry| {
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        (std::cmp::Reverse(is_dir), entry.file_name())
    });

    for entry in items {
        if entries.len() >= MAX_LIST_ENTRIES {
            *truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let symlink_metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let is_symlink = symlink_metadata.file_type().is_symlink();
        let is_dir = symlink_metadata.is_dir() && !is_symlink;
        if is_dir && IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let size = if symlink_metadata.is_file() {
            Some(symlink_metadata.len())
        } else {
            None
        };
        entries.push(json!({
            "name": name,
            "path": workspace.display_path(&path),
            "kind": if is_symlink { "symlink" } else if is_dir { "dir" } else { "file" },
            "size": size,
        }));
        if is_dir && depth > 1 {
            collect_entries(
                workspace,
                &path,
                depth - 1,
                include_hidden,
                entries,
                truncated,
                cancel.clone(),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header-only PNG: signature, `IHDR` length, `IHDR`, width, height.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    fn dimensions(bytes: &[u8]) -> (&'static str, u32, u32) {
        let header = sniff_image(bytes).expect("recognized image");
        (header.mime, header.width, header.height)
    }

    #[test]
    fn sniffs_png_dimensions() {
        assert_eq!(
            dimensions(&png_header(1920, 1080)),
            ("image/png", 1920, 1080)
        );
    }

    #[test]
    fn sniffs_gif_dimensions() {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&320u16.to_le_bytes());
        bytes.extend_from_slice(&200u16.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        assert_eq!(dimensions(&bytes), ("image/gif", 320, 200));
    }

    #[test]
    fn sniffs_bmp_dimensions() {
        let mut bytes = b"BM".to_vec();
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&640i32.to_le_bytes());
        bytes.extend_from_slice(&(-480i32).to_le_bytes());
        assert_eq!(dimensions(&bytes), ("image/bmp", 640, 480));
    }

    #[test]
    fn sniffs_jpeg_dimensions_past_the_app_segment() {
        let mut bytes = vec![0xFF, 0xD8];
        // APP0 with a 4-byte segment length (2 payload bytes).
        bytes.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        // SOF0: length, precision, height, width.
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&2u16.to_be_bytes());
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.extend_from_slice(&[0; 6]);
        assert_eq!(dimensions(&bytes), ("image/jpeg", 3, 2));
    }

    #[test]
    fn sniffs_webp_variants() {
        let mut vp8x = b"RIFF".to_vec();
        vp8x.extend_from_slice(&0u32.to_le_bytes());
        vp8x.extend_from_slice(b"WEBPVP8X");
        vp8x.extend_from_slice(&0u32.to_le_bytes());
        vp8x.extend_from_slice(&[0; 4]);
        vp8x.extend_from_slice(&[0x7F, 0x07, 0x00]); // 1920 - 1
        vp8x.extend_from_slice(&[0x37, 0x04, 0x00]); // 1080 - 1        assert_eq!(dimensions(&vp8x), ("image/webp", 1920, 1080));

        let mut vp8l = b"RIFF".to_vec();
        vp8l.extend_from_slice(&0u32.to_le_bytes());
        vp8l.extend_from_slice(b"WEBPVP8L");
        vp8l.extend_from_slice(&0u32.to_le_bytes());
        vp8l.push(0x2F);
        // width-1 in bits 0..14, height-1 in bits 14..28.
        let packed = (99u32 - 1) | ((49u32 - 1) << 14);
        vp8l.extend_from_slice(&packed.to_le_bytes());
        assert_eq!(dimensions(&vp8l), ("image/webp", 99, 49));
    }

    #[test]
    fn unknown_bytes_are_not_images() {
        assert!(sniff_image(b"fn main() {}\n").is_none());
        assert!(sniff_image(&[0x00, 0x01, 0x02, 0x03]).is_none());
    }

    #[test]
    fn truncated_image_headers_keep_the_format() {
        // The signature is enough to route the file to the image path even when
        // the header is cut short; dimensions simply stay unknown.
        let header = sniff_image(b"\x89PNG\r\n\x1a\n").expect("signature is enough");
        assert_eq!(header.mime, "image/png");
        assert_eq!((header.width, header.height), (0, 0));
    }
}
