
/// `filepath.Separator` on Windows.
pub(crate) const SEP: char = '\\';

/// `os.IsPathSeparator` on Windows: both slashes separate.
pub(crate) fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

fn is_ascii_alpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// `filepath.VolumeName` for the drive-letter and UNC forms the walk produces.
pub(crate) fn volume_name(path: &str) -> &str {
    let b = path.as_bytes();
    if b.len() >= 2
        && is_sep(path.chars().next().unwrap_or(' '))
        && is_sep(path.chars().nth(1).unwrap_or(' '))
    {
        let mut i = 2;
        while i < b.len() && !is_sep(b[i] as char) {
            i += 1;
        }
        if i < b.len() {
            i += 1; // the separator
            let start = i;
            while i < b.len() && !is_sep(b[i] as char) {
                i += 1;
            }
            if i > start {
                return &path[..i];
            }
        }
        return &path[..i.min(b.len())];
    }
    if b.len() >= 2 && b[1] == b':' && is_ascii_alpha(b[0]) {
        return &path[..2];
    }
    ""
}

/// `filepath.Clean`, enough for the absolute paths and the `.`/`..` handling
/// the library uses. Forward slashes are normalised to `SEP` first.
pub(crate) fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let vol = volume_name(path);
    let rest = &path[vol.len()..];
    let separated: String = rest
        .chars()
        .map(|c| if c == '/' { SEP } else { c })
        .collect();
    let rooted = separated.starts_with(SEP);
    let mut stack: Vec<&str> = Vec::new();
    for part in separated.split(SEP) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if !stack.is_empty() && stack.last() != Some(&"..") {
                stack.pop();
            } else if vol.is_empty() && !rooted {
                stack.push("..");
            }
            continue;
        }
        stack.push(part);
    }
    let mut out = String::from(vol);
    if rooted {
        out.push(SEP);
    }
    if stack.is_empty() {
        if !rooted {
            out.push('.');
        }
    } else {
        out.push_str(&stack.join(&SEP.to_string()));
    }
    out
}

/// `filepath.Dir`.
pub(crate) fn dir(path: &str) -> String {
    let vol = volume_name(path);
    let b = path.as_bytes();
    let mut i = b.len();
    while i > vol.len() && !is_sep(b[i - 1] as char) {
        i -= 1;
    }
    let d = clean(&path[vol.len()..i]);
    if d == "." && vol.len() > 2 {
        return vol.to_string();
    }
    format!("{vol}{d}")
}

/// `filepath.Base`.
pub(crate) fn base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let b = path.as_bytes();
    let mut end = b.len();
    while end > 0 && is_sep(b[end - 1] as char) {
        end -= 1;
    }
    let vol = volume_name(path);
    let vol_end = vol.len().min(end);
    let mut i = end;
    while i > vol_end && !is_sep(b[i - 1] as char) {
        i -= 1;
    }
    if i >= end {
        return SEP.to_string();
    }
    path[i..end].to_string()
}

/// `filepath.Ext`.
pub(crate) fn ext(path: &str) -> String {
    let b = path.as_bytes();
    let mut i = b.len();
    while i > 0 && !is_sep(b[i - 1] as char) {
        i -= 1;
        if b[i] == b'.' {
            return path[i..].to_string();
        }
    }
    String::new()
}

/// `filepath.Join`.
pub(crate) fn join(parts: &[&str]) -> String {
    for (i, e) in parts.iter().enumerate() {
        if !e.is_empty() {
            let joined = parts[i..].join(&SEP.to_string());
            return clean(&joined);
        }
    }
    String::new()
}

/// `normalizeScanDir`: `filepath.Clean` plus lower-casing on Windows, so the
/// same folder never gets two records under different spellings.
pub(crate) fn normalize_scan_dir(dir: &str) -> String {
    let c = clean(dir);
    if cfg!(windows) { c.to_lowercase() } else { c }
}
