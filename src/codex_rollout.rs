use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset};
use serde::{Serialize, Serializer};
use serde_json::Value;

const TAIL_READ_BLOCK_BYTES: usize = 64 * 1024;
const BOOTSTRAP_SCHEMA_VERSION: u32 = 1;
const BOOTSTRAP_SLICE_TYPE: &str = "recent_updates_bootstrap";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RecentMessage {
    pub offset: u64,
    #[serde(serialize_with = "serialize_timestamp")]
    pub timestamp: DateTime<FixedOffset>,
    pub text: String,
    pub phase: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RecentUpdatesBootstrap {
    pub schema_version: u32,
    pub slice_type: &'static str,
    pub session_path: PathBuf,
    pub cwd: PathBuf,
    pub target_updates: usize,
    pub scanned_complete_lines: usize,
    pub malformed_lines_skipped: usize,
    pub updates: Vec<RecentMessage>,
    pub latest_final_answer: Option<RecentMessage>,
}

struct TailSelection {
    lines: Vec<TailLine>,
    scanned_complete_lines: usize,
    malformed_lines_skipped: usize,
}

#[derive(Clone, Debug)]
struct TailLine {
    offset: u64,
    text: String,
}

struct ScannedTail {
    recent_update_line_indices: Vec<usize>,
    turn_boundary_line_indices: Vec<usize>,
    malformed_lines_skipped: usize,
}

pub fn read_recent_updates(
    path: &Path,
    target_updates: usize,
    boundary_lookback_lines: usize,
) -> Result<RecentUpdatesBootstrap> {
    if target_updates == 0 {
        bail!("target_updates must be greater than zero");
    }

    let (cwd, _) = read_session_meta(path)?;
    let selection = read_recent_update_lines(path, target_updates, boundary_lookback_lines)?;

    let mut updates = Vec::new();
    let mut latest_final_answer = None;

    for tail_line in &selection.lines {
        let record: Value = match serde_json::from_str(&tail_line.text) {
            Ok(record) => record,
            Err(_) => continue,
        };
        if let Some(message) = parse_recent_message(&record, tail_line.offset) {
            if message.phase == "final_answer" {
                latest_final_answer = Some(message);
            } else {
                updates.push(message);
            }
        }
        if let Some(answer) = parse_task_complete_answer(&record, tail_line.offset) {
            latest_final_answer = Some(answer);
        }
    }

    if updates.len() > target_updates {
        updates = updates[updates.len() - target_updates..].to_vec();
    }

    Ok(RecentUpdatesBootstrap {
        schema_version: BOOTSTRAP_SCHEMA_VERSION,
        slice_type: BOOTSTRAP_SLICE_TYPE,
        session_path: path.to_path_buf(),
        cwd,
        target_updates,
        scanned_complete_lines: selection.scanned_complete_lines,
        malformed_lines_skipped: selection.malformed_lines_skipped,
        updates,
        latest_final_answer,
    })
}

fn read_session_meta(path: &Path) -> Result<(PathBuf, Option<String>)> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    let read = reader.read_line(&mut first)?;
    if read == 0 {
        bail!("{} is empty", path.display());
    }
    let value: Value = serde_json::from_str(&first).context("parse session_meta line")?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        bail!("unsupported session format");
    }
    let cwd = value
        .get("payload")
        .and_then(|payload| payload.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("session_meta missing cwd")?;
    Ok((cwd, Some(first)))
}

fn read_recent_update_lines(
    path: &Path,
    target_updates: usize,
    boundary_lookback_lines: usize,
) -> Result<TailSelection> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut position = file.seek(SeekFrom::End(0))?;
    let mut reversed_line = Vec::new();
    let mut lines = Vec::new();
    let mut updates = 0usize;
    let mut context_remaining = None;
    let mut at_eof = true;
    let mut finished = false;

    // Parse each complete record once, scanning backwards in fixed-size blocks.
    // Bytes of a split record are carried across blocks, including split UTF-8.
    while position > 0 && !finished {
        let read_size = TAIL_READ_BLOCK_BYTES.min(position as usize);
        position -= read_size as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut block = vec![0; read_size];
        file.read_exact(&mut block)?;
        for index in (0..block.len()).rev() {
            let byte = block[index];
            if byte != b'\n' {
                reversed_line.push(byte);
                continue;
            }
            if reversed_line.is_empty() && at_eof {
                at_eof = false;
                continue;
            }
            reversed_line.reverse();
            let text = String::from_utf8_lossy(&reversed_line).into_owned();
            reversed_line.clear();
            let record = serde_json::from_str::<Value>(&text);
            // An unfinished append is buffered by the file until the next read.
            if !at_eof || record.is_ok() {
                if record.as_ref().is_ok_and(is_recent_update_record) {
                    updates += 1;
                }
                lines.push(TailLine {
                    offset: position + index as u64 + 1,
                    text,
                });
                if let Some(remaining) = context_remaining.as_mut() {
                    if *remaining == 0 {
                        finished = true;
                        break;
                    }
                    *remaining -= 1;
                } else if updates >= target_updates {
                    context_remaining = Some(boundary_lookback_lines);
                }
            }
            at_eof = false;
        }
    }
    if !finished && !reversed_line.is_empty() {
        reversed_line.reverse();
        lines.push(TailLine {
            offset: 0,
            text: String::from_utf8_lossy(&reversed_line).into_owned(),
        });
    }
    lines.reverse();
    let scanned = scan_tail_lines(&lines);
    let start = recent_update_start_index(&scanned, target_updates, boundary_lookback_lines);
    Ok(TailSelection {
        scanned_complete_lines: lines.len(),
        malformed_lines_skipped: scanned.malformed_lines_skipped,
        lines: lines[start..].to_vec(),
    })
}

fn scan_tail_lines(lines: &[TailLine]) -> ScannedTail {
    let mut recent_update_line_indices = Vec::new();
    let mut turn_boundary_line_indices = Vec::new();
    let mut malformed_lines_skipped = 0usize;

    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str::<Value>(line.text.trim_end()) {
            Ok(record) => {
                if is_turn_boundary_record(&record) {
                    turn_boundary_line_indices.push(index);
                }
                if is_recent_update_record(&record) {
                    recent_update_line_indices.push(index);
                }
            }
            Err(_) => malformed_lines_skipped += 1,
        }
    }

    ScannedTail {
        recent_update_line_indices,
        turn_boundary_line_indices,
        malformed_lines_skipped,
    }
}

fn recent_update_start_index(
    scanned: &ScannedTail,
    target_updates: usize,
    boundary_lookback_lines: usize,
) -> usize {
    let update_anchor = if scanned.recent_update_line_indices.len() >= target_updates {
        let index = scanned.recent_update_line_indices.len() - target_updates;
        scanned.recent_update_line_indices[index]
    } else {
        scanned
            .recent_update_line_indices
            .first()
            .copied()
            .unwrap_or(0)
    };

    let floor = update_anchor.saturating_sub(boundary_lookback_lines);
    scanned
        .turn_boundary_line_indices
        .iter()
        .rev()
        .copied()
        .find(|index| *index >= floor && *index <= update_anchor)
        .unwrap_or(floor)
}

fn is_recent_update_record(record: &Value) -> bool {
    if record.get("type").and_then(Value::as_str) != Some("event_msg") {
        return false;
    }
    let Some(payload) = record.get("payload") else {
        return false;
    };
    if payload.get("type").and_then(Value::as_str) != Some("agent_message") {
        return false;
    }
    parse_recent_message(record, 0)
        .is_some_and(|message| message.phase != "final_answer" && !message.text.trim().is_empty())
}

fn is_turn_boundary_record(record: &Value) -> bool {
    if record.get("type").and_then(Value::as_str) != Some("event_msg") {
        return false;
    }
    let Some(payload) = record.get("payload") else {
        return false;
    };
    matches!(
        payload.get("type").and_then(Value::as_str),
        Some("task_started") | Some("user_message")
    )
}

fn parse_recent_message(record: &Value, offset: u64) -> Option<RecentMessage> {
    if record.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("agent_message") {
        return None;
    }
    let timestamp = parse_timestamp(record.get("timestamp")?.as_str()?)?;
    let text = payload.get("message")?.as_str()?.to_string();
    let phase = payload
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("assistant")
        .to_string();
    Some(RecentMessage {
        offset,
        timestamp,
        text,
        phase,
    })
}

fn parse_task_complete_answer(record: &Value, offset: u64) -> Option<RecentMessage> {
    if record.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("task_complete") {
        return None;
    }
    let text = payload.get("last_agent_message")?.as_str()?;
    if text.is_empty() {
        return None;
    }
    let timestamp = parse_timestamp(record.get("timestamp")?.as_str()?)?;
    Some(RecentMessage {
        offset,
        timestamp,
        text: text.to_string(),
        phase: "final_answer".to_string(),
    })
}

fn parse_timestamp(raw: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(raw).ok()
}

fn serialize_timestamp<S>(value: &DateTime<FixedOffset>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const FIXTURE: &str = include_str!("../tests/fixtures/sample_rollout.jsonl");

    #[test]
    fn reads_recent_updates_from_fixture() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{FIXTURE}").unwrap();

        let bootstrap = read_recent_updates(file.path(), 2, 64).unwrap();

        assert_eq!(bootstrap.schema_version, 1);
        assert_eq!(bootstrap.slice_type, "recent_updates_bootstrap");
        assert_eq!(bootstrap.target_updates, 2);
        assert_eq!(bootstrap.malformed_lines_skipped, 0);
        assert!(bootstrap.scanned_complete_lines >= 2);
        assert_eq!(bootstrap.updates.len(), 2);
        assert!(bootstrap.updates[0].offset > 0);
        assert_eq!(
            bootstrap.updates[0].text,
            "Reading the repo contract first, then I’ll scaffold the CLI and the first supported parser."
        );
        assert_eq!(
            bootstrap.updates[1].text,
            "Adding parser and grouping tests next, then I’ll run pytest."
        );
        assert_eq!(
            bootstrap
                .latest_final_answer
                .as_ref()
                .map(|message| message.text.as_str()),
            Some(
                "Parser and timeline tests are green. If you want, I can generate the first real digest artifact next."
            )
        );
    }

    #[test]
    fn excludes_inline_final_answer_messages_from_update_budget() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", synthetic_rollout_with_updates(12, false, true)).unwrap();

        let bootstrap = read_recent_updates(file.path(), 5, 64).unwrap();
        let texts = bootstrap
            .updates
            .into_iter()
            .map(|message| message.text)
            .collect::<Vec<_>>();

        assert_eq!(texts.len(), 5);
        assert_eq!(texts.first().unwrap(), "Progress update 8.");
        assert_eq!(texts.last().unwrap(), "Progress update 12.");
    }

    #[test]
    fn tracks_malformed_lines_in_scanned_tail_window() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", synthetic_rollout_with_updates(6, true, false)).unwrap();

        let bootstrap = read_recent_updates(file.path(), 4, 64).unwrap();
        let texts = bootstrap
            .updates
            .into_iter()
            .map(|message| message.text)
            .collect::<Vec<_>>();

        assert_eq!(bootstrap.malformed_lines_skipped, 1);
        assert_eq!(
            texts,
            vec![
                "Progress update 3.".to_string(),
                "Progress update 4.".to_string(),
                "Progress update 5.".to_string(),
                "Progress update 6.".to_string(),
            ]
        );
    }

    #[test]
    fn json_contract_exposes_stable_bootstrap_fields() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{FIXTURE}").unwrap();

        let bootstrap = read_recent_updates(file.path(), 2, 64).unwrap();
        let json = serde_json::to_value(&bootstrap).unwrap();

        assert_eq!(json.get("schema_version").and_then(Value::as_u64), Some(1));
        assert_eq!(
            json.get("slice_type").and_then(Value::as_str),
            Some("recent_updates_bootstrap")
        );
        assert_eq!(json.get("target_updates").and_then(Value::as_u64), Some(2));
        assert!(json.get("malformed_lines_skipped").is_some());
        assert!(json.get("scanned_complete_lines").is_some());
        assert!(json.get("updates").is_some());
        assert!(json["updates"][0].get("offset").is_some());
    }

    #[test]
    fn tail_reader_handles_split_utf8_and_partial_append() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{FIXTURE}").unwrap();
        let text = "é".repeat(TAIL_READ_BLOCK_BYTES);
        let record = serde_json::json!({"timestamp":"2026-04-11T19:00:00Z", "type":"event_msg", "payload":{"type":"agent_message", "message": text, "phase":"commentary"}});
        writeln!(file, "{record}").unwrap();
        write!(file, "{{\"unfinished\":").unwrap();
        let bootstrap = read_recent_updates(file.path(), 1, 0).unwrap();
        assert_eq!(bootstrap.updates.len(), 1);
        assert_eq!(bootstrap.updates[0].text, text);
    }

    fn synthetic_rollout_with_updates(
        update_count: usize,
        include_malformed_tail_line: bool,
        include_inline_final_answers: bool,
    ) -> String {
        let mut out = String::from(
            "{\"timestamp\":\"2026-04-11T18:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"proof\",\"timestamp\":\"2026-04-11T18:00:00Z\",\"cwd\":\"/tmp/project\",\"originator\":\"codex\",\"source\":\"codex\",\"cli_version\":\"1.0\"}}\n",
        );
        for index in 1..=update_count {
            let base_second = index * 3;
            out.push_str(&format!(
                "{{\"timestamp\":\"2026-04-11T18:00:{base_second:02}Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-{index:02}\"}}}}\n"
            ));
            out.push_str(&format!(
                "{{\"timestamp\":\"2026-04-11T18:00:{:02}Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Do thing {index:02}.\"}}}}\n",
                base_second + 1
            ));
            out.push_str(&format!(
                "{{\"timestamp\":\"2026-04-11T18:00:{:02}Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\",\"message\":\"Progress update {index}.\",\"phase\":\"commentary\"}}}}\n",
                base_second + 2
            ));
            if include_inline_final_answers {
                out.push_str(&format!(
                    "{{\"timestamp\":\"2026-04-11T18:00:{:02}Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\",\"message\":\"Inline final answer {index}.\",\"phase\":\"final_answer\"}}}}\n",
                    base_second + 2
                ));
            }
            if include_malformed_tail_line && index == update_count - 1 {
                out.push_str("this is not valid json\n");
            }
            out.push_str(&format!(
                "{{\"timestamp\":\"2026-04-11T18:01:{:02}Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"last_agent_message\":\"Final answer {index}.\"}}}}\n",
                index % 60
            ));
        }
        out
    }
}
