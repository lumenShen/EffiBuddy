//! Downscaled thumbnails for the rasters that ride along in a transcript.
//!
//! A pasted screenshot is inlined into the message as base64 because the model
//! needs the original pixels — but the transcript only ever shows it as a
//! 184×120 card. Handing GPUI the original made it decode the whole raster and
//! upload it into the sprite atlas: one 2560×1680 screenshot cost a 4096²
//! atlas page plus a 16 MB decoded copy, ~130 MB of the process for a card
//! nobody can read at that size.
//!
//! So each big raster is downscaled once, on a background thread, into
//! `<data dir>/thumbs/<session>/<key>.png` and reused from then on — including
//! across restarts. Like the transcript it belongs to, a thumbnail is deleted
//! when that conversation is deleted; archiving keeps it, because a restored
//! conversation must not re-decode its screenshots.

use std::path::{Path, PathBuf};

use base64::Engine;

use crate::config::base_dir;

/// Longest edge of a generated thumbnail: the transcript shows these in a
/// 184×120 card (tool rasters in 420×280), so 512 covers a 2× display with room
/// for a larger preview later, at ~1 MB decoded instead of 16 MB.
const MAX_THUMB_EDGE: u32 = 512;

fn root() -> PathBuf {
    base_dir().join("thumbs")
}

/// Directories are per conversation, so deleting one is a single `remove_dir_all`.
fn session_dir(root: &Path, session_id: &str) -> PathBuf {
    root.join(sanitize(session_id))
}

/// Session ids are UUIDs; anything else (a hand-edited file, a weird id from an
/// older build) must not escape the directory it is named into.
fn sanitize(session_id: &str) -> String {
    session_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Thumbnail path for one raster in a conversation. `key` is the caller's
/// identity for that raster (attachment path + size, or `call_id:index`);
/// hashing keeps it a valid file name whatever it contains.
fn path_in(root: &Path, session_id: &str, key: &str) -> PathBuf {
    session_dir(root, session_id).join(format!("{:016x}.png", fnv1a(key.as_bytes())))
}

/// Cached thumbnail bytes, if this raster was already downscaled.
pub(crate) fn cached(session_id: &str, key: &str) -> Option<Vec<u8>> {
    cached_in(&root(), session_id, key)
}

fn cached_in(root: &Path, session_id: &str, key: &str) -> Option<Vec<u8>> {
    std::fs::read(path_in(root, session_id, key))
        .ok()
        .filter(|png| !png.is_empty())
}

/// Downscale one encoded raster into a thumbnail PNG, cache it and return the
/// bytes (so the caller can show it without touching the disk again).
///
/// Blocking: decoding a 4 MP screenshot takes tens of milliseconds, so this
/// runs on the background executor, never in a frame.
pub(crate) fn build(session_id: &str, key: &str, base64_data: &str) -> Option<Vec<u8>> {
    build_in(&root(), session_id, key, base64_data)
}

fn build_in(root: &Path, session_id: &str, key: &str, base64_data: &str) -> Option<Vec<u8>> {
    // Another frame may have produced it while this one was queued.
    if let Some(png) = cached_in(root, session_id, key) {
        return Some(png);
    }
    let encoded = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()?;
    let png = downscale(&encoded)?;
    let path = path_in(root, session_id, key);
    if let Some(directory) = path.parent() {
        let _ = std::fs::create_dir_all(directory);
    }
    // Write-then-rename: a half-written file must never be read back as a
    // thumbnail (an empty one would render as nothing forever).
    let temporary = path.with_extension("png.tmp");
    if std::fs::write(&temporary, &png).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
    Some(png)
}

/// PNG bytes of `encoded` scaled down to [`MAX_THUMB_EDGE`]. `None` for formats
/// the image decoder cannot read (SVG), which the caller renders the old way.
fn downscale(encoded: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(encoded).ok()?;
    let thumbnail = image.thumbnail(MAX_THUMB_EDGE, MAX_THUMB_EDGE);
    let mut png = Vec::new();
    thumbnail
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// Drop every thumbnail of a deleted conversation.
pub(crate) fn remove_session(session_id: &str) {
    remove_session_in(&root(), session_id);
}

fn remove_session_in(root: &Path, session_id: &str) {
    let _ = std::fs::remove_dir_all(session_dir(root, session_id));
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3000×2000 raster as a real PNG, base64-encoded the way a transcript
    /// holds it. Decodes to 24 MB, so it is exactly the case worth shrinking.
    fn big_raster_base64() -> (String, usize) {
        let image = image::RgbaImage::from_fn(3000, 2000, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode test raster");
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
        (encoded, png.len())
    }

    /// The whole point: a raster that would decode to 24 MB comes back as a
    /// small thumbnail, and the second call reads it off the disk instead of
    /// decoding again.
    #[test]
    fn build_downscales_caches_and_reuses() {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path();
        let (encoded, original) = big_raster_base64();
        assert!(original > 100_000, "test raster should be a real PNG");

        let thumbnail = build_in(root, "session-1", "attachment:a", &encoded).expect("thumbnail");
        assert!(
            thumbnail.len() * 4 < original,
            "thumbnail ({} B) is not meaningfully smaller than the original ({original} B)",
            thumbnail.len()
        );
        let decoded = image::load_from_memory(&thumbnail).expect("thumbnail decodes");
        assert_eq!(decoded.width(), MAX_THUMB_EDGE);
        assert!(
            (338..=344).contains(&decoded.height()),
            "aspect ratio is not preserved: {}×{}",
            decoded.width(),
            decoded.height()
        );

        // Second call is a cache hit: no payload needed at all.
        let cached = build_in(root, "session-1", "attachment:a", "").expect("cached thumbnail");
        assert_eq!(cached, thumbnail);
        assert!(cached_in(root, "session-1", "attachment:a").is_some());
    }

    /// A thumbnail belongs to one conversation: the same raster in a different
    /// conversation gets its own file, and deleting one keeps the other.
    #[test]
    fn thumbnails_are_per_conversation_and_deleted_with_it() {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path();
        let (encoded, _) = big_raster_base64();
        build_in(root, "session-a", "key", &encoded).expect("a");
        build_in(root, "session-b", "key", &encoded).expect("b");

        remove_session_in(root, "session-a");
        assert!(cached_in(root, "session-a", "key").is_none());
        assert!(
            cached_in(root, "session-b", "key").is_some(),
            "deleting one conversation must not touch another's thumbnails"
        );
    }

    /// A raster the decoder cannot read must fail instead of leaving an empty
    /// file behind that would render as a blank image forever.
    #[test]
    fn undecodable_rasters_leave_no_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path();
        let nonsense = base64::engine::general_purpose::STANDARD.encode(b"<svg/>");
        assert!(build_in(root, "session", "key", &nonsense).is_none());
        assert!(cached_in(root, "session", "key").is_none());
    }

    /// Ids become directory names, so nothing may traverse out of the cache.
    #[test]
    fn ids_cannot_escape_the_thumbnail_directory() {
        // Separators are gone, and so are dots: a bare `..` id would otherwise
        // name the parent directory.
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize(".."), "__");
        assert_eq!(sanitize("4db34ff6-ca27"), "4db34ff6-ca27");
    }
}
