use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub(crate) fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(
            || match (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
                (Some(drive), Some(path)) => {
                    let mut joined = PathBuf::from(drive);
                    joined.push(path);
                    Some(joined)
                }
                _ => None,
            },
        )
}

pub(crate) fn codex_home_dir() -> Result<PathBuf> {
    if let Some(explicit) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    let home = home_dir().context("no home directory was available and CODEX_HOME is not set")?;
    Ok(home.join(".codex"))
}

pub(crate) fn default_cache_home() -> PathBuf {
    if let Some(xdg_cache_home) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg_cache_home);
    }
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data);
    }
    if let Some(app_data) = env::var_os("APPDATA") {
        return PathBuf::from(app_data);
    }
    home_dir()
        .map(|home| home.join(".cache"))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn expand_user_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| path.to_path_buf());
    }
    path.to_path_buf()
}

pub(crate) fn looks_like_absolute_path(text: &str) -> bool {
    let raw = text.trim();
    raw.starts_with('/')
        || raw.starts_with("\\\\")
        || raw.as_bytes().get(1).is_some_and(|byte| *byte == b':')
            && raw
                .as_bytes()
                .get(2)
                .is_some_and(|byte| *byte == b'/' || *byte == b'\\')
}

pub(crate) fn basename_from_path_like(text: &str) -> String {
    let trimmed = text.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unix_windows_and_unc_absolute_paths() {
        assert!(looks_like_absolute_path("/tmp/puppy.txt"));
        assert!(looks_like_absolute_path("C:\\Users\\kitty\\note.md"));
        assert!(looks_like_absolute_path("\\\\server\\share\\rollout.jsonl"));
        assert!(!looks_like_absolute_path("relative/path.txt"));
    }

    #[test]
    fn basename_handles_backslashes() {
        assert_eq!(
            basename_from_path_like("C:\\Users\\kitty\\note.md"),
            "note.md"
        );
        assert_eq!(
            basename_from_path_like("/tmp/puppy/rollout.jsonl"),
            "rollout.jsonl"
        );
    }
}
