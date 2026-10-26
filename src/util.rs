//! Small pure helpers: formatting, path handling and name validation.

use std::time::SystemTime;

use chrono::{DateTime, Local};

/// Human readable size using binary units (`1.5 MiB`).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", format_size(bytes_per_sec.max(0.0) as u64))
}

pub fn format_time(t: SystemTime) -> String {
    DateTime::<Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Unix mode bits as an `ls -l` style string, e.g. `drwxr-xr-x`.
pub fn format_mode(mode: u32, is_dir: bool, is_link: bool) -> String {
    let kind = if is_link {
        'l'
    } else if is_dir {
        'd'
    } else {
        '-'
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Replace control characters so a hostile file name can't inject terminal
/// escape sequences when drawn.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// Like [`sanitize`] but keeps newlines, for multi-line server messages.
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' => '\n',
            '\t' => ' ',
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect()
}

/// Validate a single path component that is about to be joined onto a local or
/// remote directory. Rejects separators, traversal and control characters.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("name cannot be empty");
    }
    if name == "." || name == ".." {
        return Err("name cannot be '.' or '..'");
    }
    if name.contains('/') || name.contains('\\') {
        return Err("name cannot contain path separators");
    }
    if name.chars().any(char::is_control) {
        return Err("name cannot contain control characters");
    }
    Ok(())
}

/// Whether a name from a server listing can be shown at all. Names that are
/// merely ugly (control characters) are listed, sanitised for display, and
/// rejected later by [`validate_name`] if the user tries to transfer them.
pub fn is_listable(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// Join a POSIX style remote directory and a file name.
/// Abbreviate the home directory as `~` for display.
pub fn tilde(path: &std::path::Path) -> String {
    if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

pub fn remote_join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent of a POSIX style remote path. The parent of `/` is `/`.
pub fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// Final component of a POSIX style remote path.
pub fn remote_basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert!(format_size(u64::MAX).ends_with("TiB"));
    }

    #[test]
    fn modes() {
        assert_eq!(format_mode(0o755, true, false), "drwxr-xr-x");
        assert_eq!(format_mode(0o644, false, false), "-rw-r--r--");
        assert_eq!(format_mode(0o777, false, true), "lrwxrwxrwx");
    }

    #[test]
    fn names() {
        assert!(validate_name("file.txt").is_ok());
        assert!(validate_name("..").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("a\u{1b}[2J").is_err());
        assert!(validate_name("..hidden").is_ok());
    }

    #[test]
    fn listable() {
        assert!(is_listable("a b"));
        assert!(is_listable("evil\u{1b}[2J"));
        assert!(!is_listable(""));
        assert!(!is_listable(".."));
        assert!(!is_listable("a/b"));
    }

    #[test]
    fn sanitizes_controls() {
        assert_eq!(sanitize("a\u{1b}[31mb"), "a\u{fffd}[31mb");
        assert_eq!(sanitize("plain"), "plain");
    }

    #[test]
    fn sanitize_text_keeps_newlines_only() {
        assert_eq!(sanitize_text("a\nb\tc\u{1b}d"), "a\nb c\u{fffd}d");
    }

    #[test]
    fn remote_paths() {
        assert_eq!(remote_join("/", "a"), "/a");
        assert_eq!(remote_join("/a", "b"), "/a/b");
        assert_eq!(remote_join("/a/", "b"), "/a/b");
        assert_eq!(remote_parent("/a/b"), "/a");
        assert_eq!(remote_parent("/a"), "/");
        assert_eq!(remote_parent("/"), "/");
        assert_eq!(remote_parent("/a/b/"), "/a");
        assert_eq!(remote_basename("/a/b"), "b");
        assert_eq!(remote_basename("/a/b/"), "b");
    }
}
