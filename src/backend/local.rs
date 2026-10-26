//! Local filesystem listing, shaped like a remote listing.

use std::{fs, io, path::Path};

use super::{Entry, EntryKind};
use crate::util::format_mode;

/// List `dir`. Entries with non-UTF-8 names are skipped (they can't be
/// represented safely in the UI or sent over the wire).
pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        let Ok(link_meta) = item.metadata() else {
            continue;
        };
        let is_link = link_meta.file_type().is_symlink();
        // Follow symlinks for type/size, fall back to the link itself if broken.
        let meta = if is_link {
            fs::metadata(item.path()).unwrap_or(link_meta)
        } else {
            link_meta
        };
        let kind = if meta.is_dir() {
            EntryKind::Dir
        } else if is_link {
            EntryKind::Symlink
        } else if meta.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(format_mode(
                meta.permissions().mode(),
                meta.is_dir(),
                is_link,
            ))
        };
        #[cfg(not(unix))]
        let mode = {
            let _ = format_mode;
            None
        };
        out.push(Entry {
            name,
            kind,
            is_link,
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            mode,
        });
    }
    Ok(out)
}
