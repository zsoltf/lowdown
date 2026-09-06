use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::codex_rollout::{RecentUpdatesBootstrap, read_recent_updates};
use crate::digest::{build_digest, load_session, render_pretty as render_digest_pretty};
use crate::locator::{discover_latest_session, discover_repo_sessions, resolve_session_path};
use crate::text::{first_sentence_or_line, scanline_summary};
use crate::time_display::format_short_time;
use crate::watch::{WatchConfig, run_watch};

#[derive(Parser, Debug)]
#[command(
    name = "lowdown",
    version,
    about = "Native watch and digest surfaces for Lowdown Codex rollout sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    Digest(DigestArgs),
    RecentUpdates(RecentUpdatesArgs),
    Watch(WatchArgs),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Pretty,
    Json,
}

#[derive(Args, Debug, Clone)]
struct SourceArgs {
    #[arg(long, visible_alias = "session-dir", conflicts_with = "repo_root")]
    session: Option<PathBuf>,
    #[arg(long)]
    repo_root: Option<PathBuf>,
    #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(i64).range(1..=36500))]
    lookback_days: i64,
    #[arg(long, default_value_t = true, hide = true)]
    latest: bool,
}

impl Default for SourceArgs {
    fn default() -> Self {
        Self {
            session: None,
            repo_root: None,
            lookback_days: 7,
            latest: true,
        }
    }
}

#[derive(Args, Debug, Clone)]
struct DigestArgs {
    #[command(flatten)]
    source: SourceArgs,
    #[arg(long, default_value_t = false)]
    follow: bool,
    #[arg(long, default_value_t = false)]
    pretty: bool,
    #[arg(long, default_value_t = 2.0, value_parser = parse_poll_seconds)]
    poll_seconds: f64,
}

#[derive(Args, Debug)]
struct RecentUpdatesArgs {
    #[command(flatten)]
    source: SourceArgs,
    #[arg(long, default_value_t = 30)]
    target_updates: usize,
    #[arg(long, default_value_t = 64)]
    boundary_lookback_lines: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Pretty)]
    format: OutputFormat,
}

#[derive(Args, Debug, Clone)]
struct WatchArgs {
    #[command(flatten)]
    source: SourceArgs,
    #[arg(long, default_value_t = 30)]
    target_updates: usize,
    #[arg(long, default_value_t = 64)]
    boundary_lookback_lines: usize,
    #[arg(long, default_value_t = 1.5, value_parser = parse_poll_seconds)]
    poll_seconds: f64,
}

impl Default for WatchArgs {
    fn default() -> Self {
        Self {
            source: SourceArgs::default(),
            target_updates: 30,
            boundary_lookback_lines: 64,
            poll_seconds: 1.5,
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Watch(WatchArgs::default())) {
        Command::Digest(args) => {
            run_digest(args)?;
        }
        Command::RecentUpdates(args) => {
            let session_path = resolve_source(&args.source)?;
            let slice = read_recent_updates(
                &session_path,
                args.target_updates,
                args.boundary_lookback_lines,
            )?;
            match args.format {
                OutputFormat::Pretty => {
                    print!("{}", render_recent_updates(&slice));
                }
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&slice)?);
                }
            }
        }
        Command::Watch(args) => {
            let session_paths = resolve_watch_sessions(&args.source)?;
            run_watch(WatchConfig {
                session_paths,
                selected_session_index: 0,
                poll_interval: Duration::from_secs_f64(args.poll_seconds.max(0.2)),
                target_updates: args.target_updates,
                boundary_lookback_lines: args.boundary_lookback_lines,
            })?;
        }
    }
    Ok(())
}

fn run_digest(args: DigestArgs) -> Result<()> {
    render_digest_once(&args)?;
    if !args.follow {
        return Ok(());
    }

    let poll_interval = Duration::from_secs_f64(args.poll_seconds.max(0.2));
    let mut previous_snapshot = digest_snapshot(&resolve_source(&args.source)?)?;
    loop {
        std::thread::sleep(poll_interval);
        let session_path = resolve_source(&args.source)?;
        let snapshot = digest_snapshot(&session_path)?;
        if snapshot != previous_snapshot {
            println!();
            render_digest_from_path(&session_path, &args)?;
            previous_snapshot = snapshot;
        }
    }
}

fn render_digest_once(args: &DigestArgs) -> Result<()> {
    let session_path = resolve_source(&args.source)?;
    render_digest_from_path(&session_path, args)
}

fn render_digest_from_path(session_path: &Path, args: &DigestArgs) -> Result<()> {
    let session = load_session(session_path)?;
    let digest = build_digest(&session);
    if args.pretty {
        print!("{}", render_digest_pretty(&digest));
    } else {
        println!("{}", serde_json::to_string_pretty(&digest)?);
    }
    Ok(())
}

fn parse_poll_seconds(raw: &str) -> std::result::Result<f64, String> {
    let value = raw
        .parse::<f64>()
        .map_err(|_| "poll seconds must be a number")?;
    if !value.is_finite() || !(0.2..=3600.0).contains(&value) {
        return Err("poll seconds must be between 0.2 and 3600".to_string());
    }
    Ok(value)
}

fn resolve_source(source: &SourceArgs) -> Result<PathBuf> {
    if let Some(session) = &source.session {
        return resolve_session_path(session);
    }
    let repo_root = source.repo_root.clone().unwrap_or(cwd()?);
    discover_latest_session(&repo_root, source.lookback_days)
}

fn resolve_watch_sessions(source: &SourceArgs) -> Result<Vec<PathBuf>> {
    if let Some(session) = &source.session {
        return Ok(vec![resolve_session_path(session)?]);
    }
    let repo_root = source.repo_root.clone().unwrap_or(cwd()?);
    let discovered = discover_repo_sessions(&repo_root, source.lookback_days)?;
    Ok(filter_watch_sessions(discovered))
}

fn filter_watch_sessions(session_paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let filtered = session_paths
        .iter()
        .filter_map(|path| match read_recent_updates(path, 1, 64) {
            Ok(bootstrap) if !bootstrap.updates.is_empty() => Some(path.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        session_paths
    } else {
        filtered
    }
}

fn cwd() -> Result<PathBuf> {
    Ok(env::current_dir()?)
}

fn digest_snapshot(session_path: &PathBuf) -> Result<(u64, Option<std::time::SystemTime>)> {
    let metadata = std::fs::metadata(session_path)?;
    Ok((metadata.len(), metadata.modified().ok()))
}

fn render_recent_updates(slice: &RecentUpdatesBootstrap) -> String {
    let mut out = String::new();
    out.push_str("LOWDOWN RECENT UPDATES\n");
    out.push_str(&format!(
        "Contract: {} v{}\n",
        slice.slice_type, slice.schema_version
    ));
    out.push_str(&format!("Session: {}\n", slice.session_path.display()));
    out.push_str(&format!("Cwd: {}\n", slice.cwd.display()));
    out.push_str(&format!(
        "Updates: {}/{}\n",
        slice.updates.len(),
        slice.target_updates
    ));
    out.push_str(&format!(
        "Tail lines scanned: {}\n",
        slice.scanned_complete_lines
    ));
    out.push_str(&format!(
        "Malformed lines skipped: {}\n",
        slice.malformed_lines_skipped
    ));
    if let Some(answer) = &slice.latest_final_answer {
        out.push_str(&format!(
            "Latest answer: {} [{}]\n",
            first_sentence_or_line(&answer.text),
            format_short_time(answer.timestamp)
        ));
    }
    out.push('\n');
    for update in &slice.updates {
        out.push_str(&format!(
            "{}  {}\n",
            format_short_time(update.timestamp),
            scanline_summary(&update.text, 8, 56)
        ));
    }
    out
}
