use std::path::Path;

/// Match a full path against a glob pattern.
/// Supports `*` (matches anything within one path segment), `?` (one char), and `**` (zero or more segments).
pub fn matches(path: &str, pattern: &str) -> bool {
    glob_recursive(path, pattern)
}

/// Check if any component of a path (relative to root) matches any of the given patterns.
/// Used for exclude filtering — patterns match against individual path components.
pub fn path_matches_any(path: &Path, source_root: &Path, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let relative = path.strip_prefix(source_root).unwrap_or(path);
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy();
        for pattern in patterns {
            if pattern.contains('*') || pattern.contains('?') {
                if glob_simple(&name, pattern) {
                    return true;
                }
            } else if name == *pattern {
                return true;
            }
        }
    }
    false
}

fn glob_recursive(path: &str, pattern: &str) -> bool {
    if let Some(idx) = pattern.find("**") {
        let prefix = &pattern[..idx];
        let suffix = &pattern[idx + 2..];
        let suffix = suffix.strip_prefix('/').unwrap_or(suffix);

        if !prefix.is_empty() {
            let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
            if !path.starts_with(prefix) && !glob_simple_prefix(path, prefix) {
                return false;
            }
        }

        if suffix.is_empty() {
            return true;
        }

        for i in 0..=path.len() {
            if (i == 0 || i == path.len() || path.as_bytes()[i - 1] == b'/' || prefix.is_empty())
                && glob_simple(&path[i..], suffix)
            {
                return true;
            }
        }
        false
    } else {
        glob_simple(path, pattern)
    }
}

fn glob_simple_prefix(path: &str, pattern: &str) -> bool {
    for i in 0..=path.len() {
        if (i == path.len() || path.as_bytes()[i] == b'/') && glob_simple(&path[..i], pattern) {
            return true;
        }
    }
    false
}

fn glob_simple(text: &str, pattern: &str) -> bool {
    let t = text.as_bytes();
    let p = pattern.as_bytes();
    let mut ti = 0;
    let mut pi = 0;
    let mut star_ti: Option<usize> = None;
    let mut star_pi: Option<usize> = None;

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_ti = Some(ti);
            star_pi = Some(pi + 1);
            pi += 1;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) && t[ti] != b'/' {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] && t[ti] == b'/' {
            ti += 1;
            pi += 1;
            star_ti = None;
            star_pi = None;
        } else if let (Some(sti), Some(spi)) = (star_ti, star_pi) {
            if t[sti] == b'/' {
                return false;
            }
            let new_sti = sti + 1;
            star_ti = Some(new_sti);
            ti = new_sti;
            pi = spi;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_glob_simple_exact() {
        assert!(matches("photo.jpg", "photo.jpg"));
        assert!(!matches("photo.jpg", "video.mp4"));
    }

    #[test]
    fn test_glob_star() {
        assert!(matches("photo.jpg", "*.jpg"));
        assert!(matches("IMG_1234.jpg", "IMG_*.jpg"));
        assert!(!matches("photo.jpg", "*.mp4"));
    }

    #[test]
    fn test_glob_star_does_not_cross_slash() {
        assert!(!matches("a/b.jpg", "*.jpg"));
        assert!(matches("a/b.jpg", "a/*.jpg"));
    }

    #[test]
    fn test_glob_question_mark() {
        assert!(matches("a.jpg", "?.jpg"));
        assert!(!matches("ab.jpg", "?.jpg"));
    }

    #[test]
    fn test_glob_double_star() {
        assert!(matches("a/b/c.jpg", "**/*.jpg"));
        assert!(matches("c.jpg", "**/*.jpg"));
        assert!(matches("marco/2026/05/photo.jpg", "marco/**"));
        assert!(matches("marco/photo.jpg", "marco/**"));
    }

    #[test]
    fn test_glob_double_star_prefix() {
        assert!(matches("marco/2026/05/photo.jpg", "marco/2026/**"));
        assert!(!matches("laura/2026/05/photo.jpg", "marco/2026/**"));
    }

    #[test]
    fn test_path_matches_any_exact() {
        let root = Path::new("/mnt/nas");
        let patterns = vec!["@eaDir".to_string()];
        assert!(path_matches_any(Path::new("/mnt/nas/@eaDir/file.jpg"), root, &patterns));
        assert!(path_matches_any(Path::new("/mnt/nas/sub/@eaDir/file.jpg"), root, &patterns));
        assert!(!path_matches_any(Path::new("/mnt/nas/photo.jpg"), root, &patterns));
    }

    #[test]
    fn test_path_matches_any_glob() {
        let root = Path::new("/mnt/nas");
        let patterns = vec!["*.tmp".to_string()];
        assert!(path_matches_any(Path::new("/mnt/nas/file.tmp"), root, &patterns));
        assert!(!path_matches_any(Path::new("/mnt/nas/file.jpg"), root, &patterns));
    }

    #[test]
    fn test_path_matches_any_empty_patterns() {
        let root = Path::new("/mnt/nas");
        assert!(!path_matches_any(Path::new("/mnt/nas/file.jpg"), root, &[]));
    }

    #[test]
    fn test_path_matches_any_multiple() {
        let root = Path::new("/mnt/nas");
        let patterns = vec!["@eaDir".to_string(), "#recycle".to_string(), ".DS_Store".to_string()];
        assert!(path_matches_any(Path::new("/mnt/nas/@eaDir/x"), root, &patterns));
        assert!(path_matches_any(Path::new("/mnt/nas/#recycle/x"), root, &patterns));
        assert!(path_matches_any(Path::new("/mnt/nas/.DS_Store"), root, &patterns));
        assert!(!path_matches_any(Path::new("/mnt/nas/photo.jpg"), root, &patterns));
    }
}
