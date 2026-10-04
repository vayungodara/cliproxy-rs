//! Store version rules (internal/pluginstore/version.go).

/// Go `normalizeVersion`: trimmed, one leading `v`/`V` dropped (not from `v` alone).
pub fn normalize_version(version: &str) -> String {
    let version = version.trim();
    match version.as_bytes() {
        [b'v' | b'V', _, ..] => version[1..].to_owned(),
        _ => version.to_owned(),
    }
}

/// Go `UpdateAvailable`: `latest` is offered over `installed` when it differs, unless
/// both are dotted numbers and `latest` is not newer.
pub fn update_available(installed: &str, latest: &str) -> bool {
    let (installed, latest) = (normalize_version(installed), normalize_version(latest));
    if installed.is_empty() || latest.is_empty() || installed == latest {
        return false;
    }
    match compare_versions(&installed, &latest) {
        Some(ordering) => ordering == std::cmp::Ordering::Less,
        None => true,
    }
}

/// Go `compareVersions`: dotted numeric segments, missing ones zero; `None` when a
/// segment is not a non-negative integer.
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split('.').collect(), b.split('.').collect());
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (segment(&a, i)?, segment(&b, i)?);
        if x != y {
            return Some(x.cmp(&y));
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// `strconv.ParseInt(s, 10, 64)` of one segment, non-negative.
fn segment(segments: &[&str], i: usize) -> Option<i64> {
    match segments.get(i) {
        None => Some(0),
        Some(s) => {
            let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
            if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            s.parse::<i64>().ok().filter(|n| *n >= 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_follow_go() {
        assert_eq!(normalize_version(" v1.2 "), "1.2");
        assert_eq!(normalize_version("v"), "v");
        assert!(update_available("1.2.0", "v1.10"));
        assert!(!update_available("1.10", "1.9"));
        assert!(!update_available("1.0", "1.0.0"));
        assert!(update_available("1.0-beta", "1.0"));
        assert!(!update_available("", "1.0"));
    }
}
