use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use serde_json::Value;

use crate::host::{basename_from_path_like, looks_like_absolute_path};
use crate::summaries::{CodexSummaryClient, UpdateSummary, raw_summary, update_key};
use crate::text::{crop_chars, first_sentence_or_line};
use crate::time_display::{format_digest_datetime, format_short_time};

const RECENT_UPDATES_LIMIT: usize = 12;
const TIMELINE_TARGET: usize = 10;

#[derive(Clone, Debug)]
struct SessionMeta {
    cwd: PathBuf,
}

#[derive(Clone, Debug)]
struct MessageEvent {
    timestamp: DateTime<FixedOffset>,
    line_no: usize,
    text: String,
    phase: Option<String>,
}

#[derive(Clone, Debug)]
struct ToolReceipt {
    line_no: usize,
    summary: String,
    exit_code: Option<i32>,
    changed_paths: Vec<PathBuf>,
    artifact_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
struct Turn {
    start_time: DateTime<FixedOffset>,
    start_line: usize,
    end_time: Option<DateTime<FixedOffset>>,
    completed: bool,
    user_messages: Vec<MessageEvent>,
    assistant_messages: Vec<MessageEvent>,
    receipts: Vec<ToolReceipt>,
}

#[derive(Clone, Debug)]
pub(crate) struct Session {
    path: PathBuf,
    meta: SessionMeta,
    turns: Vec<Turn>,
}

#[derive(Clone, Debug)]
struct TurnSummary {
    objective: String,
    summary: String,
    status: String,
    caveat: String,
    blocker_or_repair: String,
    next_step: String,
    stop_reason: String,
    changed_paths: Vec<PathBuf>,
    artifact_paths: Vec<PathBuf>,
    latest_receipt: Option<ToolReceipt>,
    start_time: DateTime<FixedOffset>,
    end_time: DateTime<FixedOffset>,
    start_line: usize,
    assistant_updates: Vec<MessageEvent>,
    final_answer: Option<MessageEvent>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DigestRef {
    label: String,
    target: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DigestUpdate {
    timestamp: String,
    status: String,
    summary: String,
    raw_text: String,
    summary_source: String,
    target: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DigestChapter {
    started_at: String,
    ended_at: String,
    summary: String,
    target: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NativeDigest {
    project: String,
    log: String,
    objective: String,
    what_changed: String,
    current_status: String,
    decision_changing_caveat: String,
    current_slice: String,
    blocker_or_active_repair: String,
    exact_next_step: String,
    stop_reason_if_stopped: String,
    update_summary_notice: String,
    latest_answer: Option<(String, String)>,
    recent_updates: Vec<DigestUpdate>,
    timeline: Vec<DigestChapter>,
    jump_refs: Vec<DigestRef>,
}

pub(crate) fn load_session(path: &Path) -> Result<Session> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut meta: Option<SessionMeta> = None;
    let mut turns: Vec<Turn> = Vec::new();
    let mut current_turn: Option<Turn> = None;
    let mut receipts_by_call_id: HashMap<String, usize> = HashMap::new();

    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let raw_line =
            line.with_context(|| format!("read line {} from {}", line_no, path.display()))?;
        let record: Value = match serde_json::from_str(&raw_line) {
            Ok(record) => record,
            Err(_) => continue,
        };
        let timestamp = match record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
        {
            Some(timestamp) => timestamp,
            None => continue,
        };

        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let cwd = record
                    .get("payload")
                    .and_then(|payload| payload.get("cwd"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .context("session_meta missing cwd")?;
                meta = Some(SessionMeta { cwd });
            }
            Some("event_msg") => {
                let payload = match record.get("payload") {
                    Some(payload) => payload,
                    None => continue,
                };
                match payload.get("type").and_then(Value::as_str) {
                    Some("task_started") => {
                        if let Some(turn) = current_turn.take()
                            && turn_has_signal(&turn)
                        {
                            turns.push(turn);
                        }
                        current_turn = Some(Turn {
                            start_time: timestamp,
                            start_line: line_no,
                            end_time: None,
                            completed: false,
                            user_messages: Vec::new(),
                            assistant_messages: Vec::new(),
                            receipts: Vec::new(),
                        });
                        receipts_by_call_id.clear();
                    }
                    Some("user_message") => {
                        let turn = ensure_turn(&mut current_turn, timestamp, line_no);
                        turn.user_messages.push(MessageEvent {
                            timestamp,
                            line_no,
                            text: payload
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            phase: None,
                        });
                    }
                    Some("agent_message") => {
                        let turn = ensure_turn(&mut current_turn, timestamp, line_no);
                        turn.assistant_messages.push(MessageEvent {
                            timestamp,
                            line_no,
                            text: payload
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            phase: payload
                                .get("phase")
                                .and_then(Value::as_str)
                                .map(|value| value.to_string()),
                        });
                    }
                    Some("exec_command_end") => {
                        let turn = ensure_turn(&mut current_turn, timestamp, line_no);
                        let call_id = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let receipt = ToolReceipt {
                            line_no,
                            summary: summarize_exec_receipt(payload),
                            exit_code: payload
                                .get("exit_code")
                                .and_then(Value::as_i64)
                                .map(|value| value as i32),
                            changed_paths: Vec::new(),
                            artifact_paths: extract_paths_from_value(payload),
                        };
                        turn.receipts.push(receipt);
                        if !call_id.is_empty() {
                            receipts_by_call_id.insert(call_id, turn.receipts.len() - 1);
                        }
                    }
                    Some("patch_apply_end") => {
                        let turn = ensure_turn(&mut current_turn, timestamp, line_no);
                        let changed_paths = payload
                            .get("changes")
                            .and_then(Value::as_object)
                            .map(|changes| changes.keys().map(PathBuf::from).collect::<Vec<_>>())
                            .unwrap_or_default();
                        let receipt = ToolReceipt {
                            line_no,
                            summary: if changed_paths.is_empty() {
                                "Applied patch.".to_string()
                            } else {
                                format!(
                                    "Patched {} files: {}.",
                                    changed_paths.len(),
                                    basename_list(&changed_paths, 3)
                                )
                            },
                            exit_code: None,
                            changed_paths,
                            artifact_paths: extract_paths_from_value(payload),
                        };
                        turn.receipts.push(receipt);
                    }
                    Some("task_complete") => {
                        let turn = ensure_turn(&mut current_turn, timestamp, line_no);
                        let final_text = payload
                            .get("last_agent_message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if !final_text.is_empty()
                            && !turn
                                .assistant_messages
                                .iter()
                                .any(|message| message.text.trim() == final_text)
                        {
                            turn.assistant_messages.push(MessageEvent {
                                timestamp,
                                line_no,
                                text: final_text,
                                phase: Some("final_answer".to_string()),
                            });
                        }
                        turn.completed = true;
                        turn.end_time = Some(timestamp);
                        if let Some(turn) = current_turn.take()
                            && turn_has_signal(&turn)
                        {
                            turns.push(turn);
                        }
                        receipts_by_call_id.clear();
                    }
                    _ => {}
                }
            }
            Some("response_item") => {
                let payload = match record.get("payload") {
                    Some(payload) => payload,
                    None => continue,
                };
                let item_type = payload.get("type").and_then(Value::as_str);
                if matches!(
                    item_type,
                    Some("function_call_output") | Some("custom_tool_call_output")
                ) {
                    let turn = ensure_turn(&mut current_turn, timestamp, line_no);
                    turn.receipts.push(ToolReceipt {
                        line_no,
                        summary: first_sentence_or_line(
                            payload
                                .get("output")
                                .and_then(Value::as_str)
                                .unwrap_or("Tool output."),
                        ),
                        exit_code: None,
                        changed_paths: Vec::new(),
                        artifact_paths: extract_paths_from_value(payload),
                    });
                }
            }
            _ => {}
        }
    }

    if let Some(turn) = current_turn.take()
        && turn_has_signal(&turn)
    {
        turns.push(turn);
    }

    let meta = meta.context("session missing session_meta")?;
    Ok(Session {
        path: path.to_path_buf(),
        meta,
        turns,
    })
}

pub(crate) fn build_digest(session: &Session) -> NativeDigest {
    build_digest_with_client(session, &CodexSummaryClient::new())
}

fn build_digest_with_client(session: &Session, client: &CodexSummaryClient) -> NativeDigest {
    let turn_summaries = session
        .turns
        .iter()
        .filter(|turn| turn_has_signal(turn))
        .map(summarize_turn)
        .collect::<Vec<_>>();

    let latest = turn_summaries
        .last()
        .cloned()
        .unwrap_or_else(empty_turn_summary);
    let recent_updates = build_recent_updates(&session.path, &turn_summaries, client);
    let timeline = build_timeline(&turn_summaries);
    let latest_answer = latest.final_answer.as_ref().map(|message| {
        (
            scrub_display_paths(&message.text),
            format!("session log, line {}", message.line_no),
        )
    });
    let jump_refs = build_jump_refs(session, &latest);

    NativeDigest {
        project: project_label(&session.meta.cwd),
        log: log_label(&session.path),
        objective: latest_user_objective(&turn_summaries)
            .unwrap_or_else(|| "No explicit objective recorded.".to_string()),
        what_changed: if latest.changed_paths.is_empty() {
            format!(
                "No file changes confirmed by parsed receipts. Reported: {}",
                latest.summary
            )
        } else {
            format!(
                "Patch receipts name: {}",
                latest
                    .changed_paths
                    .iter()
                    .map(|path| basename(path))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        current_status: status_label(&latest.status),
        decision_changing_caveat: latest.caveat.clone(),
        current_slice: latest.objective.clone(),
        blocker_or_active_repair: latest.blocker_or_repair.clone(),
        exact_next_step: latest.next_step.clone(),
        stop_reason_if_stopped: latest.stop_reason.clone(),
        update_summary_notice: summary_notice(&recent_updates),
        latest_answer,
        recent_updates,
        timeline,
        jump_refs,
    }
}

pub(crate) fn render_pretty(digest: &NativeDigest) -> String {
    let mut lines = vec![
        format!("Project: {}", digest.project),
        format!("Log: {}", digest.log),
        String::new(),
        "Headline".to_string(),
        format!("- objective: {}", digest.objective),
        format!("- what actually changed: {}", digest.what_changed),
        format!("- current status: {}", digest.current_status),
        format!(
            "- decision-changing caveat: {}",
            digest.decision_changing_caveat
        ),
        String::new(),
        "Rolling state".to_string(),
        format!("- current slice: {}", digest.current_slice),
        format!(
            "- blocker or active repair: {}",
            digest.blocker_or_active_repair
        ),
        format!("- exact next step: {}", digest.exact_next_step),
        format!(
            "- stop reason if stopped: {}",
            digest.stop_reason_if_stopped
        ),
        String::new(),
        "Summary mode".to_string(),
        format!("- update summaries: {}", digest.update_summary_notice),
        String::new(),
        "Latest answer".to_string(),
    ];
    if let Some((text, target)) = &digest.latest_answer {
        lines.push(text.clone());
        lines.push(format!("[{}]", target));
    } else {
        lines.push("No final answer yet.".to_string());
    }

    lines.push(String::new());
    lines.push("Recent updates".to_string());
    for (index, update) in digest.recent_updates.iter().enumerate() {
        lines.push(format!(
            "{}. {} {} [{}]",
            index + 1,
            update.timestamp,
            update.summary,
            update.target
        ));
    }

    lines.push(String::new());
    lines.push("Chaptered timeline".to_string());
    for (index, chapter) in digest.timeline.iter().enumerate() {
        lines.push(format!(
            "{}. {}-{} {} [{}]",
            index + 1,
            chapter.started_at,
            chapter.ended_at,
            chapter.summary,
            chapter.target
        ));
    }

    lines.push(String::new());
    lines.push("Jump refs".to_string());
    for jump_ref in &digest.jump_refs {
        lines.push(format!("- {}: {}", jump_ref.label, jump_ref.target));
    }
    lines.join("\n")
}

fn ensure_turn(
    current_turn: &mut Option<Turn>,
    timestamp: DateTime<FixedOffset>,
    line_no: usize,
) -> &mut Turn {
    current_turn.get_or_insert_with(|| Turn {
        start_time: timestamp,
        start_line: line_no,
        end_time: None,
        completed: false,
        user_messages: Vec::new(),
        assistant_messages: Vec::new(),
        receipts: Vec::new(),
    })
}

fn turn_has_signal(turn: &Turn) -> bool {
    !turn.user_messages.is_empty()
        || !turn.assistant_messages.is_empty()
        || !turn.receipts.is_empty()
}

fn summarize_exec_receipt(payload: &Value) -> String {
    let output = payload
        .get("aggregated_output")
        .and_then(Value::as_str)
        .or_else(|| payload.get("stdout").and_then(Value::as_str))
        .or_else(|| payload.get("stderr").and_then(Value::as_str))
        .unwrap_or("");
    let first = first_sentence_or_line(output);
    if !first.is_empty() {
        return crop_chars(&first, 160);
    }
    if let Some(parts) = payload.get("command").and_then(Value::as_array) {
        let command = parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        if !command.is_empty() {
            return crop_chars(&format!("Ran {}", command), 160);
        }
    }
    "Ran exec command.".to_string()
}

fn extract_paths_from_value(value: &Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    extract_paths_recursive(value, &mut paths);
    paths
}

fn extract_paths_recursive(value: &Value, paths: &mut Vec<PathBuf>) {
    match value {
        Value::String(text) => {
            for token in text.split_whitespace() {
                let cleaned =
                    token.trim_matches(|ch: char| matches!(ch, ',' | '.' | ')' | ']' | '"' | '\''));
                if looks_like_absolute_path(cleaned) {
                    let path = PathBuf::from(cleaned);
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                extract_paths_recursive(item, paths);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                extract_paths_recursive(value, paths);
            }
        }
        _ => {}
    }
}

fn summarize_turn(turn: &Turn) -> TurnSummary {
    let latest_receipt = turn.receipts.last().cloned();
    let objective = turn
        .user_messages
        .first()
        .map(|message| clean_objective(&message.text))
        .filter(|text| !text.is_empty())
        .or_else(|| {
            turn.assistant_messages
                .first()
                .map(|message| first_sentence_or_line(&message.text))
        })
        .unwrap_or_else(|| "No explicit objective recorded.".to_string());

    let final_answer = turn
        .assistant_messages
        .iter()
        .rev()
        .find(|message| message.phase.as_deref() == Some("final_answer"))
        .cloned();
    let assistant_updates = turn
        .assistant_messages
        .iter()
        .filter(|message| message.phase.as_deref() != Some("final_answer"))
        .cloned()
        .collect::<Vec<_>>();
    let summary = final_answer
        .as_ref()
        .map(|message| first_sentence_or_line(&message.text))
        .filter(|text| !text.is_empty())
        .or_else(|| {
            assistant_updates
                .last()
                .map(|message| first_sentence_or_line(&message.text))
        })
        .or_else(|| {
            latest_receipt
                .as_ref()
                .map(|receipt| receipt.summary.clone())
        })
        .unwrap_or_else(|| objective.clone());
    let status = classify_status(
        turn,
        &assistant_updates,
        latest_receipt.as_ref(),
        final_answer.as_ref(),
    );
    let caveat = decision_caveat(&assistant_updates, latest_receipt.as_ref(), &status);
    let blocker_or_repair = blocker_or_repair(&status, &assistant_updates, latest_receipt.as_ref());
    let next_step = assistant_updates
        .iter()
        .rev()
        .find_map(|message| extract_next_step(&message.text))
        .unwrap_or_else(|| "No explicit next step was recorded.".to_string());
    let stop_reason = if status == "stopped" {
        format!(
            "Turn completed at {} and no further activity appears in this session.",
            format_digest_datetime(turn.end_time.unwrap_or(turn.start_time))
        )
    } else {
        "Session still looked active at the end of the recorded turn.".to_string()
    };
    let changed_paths = turn
        .receipts
        .iter()
        .flat_map(|receipt| receipt.changed_paths.iter().cloned())
        .fold(Vec::<PathBuf>::new(), |mut acc, path| {
            if !acc.contains(&path) {
                acc.push(path);
            }
            acc
        });
    let artifact_paths = turn
        .receipts
        .iter()
        .flat_map(|receipt| receipt.artifact_paths.iter().cloned())
        .fold(Vec::<PathBuf>::new(), |mut acc, path| {
            if !acc.contains(&path) {
                acc.push(path);
            }
            acc
        });

    TurnSummary {
        objective,
        summary,
        status,
        caveat,
        blocker_or_repair,
        next_step,
        stop_reason,
        changed_paths,
        artifact_paths,
        latest_receipt,
        start_time: turn.start_time,
        end_time: turn.end_time.unwrap_or(turn.start_time),
        start_line: turn.start_line,
        assistant_updates,
        final_answer,
    }
}

fn classify_status(
    turn: &Turn,
    assistant_updates: &[MessageEvent],
    latest_receipt: Option<&ToolReceipt>,
    final_answer: Option<&MessageEvent>,
) -> String {
    let combined = assistant_updates
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if latest_receipt
        .and_then(|receipt| receipt.exit_code)
        .is_some_and(|code| code != 0)
    {
        return "repairing".to_string();
    }
    if contains_any(
        &combined,
        &["wait", "monitor", "hold steady", "keep waiting"],
    ) {
        return "waiting".to_string();
    }
    if final_answer.is_some() && turn.completed {
        return "stopped".to_string();
    }
    "progressing".to_string()
}

fn decision_caveat(
    assistant_updates: &[MessageEvent],
    latest_receipt: Option<&ToolReceipt>,
    status: &str,
) -> String {
    if status == "repairing" {
        return latest_receipt
            .map(|receipt| receipt.summary.clone())
            .unwrap_or_else(|| "A failed step was visible in the recent turn.".to_string());
    }
    for message in assistant_updates.iter().rev() {
        let text = message.text.to_lowercase();
        if contains_any(
            &text,
            &["wait", "monitor", "blocked", "missing", "failed", "partial"],
        ) {
            return crop_chars(&first_sentence_or_line(&message.text), 160);
        }
    }
    "No decision-reversing caveat was explicit in the latest turn.".to_string()
}

fn blocker_or_repair(
    status: &str,
    assistant_updates: &[MessageEvent],
    latest_receipt: Option<&ToolReceipt>,
) -> String {
    if status == "repairing" {
        return latest_receipt
            .map(|receipt| receipt.summary.clone())
            .unwrap_or_else(|| "A failed step was visible.".to_string());
    }
    if status == "waiting" {
        return assistant_updates
            .iter()
            .rev()
            .map(|message| first_sentence_or_line(&message.text))
            .find(|text| !text.is_empty())
            .unwrap_or_else(|| "Waiting or monitoring.".to_string());
    }
    "No active blocker or repair was explicit in the latest turn.".to_string()
}

fn build_recent_updates(
    session_path: &Path,
    turns: &[TurnSummary],
    client: &CodexSummaryClient,
) -> Vec<DigestUpdate> {
    let messages = turns
        .iter()
        .flat_map(|turn| turn.assistant_updates.iter().cloned())
        .collect::<Vec<_>>();
    let messages = if messages.len() > RECENT_UPDATES_LIMIT {
        messages[messages.len() - RECENT_UPDATES_LIMIT..].to_vec()
    } else {
        messages
    };

    let recent_messages = messages
        .iter()
        .map(|message| crate::codex_rollout::RecentMessage {
            offset: message.line_no as u64,
            timestamp: message.timestamp,
            text: message.text.clone(),
            phase: message
                .phase
                .clone()
                .unwrap_or_else(|| "commentary".to_string()),
        })
        .collect::<Vec<_>>();
    let mut summaries = client.cached_for_updates(session_path, &recent_messages);
    if client.available() {
        let missing = recent_messages
            .iter()
            .filter(|message| !summaries.contains_key(&update_key(message)))
            .cloned()
            .collect::<Vec<_>>();
        for batch in missing.chunks(8) {
            if let Ok(hydrated) = client.summarize_updates(session_path, batch) {
                summaries.extend(hydrated);
            }
        }
    }

    messages
        .iter()
        .zip(recent_messages.iter())
        .map(|message| {
            let (raw_message, recent_message) = message;
            let key = update_key(recent_message);
            let summary = summaries
                .get(&key)
                .cloned()
                .unwrap_or_else(|| UpdateSummary {
                    key,
                    summary: raw_summary(recent_message),
                    source: "fallback_local".to_string(),
                });
            DigestUpdate {
                timestamp: format_short_time(raw_message.timestamp),
                status: "progressing".to_string(),
                summary: summary.summary,
                raw_text: raw_message.text.clone(),
                summary_source: summary.source,
                target: format!("session log, line {}", raw_message.line_no),
            }
        })
        .collect()
}

fn build_timeline(turns: &[TurnSummary]) -> Vec<DigestChapter> {
    if turns.is_empty() {
        return Vec::new();
    }
    let group_count = turns.len().clamp(1, TIMELINE_TARGET);
    let chunk_size = turns.len().div_ceil(group_count);
    turns
        .chunks(chunk_size)
        .map(|chunk| {
            let first = &chunk[0];
            let last = &chunk[chunk.len() - 1];
            let summary = if first.summary == last.summary {
                first.summary.clone()
            } else {
                crop_chars(&format!("{} -> {}", first.summary, last.summary), 180)
            };
            DigestChapter {
                started_at: format_short_time(first.start_time),
                ended_at: format_short_time(last.end_time),
                summary,
                target: format!("session log, line {}", first.start_line),
            }
        })
        .collect()
}

fn build_jump_refs(session: &Session, latest: &TurnSummary) -> Vec<DigestRef> {
    let mut refs = vec![DigestRef {
        label: "raw transcript".to_string(),
        target: session.path.display().to_string(),
    }];
    if let Some(receipt) = &latest.latest_receipt {
        refs.push(DigestRef {
            label: "latest receipt".to_string(),
            target: format!("session log, line {}", receipt.line_no),
        });
    }
    for path in latest
        .artifact_paths
        .iter()
        .chain(latest.changed_paths.iter())
        .take(3)
    {
        refs.push(DigestRef {
            label: format!("artifact: {}", basename(path)),
            target: if path.is_absolute() || looks_like_absolute_path(&path.to_string_lossy()) {
                path.display().to_string()
            } else {
                session.meta.cwd.join(path).display().to_string()
            },
        });
    }
    refs
}

fn latest_user_objective(turns: &[TurnSummary]) -> Option<String> {
    turns.iter().rev().find_map(|turn| {
        let lower = turn.objective.to_lowercase();
        if lower == "no explicit objective recorded." {
            None
        } else {
            Some(turn.objective.clone())
        }
    })
}

fn summary_notice(recent_updates: &[DigestUpdate]) -> String {
    if recent_updates.is_empty() {
        return "fallback summaries".to_string();
    }
    let mut saw_codex = false;
    let mut saw_fallback = false;
    for update in recent_updates {
        if update.summary_source == "codex_exec" {
            saw_codex = true;
        } else {
            saw_fallback = true;
        }
    }
    match (saw_codex, saw_fallback) {
        (true, false) => "codex exec summaries".to_string(),
        (true, true) => "mixed codex exec and fallback summaries".to_string(),
        _ => "fallback summaries".to_string(),
    }
}

fn empty_turn_summary() -> TurnSummary {
    let now = chrono::Local::now().fixed_offset();
    TurnSummary {
        objective: "No explicit objective recorded.".to_string(),
        summary: "No concise summary available.".to_string(),
        status: "progressing".to_string(),
        caveat: "No decision-reversing caveat was explicit in the latest turn.".to_string(),
        blocker_or_repair: "No active blocker or repair was explicit in the latest turn."
            .to_string(),
        next_step: "No explicit next step was recorded.".to_string(),
        stop_reason: "Session still looked active at the end of the recorded turn.".to_string(),
        changed_paths: Vec::new(),
        artifact_paths: Vec::new(),
        latest_receipt: None,
        start_time: now,
        end_time: now,
        start_line: 1,
        assistant_updates: Vec::new(),
        final_answer: None,
    }
}

fn clean_objective(text: &str) -> String {
    let mut useful_lines = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        let lower = line.to_lowercase();
        let stripped = line.trim_matches('`');
        let work_in_target = lower
            .strip_prefix("work in ")
            .map(|_| line[8..].trim().trim_matches('`'));
        if line.is_empty()
            || lower == "read first:"
            || work_in_target.is_some_and(looks_like_absolute_path)
            || looks_like_absolute_path(stripped)
            || line.starts_with("`/")
        {
            continue;
        }
        useful_lines.push(line);
    }
    first_sentence_or_line(&useful_lines.join(" "))
}

fn extract_next_step(text: &str) -> Option<String> {
    for raw_line in text.lines() {
        let line = raw_line.trim().trim_start_matches('-').trim();
        let lower = line.to_lowercase();
        if lower.starts_with("if you want, i can ") {
            return Some(crop_chars(
                line.trim_start_matches("If you want, I can ").trim(),
                140,
            ));
        }
        if lower.starts_with("next ")
            || lower.starts_with("then ")
            || lower.starts_with("from here")
        {
            return Some(crop_chars(line, 140));
        }
    }
    let lower = text.to_lowercase();
    if let Some(index) = lower.find("then i'll ") {
        let candidate = &text[index + 9..];
        return Some(crop_chars(&first_sentence_or_line(candidate), 140));
    }
    None
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn status_label(status: &str) -> String {
    match status {
        "repairing" => "repairing after a failed step".to_string(),
        "waiting" => "waiting or monitoring".to_string(),
        "stopped" => "stopped after a completed turn".to_string(),
        other => other.to_string(),
    }
}

fn project_label(path: &Path) -> String {
    basename(path)
}

fn log_label(path: &Path) -> String {
    basename(path)
}

fn basename(path: &Path) -> String {
    basename_from_path_like(&path.to_string_lossy())
}

fn basename_list(paths: &[PathBuf], limit: usize) -> String {
    let mut names = paths
        .iter()
        .take(limit)
        .map(|path| basename(path))
        .collect::<Vec<_>>();
    if paths.len() > limit {
        names.push(format!("+{} more", paths.len() - limit));
    }
    names.join(", ")
}

fn parse_timestamp(raw: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(raw).ok()
}

pub(crate) fn scrub_display_paths(text: &str) -> String {
    use regex::Regex;
    static LINKS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let links = LINKS.get_or_init(|| Regex::new(r"\[([^\]\n]+)\]\(([^)\n]+)\)").unwrap());
    let text = links.replace_all(text, |captures: &regex::Captures<'_>| {
        if looks_like_absolute_path(captures[2].trim_matches(['<', '>'])) {
            captures[1].to_string()
        } else {
            captures[0].to_string()
        }
    });
    text.split_inclusive(char::is_whitespace)
        .map(|token| {
            let trimmed = token
                .trim()
                .trim_matches(['`', ',', '.', ')', ']', '"', '\'']);
            if looks_like_absolute_path(trimmed) {
                return token.replace(trimmed, &basename_from_path_like(trimmed));
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const FIXTURE: &str = include_str!("../tests/fixtures/sample_rollout.jsonl");

    #[test]
    fn native_digest_renders_fixture_sections() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{FIXTURE}").unwrap();

        let session = load_session(file.path()).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let digest = build_digest_with_client(
            &session,
            &CodexSummaryClient::offline(cache.path().to_path_buf()),
        );
        let rendered = render_pretty(&digest);

        assert!(rendered.contains("Headline"));
        assert!(rendered.contains("Rolling state"));
        assert!(rendered.contains("Recent updates"));
        assert!(rendered.contains("Chaptered timeline"));
        assert!(rendered.contains("Jump refs"));
        assert!(rendered.contains("Latest answer"));
        assert_eq!(digest.objective, digest.current_slice);
        assert!(
            digest
                .what_changed
                .contains("Reported: Parser and timeline tests are green.")
        );
        assert_eq!(
            digest.jump_refs[0].target,
            file.path().display().to_string()
        );
    }

    #[test]
    fn answer_path_labels_preserve_paragraphs() {
        let original = "Changed [watch.rs](/Users/person/project/src/watch.rs:10).\n\nTests pass.\nOpen `C:\\Users\\person\\result.txt`.";
        assert_eq!(
            scrub_display_paths(original),
            "Changed watch.rs.\n\nTests pass.\nOpen `result.txt`."
        );
    }

    #[test]
    fn watch_and_digest_share_summary_cache_keys() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{FIXTURE}").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let client = CodexSummaryClient::offline(cache.path().to_path_buf());
        let watch = crate::codex_rollout::read_recent_updates(file.path(), 30, 64).unwrap();
        let summaries = watch
            .updates
            .iter()
            .map(|update| {
                let key = update_key(update);
                (
                    key.clone(),
                    UpdateSummary {
                        key,
                        summary: "Cached watch summary".into(),
                        source: "codex_exec".into(),
                    },
                )
            })
            .collect();
        client.store_summaries(file.path(), &summaries).unwrap();
        let session = load_session(file.path()).unwrap();
        let digest = build_digest_with_client(&session, &client);
        assert_eq!(digest.recent_updates.len(), watch.updates.len());
        assert!(
            digest
                .recent_updates
                .iter()
                .all(|update| update.summary == "Cached watch summary"
                    && update.summary_source == "codex_exec")
        );
    }

    #[test]
    fn timeline_keeps_chapters_bounded_and_ordered() {
        let turns = (1..=100)
            .map(|line| {
                let mut turn = empty_turn_summary();
                turn.start_line = line;
                turn.summary = format!("Update {line}");
                turn
            })
            .collect::<Vec<_>>();
        let chapters = build_timeline(&turns);
        assert!(chapters.len() <= TIMELINE_TARGET);
        assert!(chapters.first().unwrap().summary.starts_with("Update 1"));
        assert!(chapters.last().unwrap().summary.ends_with("Update 100"));
        assert_eq!(chapters.first().unwrap().target, "session log, line 1");
    }
}
