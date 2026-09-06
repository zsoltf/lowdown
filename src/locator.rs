use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde_json::Value;
use walkdir::WalkDir;

use crate::host::{codex_home_dir, expand_user_path};

const DEFAULT_MAX_CANDIDATES: usize = 200;

pub fn resolve_session_path(session_dir: &Path) -> Result<PathBuf> {
    let path = expand_user_path(session_dir);
    if path.is_file() {
        return Ok(path);
    }
    if !path.is_dir() {
        bail!(
            "{} is not a rollout JSONL file or directory",
            path.display()
        );
    }
    let mut candidates = fs::read_dir(&path)
        .with_context(|| format!("read {}", path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate.is_file()
                && candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.pop().context(format!(
        "{} does not contain any rollout-*.jsonl files",
        path.display()
    ))
}

pub fn discover_latest_session(repo_root: &Path, lookback_days: i64) -> Result<PathBuf> {
    let matches = discover_repo_sessions(repo_root, lookback_days)?;
    matches
        .into_iter()
        .next()
        .context("no matching rollout sessions found")
}

pub fn discover_repo_sessions(repo_root: &Path, lookback_days: i64) -> Result<Vec<PathBuf>> {
    if !(1..=36500).contains(&lookback_days) {
        bail!("--lookback-days must be between 1 and 36500");
    }

    let repo_root = expand_user_path(repo_root)
        .canonicalize()
        .with_context(|| format!("canonicalize repo root {}", repo_root.display()))?;
    let session_root = default_session_root()?;
    let session_index = default_session_index()?;

    let candidates = candidate_session_paths(&session_root, &session_index, lookback_days)?;
    let mut matches = Vec::new();

    for path in candidates {
        let Some(meta) = sniff_session_meta(&path)? else {
            continue;
        };
        let Some(cwd) = meta
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .map(|cwd| expand_user_path(&cwd))
            .and_then(|cwd| cwd.canonicalize().ok())
        else {
            continue;
        };
        if cwd != repo_root && !cwd.starts_with(&repo_root) {
            continue;
        }
        matches.push(path);
    }

    if matches.is_empty() {
        bail!(
            "no recent rollout JSONL sessions matched that repo root; use --session for an older pinned log"
        );
    }

    Ok(matches)
}

fn candidate_session_paths(
    session_root: &Path,
    session_index: &Path,
    lookback_days: i64,
) -> Result<Vec<PathBuf>> {
    let indexed = indexed_candidate_paths(
        session_root,
        &recent_indexed_sessions(session_index, lookback_days)?,
    )?;
    let recent = recent_paths_by_mtime(session_root, lookback_days)?;

    let mut freshness_by_path = std::collections::HashMap::<PathBuf, f64>::new();
    for (freshness, path) in indexed.into_iter().chain(recent) {
        freshness_by_path
            .entry(path)
            .and_modify(|existing| {
                if freshness > *existing {
                    *existing = freshness;
                }
            })
            .or_insert(freshness);
    }

    let mut ordered = freshness_by_path.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(ordered
        .into_iter()
        .take(DEFAULT_MAX_CANDIDATES)
        .map(|(path, _)| path)
        .collect())
}

fn recent_indexed_sessions(
    session_index: &Path,
    lookback_days: i64,
) -> Result<Vec<(DateTime<Utc>, String)>> {
    if !session_index.exists() {
        return Ok(Vec::new());
    }

    let cutoff = Utc::now() - Duration::days(lookback_days);
    let contents = fs::read_to_string(session_index)
        .with_context(|| format!("read {}", session_index.display()))?;
    let mut indexed = Vec::new();

    for raw_line in contents.lines() {
        let Ok(record) = serde_json::from_str::<Value>(raw_line) else {
            continue;
        };
        let Some(session_id) = record.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(updated_at_raw) = record.get("updated_at").and_then(Value::as_str) else {
            continue;
        };
        let Ok(updated_at) = DateTime::parse_from_rfc3339(updated_at_raw) else {
            continue;
        };
        let updated_at = updated_at.with_timezone(&Utc);
        if updated_at < cutoff {
            continue;
        }
        indexed.push((updated_at, session_id.to_string()));
    }

    indexed.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut seen = std::collections::HashSet::new();
    indexed.retain(|(_, id)| seen.insert(id.clone()));
    indexed.truncate(DEFAULT_MAX_CANDIDATES);
    Ok(indexed)
}

fn indexed_candidate_paths(
    session_root: &Path,
    indexed_sessions: &[(DateTime<Utc>, String)],
) -> Result<Vec<(f64, PathBuf)>> {
    if indexed_sessions.is_empty() {
        return Ok(Vec::new());
    }

    let freshness_by_session = indexed_sessions
        .iter()
        .map(|(updated_at, session_id)| (session_id.clone(), updated_at.timestamp() as f64))
        .collect::<std::collections::HashMap<_, _>>();

    let mut matched = Vec::new();
    for entry in WalkDir::new(session_root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        if !is_rollout_file(path) {
            continue;
        }
        let Some(session_id) = session_id_from_filename(path) else {
            continue;
        };
        let Some(freshness) = freshness_by_session.get(&session_id) else {
            continue;
        };
        matched.push((*freshness, path.to_path_buf()));
    }
    matched.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(matched)
}

fn recent_paths_by_mtime(session_root: &Path, lookback_days: i64) -> Result<Vec<(f64, PathBuf)>> {
    let cutoff = (Utc::now() - Duration::days(lookback_days)).timestamp() as f64;
    let mut candidates = Vec::new();
    for entry in WalkDir::new(session_root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        if !is_rollout_file(path) {
            continue;
        }
        let metadata = entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64())
            .unwrap_or_default();
        if modified < cutoff {
            continue;
        }
        candidates.push((modified, path.to_path_buf()));
    }
    candidates.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(DEFAULT_MAX_CANDIDATES);
    Ok(candidates)
}

fn sniff_session_meta(path: &Path) -> Result<Option<Value>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut first = String::new();
    use std::io::BufRead;
    if reader.read_line(&mut first)? == 0 {
        return Ok(None);
    }
    let Ok(record) = serde_json::from_str::<Value>(&first) else {
        return Ok(None);
    };
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(None);
    }
    Ok(record.get("payload").cloned())
}

fn session_id_from_filename(path: &Path) -> Option<String> {
    static SESSION_ID_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = SESSION_ID_RE.get_or_init(|| {
        Regex::new(r"([0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})$")
            .expect("session id regex is valid")
    });
    let stem = path.file_stem()?.to_string_lossy();
    regex
        .captures(&stem)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_string())
}

fn is_rollout_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

fn default_session_root() -> Result<PathBuf> {
    Ok(codex_home_dir()?.join("sessions"))
}

fn default_session_index() -> Result<PathBuf> {
    Ok(codex_home_dir()?.join("session_index.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn resolve_session_path_accepts_rollout_directory() {
        let tmp = TempDir::new().unwrap();
        let first = tmp
            .path()
            .join("rollout-2026-04-10T00-00-00-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
        let second = tmp
            .path()
            .join("rollout-2026-04-11T00-00-00-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl");
        fs::write(&first, "").unwrap();
        fs::write(&second, "").unwrap();

        let resolved = resolve_session_path(tmp.path()).unwrap();
        assert_eq!(resolved, second);
    }

    #[test]
    fn discover_repo_sessions_prefers_matching_recent_sessions() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().join("sessions");
        let index = tmp.path().join("session_index.jsonl");
        fs::create_dir_all(session_root.join("2026/04/13")).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();

        let matching = session_root.join(
            "2026/04/13/rollout-2026-04-13T00-00-00-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl",
        );
        let other = session_root.join(
            "2026/04/13/rollout-2026-04-13T00-00-00-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl",
        );
        write_rollout_meta(&matching, &repo);
        write_rollout_meta(&other, &tmp.path().join("other"));
        fs::write(
            &index,
            [
                "{\"id\":\"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\",\"updated_at\":\"2099-04-13T00:00:00Z\"}",
                "{\"id\":\"bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb\",\"updated_at\":\"2099-04-12T00:00:00Z\"}",
            ]
            .join("\n"),
        )
        .unwrap();

        let candidates = candidate_session_paths(&session_root, &index, 7).unwrap();
        assert_eq!(candidates[0], matching);
    }

    fn write_rollout_meta(path: &Path, cwd: &Path) {
        let mut file = fs::File::create(path).unwrap();
        writeln!(file, "{}", serde_json::json!({"type":"session_meta", "payload":{"id":"proof", "cwd":cwd, "timestamp":"2026-04-13T00:00:00Z"}}))
        .unwrap();
    }
}
