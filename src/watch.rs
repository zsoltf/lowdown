use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::codex_rollout::{RecentMessage, RecentUpdatesBootstrap, read_recent_updates};
use crate::digest::scrub_display_paths;
use crate::summaries::{CodexSummaryClient, UpdateSummary, update_key};
use crate::text::{crop_chars, first_sentence_or_line};
use crate::time_display::format_short_time;

#[derive(Clone, Debug)]
pub struct WatchConfig {
    pub session_paths: Vec<PathBuf>,
    pub selected_session_index: usize,
    pub poll_interval: Duration,
    pub target_updates: usize,
    pub boundary_lookback_lines: usize,
}

#[derive(Debug)]
enum RefreshMessage {
    Snapshot(RecentUpdatesBootstrap),
    Error {
        session_path: PathBuf,
        message: String,
    },
}

#[derive(Clone, Debug)]
struct SummaryRequest {
    session_token: u64,
    session_path: PathBuf,
    updates: Vec<RecentMessage>,
}

#[derive(Debug)]
enum SummaryMessage {
    Cached {
        session_token: u64,
        summaries: HashMap<String, UpdateSummary>,
    },
    Hydrated {
        session_token: u64,
        keys: Vec<String>,
        summaries: HashMap<String, UpdateSummary>,
    },
    Complete {
        session_token: u64,
        keys: Vec<String>,
    },
    Failed {
        session_token: u64,
        keys: Vec<String>,
        message: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum WatchAction {
    Continue,
    Quit,
    SwitchSession(isize),
}

#[derive(Debug)]
struct WatchApp {
    bootstrap: RecentUpdatesBootstrap,
    selected_index: usize,
    updates_offset: usize,
    follow_latest: bool,
    selected_expanded: bool,
    answer_expanded: bool,
    wrap_updates: bool,
    selected_scroll: u16,
    answer_scroll: u16,
    session_paths: Vec<PathBuf>,
    current_session_index: usize,
    session_token: u64,
    updates_page_size: usize,
    selected_page_size: u16,
    answer_page_size: u16,
    summary_by_key: HashMap<String, UpdateSummary>,
    summary_inflight_keys: HashSet<String>,
    summary_failed_keys: HashSet<String>,
    summary_error: Option<String>,
    refresh_error: Option<String>,
    recent_sessions: VecDeque<(RecentUpdatesBootstrap, HashMap<String, UpdateSummary>)>,
}

pub fn run_watch(config: WatchConfig) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("watch requires a TTY; use lowdown digest --pretty for text output");
    }
    if config.session_paths.is_empty() {
        bail!("watch requires at least one resolved session path");
    }

    let selected_session_index = config
        .selected_session_index
        .min(config.session_paths.len().saturating_sub(1));
    let current_session_path = config.session_paths[selected_session_index].clone();
    let initial = read_recent_updates(
        &current_session_path,
        config.target_updates,
        config.boundary_lookback_lines,
    )?;
    let mut app = WatchApp::new(
        initial,
        config.session_paths.clone(),
        selected_session_index,
    );

    let (refresh_tx, refresh_rx) = mpsc::channel();
    let (summary_req_tx, summary_req_rx) = mpsc::channel();
    let (model_req_tx, model_req_rx) = mpsc::channel();
    let (summary_tx, summary_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let shared_session_path = Arc::new(Mutex::new((current_session_path, config.target_updates)));

    let refresh_worker = spawn_refresh_worker(
        shared_session_path.clone(),
        config.poll_interval,
        config.boundary_lookback_lines,
        refresh_tx,
        stop.clone(),
    );
    let cache_worker = spawn_cache_worker(
        summary_req_rx,
        summary_tx.clone(),
        model_req_tx,
        stop.clone(),
        CodexSummaryClient::new(),
    );
    let summary_worker = spawn_summary_worker(model_req_rx, summary_tx, stop.clone());

    let mut terminal = match init_terminal() {
        Ok(terminal) => terminal,
        Err(error) => {
            stop.store(true, Ordering::Relaxed);
            let _ = refresh_worker.join();
            let _ = cache_worker.join();
            let _ = summary_worker.join();
            return Err(error);
        }
    };
    let result = run_watch_loop(
        &mut terminal,
        &mut app,
        shared_session_path,
        config.target_updates,
        refresh_rx,
        summary_rx,
        &summary_req_tx,
    );
    let restored = restore_terminal(&mut terminal);
    stop.store(true, Ordering::Relaxed);
    let _ = refresh_worker.join();
    let _ = cache_worker.join();
    let _ = summary_worker.join();
    result.and(restored)
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        previous(info);
    }));
    let result = (|| {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
        Terminal::new(CrosstermBackend::new(stdout)).context("create terminal")
    })();
    if result.is_err() {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
    result
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;
    Ok(())
}

fn spawn_refresh_worker(
    session_path: Arc<Mutex<(PathBuf, usize)>>,
    poll_interval: Duration,
    boundary_lookback_lines: usize,
    tx: mpsc::Sender<RefreshMessage>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut last_sent: Option<RecentUpdatesBootstrap> = None;
        let mut last_error: Option<(PathBuf, String)> = None;
        let mut last_signature = None;

        while !stop.load(Ordering::Relaxed) {
            let (current_session_path, target_updates) = match session_path.lock() {
                Ok(guard) => guard.clone(),
                Err(_) => break,
            };
            let signature = std::fs::metadata(&current_session_path).ok().map(|meta| {
                (
                    current_session_path.clone(),
                    target_updates,
                    meta.len(),
                    meta.modified().ok(),
                )
            });
            if signature.is_some() && signature == last_signature {
                wait_for_refresh(
                    poll_interval,
                    &session_path,
                    &current_session_path,
                    target_updates,
                    &stop,
                );
                continue;
            }
            match read_recent_updates(
                &current_session_path,
                target_updates,
                boundary_lookback_lines,
            ) {
                Ok(snapshot) => {
                    if last_sent.as_ref() != Some(&snapshot) {
                        if tx.send(RefreshMessage::Snapshot(snapshot.clone())).is_err() {
                            break;
                        }
                        last_sent = Some(snapshot);
                    }
                    last_error = None;
                    last_signature = signature;
                }
                Err(error) => {
                    let message = format!("refresh error: {error}");
                    let signature = (current_session_path.clone(), message.clone());
                    if last_error.as_ref() != Some(&signature) {
                        if tx
                            .send(RefreshMessage::Error {
                                session_path: current_session_path.clone(),
                                message: message.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        last_error = Some(signature);
                    }
                }
            }
            wait_for_refresh(
                poll_interval,
                &session_path,
                &current_session_path,
                target_updates,
                &stop,
            );
        }
    })
}

fn wait_for_refresh(
    interval: Duration,
    requested: &Mutex<(PathBuf, usize)>,
    current_path: &Path,
    current_target: usize,
    stop: &AtomicBool,
) {
    let deadline = std::time::Instant::now() + interval;
    while !stop.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
        if requested
            .lock()
            .is_ok_and(|request| request.0 != current_path || request.1 != current_target)
        {
            break;
        }
        thread::sleep(Duration::from_millis(40));
    }
}

// Disk reads cannot hold up terminal input, transcript refresh, or cache hits
// for the duration of an unrelated model batch.
fn spawn_cache_worker(
    rx: mpsc::Receiver<SummaryRequest>,
    tx: mpsc::Sender<SummaryMessage>,
    model_tx: mpsc::Sender<SummaryRequest>,
    stop: Arc<AtomicBool>,
    client: CodexSummaryClient,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let mut request = match rx.recv_timeout(Duration::from_millis(120)) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            while let Ok(newer) = rx.try_recv() {
                request = prioritize_summary_request(request, newer);
            }
            let summaries = client.cached_for_updates(&request.session_path, &request.updates);
            request
                .updates
                .retain(|update| !summaries.contains_key(&update_key(update)));
            if tx
                .send(SummaryMessage::Cached {
                    session_token: request.session_token,
                    summaries,
                })
                .is_err()
            {
                break;
            }
            if !request.updates.is_empty() && model_tx.send(request).is_err() {
                break;
            }
        }
    })
}

fn spawn_summary_worker(
    rx: mpsc::Receiver<SummaryRequest>,
    tx: mpsc::Sender<SummaryMessage>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let client = CodexSummaryClient::new().with_cancel(stop.clone());
        let mut pending = None;

        while !stop.load(Ordering::Relaxed) {
            let mut request = match pending.take() {
                Some(request) => request,
                None => match rx.recv_timeout(Duration::from_millis(120)) {
                    Ok(request) => request,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
            };
            // Reconsider new work between batches, not after the whole history.
            while let Ok(newer) = rx.try_recv() {
                request = prioritize_summary_request(request, newer);
            }

            let keys = request.updates.iter().map(update_key).collect::<Vec<_>>();

            let cached = client.cached_for_updates(&request.session_path, &request.updates);
            if !cached.is_empty()
                && tx
                    .send(SummaryMessage::Cached {
                        session_token: request.session_token,
                        summaries: cached.clone(),
                    })
                    .is_err()
            {
                break;
            }

            let mut missing = request
                .updates
                .into_iter()
                .filter(|update| !cached.contains_key(&update_key(update)))
                .collect::<Vec<_>>();

            if missing.is_empty() {
                if tx
                    .send(SummaryMessage::Complete {
                        session_token: request.session_token,
                        keys,
                    })
                    .is_err()
                {
                    break;
                }
                continue;
            }

            if !client.available() {
                if tx
                    .send(SummaryMessage::Failed {
                        session_token: request.session_token,
                        keys,
                        message: "codex exec unavailable".to_string(),
                    })
                    .is_err()
                {
                    break;
                }
                continue;
            }

            if missing.len() > 8 {
                pending = Some(SummaryRequest {
                    session_token: request.session_token,
                    session_path: request.session_path.clone(),
                    updates: missing.split_off(8),
                });
            }
            {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let batch = missing.as_slice();
                let keys = batch.iter().map(update_key).collect::<Vec<_>>();
                match client.summarize_updates(&request.session_path, batch) {
                    Ok(summaries) => {
                        if tx
                            .send(SummaryMessage::Hydrated {
                                session_token: request.session_token,
                                keys,
                                summaries,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        if tx
                            .send(SummaryMessage::Failed {
                                session_token: request.session_token,
                                keys,
                                message: format!("summary fallback: {error}"),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    })
}

fn prioritize_summary_request(older: SummaryRequest, mut newer: SummaryRequest) -> SummaryRequest {
    if older.session_token == newer.session_token {
        newer.updates.extend(older.updates);
    }
    newer
}

fn run_watch_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut WatchApp,
    shared_session_path: Arc<Mutex<(PathBuf, usize)>>,
    target_updates: usize,
    refresh_rx: mpsc::Receiver<RefreshMessage>,
    summary_rx: mpsc::Receiver<SummaryMessage>,
    summary_req_tx: &mpsc::Sender<SummaryRequest>,
) -> Result<()> {
    loop {
        while let Ok(message) = refresh_rx.try_recv() {
            match message {
                RefreshMessage::Snapshot(snapshot) => {
                    if snapshot.session_path != app.current_session_path() {
                        continue;
                    }
                    app.refresh_error = None;
                    app.apply_snapshot(snapshot);
                }
                RefreshMessage::Error {
                    session_path,
                    message,
                } => {
                    if session_path == app.current_session_path() {
                        app.refresh_error = Some(message);
                    }
                }
            }
        }

        while let Ok(message) = summary_rx.try_recv() {
            app.apply_summary_message(message);
        }

        terminal.draw(|frame| app.render(frame))?;
        // Rendering establishes the visible rows before we prioritize model work.
        if let Some(request) = app.build_summary_request() {
            let _ = summary_req_tx.send(request);
        }

        if !event::poll(Duration::from_millis(120)).context("poll terminal event")? {
            continue;
        }

        match event::read().context("read terminal event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match app.handle_key(key) {
                WatchAction::Quit => break,
                WatchAction::SwitchSession(delta) => {
                    if let Some(next_index) = app.switchable_session_index(delta) {
                        let next_path = app.session_paths[next_index].clone();
                        let mut bootstrap = app.bootstrap.clone();
                        bootstrap.session_path = next_path.clone();
                        bootstrap.updates.clear();
                        bootstrap.latest_final_answer = None;
                        bootstrap.target_updates = target_updates;
                        app.switch_session(bootstrap, next_index);
                        if let Ok(mut guard) = shared_session_path.lock() {
                            *guard = (next_path, app.bootstrap.target_updates);
                        }
                    }
                }
                WatchAction::Continue => {
                    if matches!(key.code, KeyCode::Up | KeyCode::PageUp | KeyCode::Char('k'))
                        && !app.selected_expanded
                        && !app.answer_expanded
                        && app.selected_index == 0
                        && app.bootstrap.updates.len() >= app.bootstrap.target_updates
                        && let Ok(mut guard) = shared_session_path.lock()
                    {
                        guard.1 = app.bootstrap.target_updates.saturating_add(30);
                    }
                }
            },
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    Ok(())
}

impl WatchApp {
    fn new(
        bootstrap: RecentUpdatesBootstrap,
        session_paths: Vec<PathBuf>,
        current_session_index: usize,
    ) -> Self {
        let selected_index = bootstrap.updates.len().saturating_sub(1);
        Self {
            bootstrap,
            selected_index,
            updates_offset: 0,
            follow_latest: true,
            selected_expanded: false,
            answer_expanded: false,
            wrap_updates: false,
            selected_scroll: 0,
            answer_scroll: 0,
            session_paths,
            current_session_index,
            session_token: 0,
            updates_page_size: 8,
            selected_page_size: 4,
            answer_page_size: 4,
            summary_by_key: HashMap::new(),
            summary_inflight_keys: HashSet::new(),
            summary_failed_keys: HashSet::new(),
            summary_error: None,
            refresh_error: None,
            recent_sessions: VecDeque::new(),
        }
    }

    fn apply_snapshot(&mut self, bootstrap: RecentUpdatesBootstrap) {
        let selected_offset = self.selected_update().map(|update| update.offset);
        self.bootstrap = bootstrap;
        let new_len = self.bootstrap.updates.len();

        if new_len == 0 {
            self.selected_index = 0;
            self.updates_offset = 0;
            self.follow_latest = true;
        } else if self.follow_latest {
            self.selected_index = new_len - 1;
            self.ensure_selected_visible();
        } else {
            self.selected_index = self
                .bootstrap
                .updates
                .iter()
                .position(|update| Some(update.offset) == selected_offset)
                .unwrap_or(0);
            self.ensure_selected_visible();
        }

        let current_keys = self.current_update_keys();
        self.summary_by_key
            .retain(|key, _| current_keys.contains(key.as_str()));
        self.summary_inflight_keys
            .retain(|key| current_keys.contains(key.as_str()));
        self.summary_failed_keys
            .retain(|key| current_keys.contains(key.as_str()));
        if self.summary_failed_keys.is_empty() {
            self.summary_error = None;
        }
    }

    fn build_summary_request(&mut self) -> Option<SummaryRequest> {
        let mut updates = Vec::new();
        let start = self.updates_offset.min(self.bootstrap.updates.len());
        let end = (start + self.updates_page_size.max(1)).min(self.bootstrap.updates.len());
        for update in self.bootstrap.updates[start..end]
            .iter()
            .rev()
            .chain(self.bootstrap.updates[end..].iter().rev())
            .chain(self.bootstrap.updates[..start].iter().rev())
        {
            let key = update_key(update);
            if self.summary_by_key.contains_key(&key)
                || self.summary_inflight_keys.contains(&key)
                || self.summary_failed_keys.contains(&key)
            {
                continue;
            }
            self.summary_inflight_keys.insert(key);
            updates.push(update.clone());
        }

        if updates.is_empty() {
            return None;
        }

        Some(SummaryRequest {
            session_token: self.session_token,
            session_path: self.bootstrap.session_path.clone(),
            updates,
        })
    }

    fn apply_summary_message(&mut self, message: SummaryMessage) {
        match message {
            SummaryMessage::Cached {
                session_token,
                summaries,
            } if session_token == self.session_token => {
                for (key, summary) in summaries {
                    self.summary_inflight_keys.remove(&key);
                    self.summary_failed_keys.remove(&key);
                    self.summary_by_key.insert(key, summary);
                }
            }
            SummaryMessage::Hydrated {
                session_token,
                keys,
                summaries,
            } if session_token == self.session_token => {
                for key in keys {
                    self.summary_inflight_keys.remove(&key);
                    self.summary_failed_keys.remove(&key);
                }
                for (key, summary) in summaries {
                    self.summary_by_key.insert(key, summary);
                }
                self.summary_error = None;
            }
            SummaryMessage::Complete {
                session_token,
                keys,
            } if session_token == self.session_token => {
                for key in keys {
                    self.summary_inflight_keys.remove(&key);
                    self.summary_failed_keys.remove(&key);
                }
                self.summary_error = None;
            }
            SummaryMessage::Failed {
                session_token,
                keys,
                message,
            } if session_token == self.session_token => {
                for key in keys {
                    self.summary_inflight_keys.remove(&key);
                    if !self.summary_by_key.contains_key(&key) {
                        self.summary_failed_keys.insert(key);
                    }
                }
                self.summary_error = Some(message);
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> WatchAction {
        match key.code {
            KeyCode::Char('q') => return WatchAction::Quit,
            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                return WatchAction::Quit;
            }
            KeyCode::Char('p') => self.wrap_updates = !self.wrap_updates,
            KeyCode::Char('h') => return WatchAction::SwitchSession(1),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('l') => return WatchAction::SwitchSession(-1),
            KeyCode::PageDown => self.page_down(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::Char('i') => {
                self.selected_expanded = !self.selected_expanded;
                self.selected_scroll = 0;
            }
            KeyCode::Char('o') => {
                self.answer_expanded = !self.answer_expanded;
                self.answer_scroll = 0;
            }
            _ => {}
        }
        WatchAction::Continue
    }

    fn current_session_path(&self) -> PathBuf {
        self.session_paths[self.current_session_index].clone()
    }

    fn thread_position_label(&self) -> Option<String> {
        (self.session_paths.len() > 1).then(|| {
            format!(
                "thread {}/{}",
                self.current_session_index + 1,
                self.session_paths.len()
            )
        })
    }

    fn switchable_session_index(&self, delta: isize) -> Option<usize> {
        let target = self.current_session_index as isize + delta;
        (0..self.session_paths.len() as isize)
            .contains(&target)
            .then_some(target as usize)
    }

    fn switch_session(&mut self, bootstrap: RecentUpdatesBootstrap, next_index: usize) {
        let cached = self
            .recent_sessions
            .iter()
            .position(|(snapshot, _)| snapshot.session_path == bootstrap.session_path)
            .and_then(|index| self.recent_sessions.remove(index));
        let (bootstrap, summaries) = cached.unwrap_or_else(|| (bootstrap, HashMap::new()));
        let previous = std::mem::replace(&mut self.bootstrap, bootstrap);
        let previous_summaries = std::mem::replace(&mut self.summary_by_key, summaries);
        self.recent_sessions
            .push_back((previous, previous_summaries));
        // Retain a small working set, not every session's expanded history.
        if self.recent_sessions.len() > 8 {
            self.recent_sessions.pop_front();
        }
        self.current_session_index = next_index;
        self.session_token = self.session_token.saturating_add(1);
        self.summary_inflight_keys.clear();
        self.summary_failed_keys.clear();
        self.summary_error = None;
        self.refresh_error = None;
        self.selected_scroll = 0;
        self.answer_scroll = 0;
        self.follow_latest = true;
        self.updates_offset = 0;
        self.selected_index = self.bootstrap.updates.len().saturating_sub(1);
        self.ensure_selected_visible();
    }

    fn move_selection(&mut self, delta: isize) {
        if self.bootstrap.updates.is_empty() {
            return;
        }
        let max = self.bootstrap.updates.len().saturating_sub(1) as isize;
        let next = (self.selected_index as isize + delta).clamp(0, max) as usize;
        self.selected_index = next;
        self.follow_latest = self.selected_index + 1 == self.bootstrap.updates.len();
        self.selected_scroll = 0;
        self.ensure_selected_visible();
    }

    fn page_down(&mut self) {
        if self.selected_expanded {
            self.selected_scroll = self
                .selected_scroll
                .saturating_add(self.selected_page_size.max(1));
            return;
        }
        if self.answer_expanded {
            self.answer_scroll = self
                .answer_scroll
                .saturating_add(self.answer_page_size.max(1));
            return;
        }
        let step = self.updates_page_size.max(1) as isize;
        self.move_selection(step);
    }

    fn page_up(&mut self) {
        if self.selected_expanded {
            self.selected_scroll = self
                .selected_scroll
                .saturating_sub(self.selected_page_size.max(1));
            return;
        }
        if self.answer_expanded {
            self.answer_scroll = self
                .answer_scroll
                .saturating_sub(self.answer_page_size.max(1));
            return;
        }
        let step = self.updates_page_size.max(1) as isize;
        self.move_selection(-step);
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let [selected_area, updates_area, answer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints(layout_constraints(
                frame.area().height,
                self.selected_expanded,
                self.answer_expanded,
            ))
            .areas(frame.area());

        self.updates_page_size = updates_area.height.saturating_sub(2) as usize;
        if self.wrap_updates {
            self.updates_page_size /= 2;
        }
        self.selected_page_size = selected_area.height.saturating_sub(3);
        self.answer_page_size = answer_area.height.saturating_sub(3);

        let selected_lines = self
            .selected_panel(selected_area)
            .line_count(selected_area.width)
            .saturating_sub(selected_area.height.saturating_sub(2) as usize);
        let answer_lines = self
            .answer_panel(answer_area)
            .line_count(answer_area.width)
            .saturating_sub(answer_area.height.saturating_sub(2) as usize);
        self.selected_scroll = self
            .selected_scroll
            .min(selected_lines.min(u16::MAX as usize) as u16);
        self.answer_scroll = self
            .answer_scroll
            .min(answer_lines.min(u16::MAX as usize) as u16);
        frame.render_widget(self.selected_panel(selected_area), selected_area);
        let mut list_state = self.list_state();
        frame.render_stateful_widget(
            self.updates_panel(updates_area),
            updates_area,
            &mut list_state,
        );
        self.updates_offset = list_state.offset();
        frame.render_widget(self.answer_panel(answer_area), answer_area);
    }

    fn selected_panel(&self, area: Rect) -> Paragraph<'static> {
        let body = if let Some(update) = self.selected_update() {
            update.text.clone()
        } else {
            "No commentary updates in this session yet.".to_string()
        };
        let subtitle = if let Some(update) = self.selected_update() {
            let mut parts = vec![
                format_short_time(update.timestamp),
                format!(
                    "{}/{}",
                    self.selected_index + 1,
                    self.bootstrap.updates.len()
                ),
                self.summary_mode().to_string(),
                self.follow_mode().to_string(),
            ];
            if self.refresh_error.is_some() {
                parts.push("REFRESH".to_string());
            }
            if let Some(label) = self.thread_position_label() {
                parts.push(label);
            }
            parts.join(" | ")
        } else {
            self.summary_mode().to_string()
        };

        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Selected update")
                    .title_bottom(Line::from(crop_chars(
                        &subtitle,
                        area.width.saturating_sub(4) as usize,
                    ))),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.selected_scroll, 0))
    }

    fn updates_panel(&self, area: Rect) -> List<'static> {
        let inner_width = area.width.saturating_sub(5) as usize;
        let items = if self.bootstrap.updates.is_empty() {
            vec![ListItem::new(Line::from("No commentary updates yet."))]
        } else {
            self.bootstrap
                .updates
                .iter()
                .map(|update| {
                    let line = format!(
                        "{}  {}",
                        format_short_time(update.timestamp),
                        self.render_update_scanline(
                            update,
                            if self.wrap_updates {
                                usize::MAX
                            } else {
                                inner_width
                            }
                        )
                    );
                    if self.wrap_updates && inner_width > 0 {
                        let lines = textwrap::wrap(&line, inner_width)
                            .into_iter()
                            .take(2)
                            .map(|line| Line::from(line.into_owned()))
                            .collect::<Vec<_>>();
                        ListItem::new(lines)
                    } else {
                        ListItem::new(Line::from(crop_chars(&line, inner_width)))
                    }
                })
                .collect::<Vec<_>>()
        };

        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Recent updates  j/k move  p wrap")
                    .title_bottom(Line::from(crop_chars(
                        &format!(
                            "{}/{}",
                            if self.bootstrap.updates.is_empty() {
                                0
                            } else {
                                self.selected_index.saturating_add(1)
                            },
                            self.bootstrap.updates.len()
                        ),
                        area.width.saturating_sub(4) as usize,
                    ))),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
            .highlight_symbol(">> ")
            .repeat_highlight_symbol(true)
    }

    fn answer_panel(&self, area: Rect) -> Paragraph<'static> {
        let body = if let Some(answer) = &self.bootstrap.latest_final_answer {
            if self.answer_expanded {
                scrub_display_paths(&answer.text)
            } else {
                first_sentence_or_line(&scrub_display_paths(&answer.text))
            }
        } else {
            "No final answer yet.".to_string()
        };

        let subtitle = if let Some(answer) = &self.bootstrap.latest_final_answer {
            format!(
                "{} | {}",
                format_short_time(answer.timestamp),
                if self.answer_expanded {
                    "o collapse"
                } else {
                    "o expand"
                }
            )
        } else {
            "waiting for final answer".to_string()
        };

        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Latest answer")
                    .title_bottom(Line::from(crop_chars(
                        &subtitle,
                        area.width.saturating_sub(4) as usize,
                    ))),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.answer_scroll, 0))
    }

    fn selected_update(&self) -> Option<&RecentMessage> {
        self.bootstrap.updates.get(self.selected_index)
    }

    fn list_state(&self) -> ListState {
        let mut state = ListState::default().with_offset(self.updates_offset);
        if !self.bootstrap.updates.is_empty() {
            state.select(Some(self.selected_index));
        }
        state
    }

    fn ensure_selected_visible(&mut self) {
        if self.bootstrap.updates.is_empty() {
            self.updates_offset = 0;
            return;
        }
        let page = self.updates_page_size.max(1);
        if self.selected_index < self.updates_offset {
            self.updates_offset = self.selected_index;
        } else if self.selected_index >= self.updates_offset.saturating_add(page) {
            self.updates_offset = self.selected_index.saturating_sub(page.saturating_sub(1));
        }
    }

    fn render_update_scanline(&self, update: &RecentMessage, max_chars: usize) -> String {
        let key = update_key(update);
        let summary = self
            .summary_by_key
            .get(&key)
            .map(|summary| summary.summary.clone())
            .unwrap_or_else(|| update.text.split_whitespace().collect::<Vec<_>>().join(" "));
        crop_chars(&summary, max_chars)
    }

    fn summary_mode(&self) -> &'static str {
        let current_keys = self.current_update_keys();
        let total = current_keys.len();
        if total == 0 {
            return "RAW";
        }

        let summarized = current_keys
            .iter()
            .filter(|key| self.summary_by_key.contains_key((*key).as_str()))
            .count();
        let inflight = current_keys
            .iter()
            .filter(|key| self.summary_inflight_keys.contains((*key).as_str()))
            .count();
        let failed = current_keys
            .iter()
            .filter(|key| self.summary_failed_keys.contains((*key).as_str()))
            .count();

        if inflight > 0 {
            return "HYDRATING";
        }
        if summarized == total {
            return "CODEX";
        }
        if summarized > 0 {
            return "MIXED";
        }
        if failed > 0 {
            return "FALLBACK";
        }
        "RAW"
    }

    fn follow_mode(&self) -> &'static str {
        if self.follow_latest { "LIVE" } else { "SCROLL" }
    }

    fn current_update_keys(&self) -> HashSet<String> {
        self.bootstrap.updates.iter().map(update_key).collect()
    }
}

fn layout_constraints(
    total_height: u16,
    selected_expanded: bool,
    answer_expanded: bool,
) -> [Constraint; 3] {
    let min_updates = 8u16;
    let mut selected = if selected_expanded { 11 } else { 7 };
    let mut answer = if answer_expanded { 8 } else { 4 };

    while selected + answer + min_updates > total_height {
        if answer > 3 {
            answer -= 1;
        } else if selected > 4 {
            selected -= 1;
        } else {
            break;
        }
    }

    [
        Constraint::Length(selected),
        Constraint::Min(min_updates),
        Constraint::Length(answer),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_rollout::RecentUpdatesBootstrap;
    use chrono::DateTime;
    use crossterm::event::KeyModifiers;

    #[test]
    fn apply_snapshot_keeps_selected_message_when_new_updates_arrive() {
        let mut app = WatchApp::new(sample_bootstrap(4), sample_session_paths(), 0);
        app.selected_index = 1;
        app.follow_latest = false;

        app.apply_snapshot(sample_bootstrap(6));

        assert_eq!(app.selected_index, 1);
        assert!(!app.follow_latest);
    }

    #[test]
    fn refresh_worker_respects_poll_interval_when_unchanged() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            "{}",
            include_str!("../tests/fixtures/sample_rollout.jsonl")
        )
        .unwrap();
        let requested = Arc::new(Mutex::new((file.path().to_path_buf(), 30)));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker =
            spawn_refresh_worker(requested, Duration::from_millis(800), 64, tx, stop.clone());
        let initial = rx.recv_timeout(Duration::from_secs(3));
        thread::sleep(Duration::from_millis(1000));
        writeln!(file, "{}", serde_json::json!({"timestamp":"2026-04-11T19:00:00Z", "type":"event_msg", "payload":{"type":"agent_message", "phase":"commentary", "message":"Polling cadence changed."}})).unwrap();
        let premature = rx.recv_timeout(Duration::from_millis(200));
        let refreshed = rx.recv_timeout(Duration::from_secs(3));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(initial.is_ok());
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        assert!(matches!(refreshed, Ok(RefreshMessage::Snapshot(_))));
    }

    #[test]
    fn session_switch_interrupts_a_long_refresh_wait() {
        let requested = Mutex::new((PathBuf::from("new.jsonl"), 30));
        let start = std::time::Instant::now();
        wait_for_refresh(
            Duration::from_secs(3600),
            &requested,
            Path::new("old.jsonl"),
            30,
            &AtomicBool::new(false),
        );
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn cache_hits_bypass_model_work_and_only_misses_are_forwarded() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            "{}",
            include_str!("../tests/fixtures/sample_rollout.jsonl")
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let client = CodexSummaryClient::offline(cache.path().to_path_buf());
        let bootstrap = read_recent_updates(file.path(), 30, 64).unwrap();
        let summaries = bootstrap
            .updates
            .iter()
            .map(|update| {
                let key = update_key(update);
                (
                    key.clone(),
                    UpdateSummary {
                        key,
                        summary: "Cached parser fix verified".into(),
                        source: "codex_exec".into(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        assert!(!summaries.is_empty());
        client.store_summaries(file.path(), &summaries).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let (request_tx, request_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let (model_tx, model_rx) = mpsc::channel();
        // No model worker is consuming this channel, as if it were busy.
        let worker = spawn_cache_worker(request_rx, tx, model_tx, stop.clone(), client);
        let mut updates = bootstrap.updates.clone();
        let mut missing = updates[0].clone();
        missing.text = "New uncached update.".into();
        updates.push(missing.clone());
        request_tx
            .send(SummaryRequest {
                session_token: 7,
                session_path: file.path().to_path_buf(),
                updates,
            })
            .unwrap();
        let received = rx.recv_timeout(Duration::from_secs(3));
        let forwarded = model_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(forwarded.updates, vec![missing]);
        request_tx
            .send(SummaryRequest {
                session_token: 8,
                session_path: file.path().to_path_buf(),
                updates: bootstrap.updates,
            })
            .unwrap();
        let second = rx.recv_timeout(Duration::from_secs(3));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        match received.unwrap() {
            SummaryMessage::Cached {
                session_token: 7,
                summaries: cached,
            } => {
                assert_eq!(cached, summaries);
            }
            other => panic!("expected cache hits, got {other:?}"),
        }
        assert!(matches!(
            second,
            Ok(SummaryMessage::Cached {
                session_token: 8,
                ..
            })
        ));
        assert!(
            model_rx.try_recv().is_err(),
            "cache hits must not request a model"
        );
    }

    #[test]
    fn paging_targets_answer_panel_when_answer_is_expanded() {
        let mut app = WatchApp::new(sample_bootstrap(4), sample_session_paths(), 0);
        app.answer_expanded = true;
        app.answer_page_size = 5;

        app.page_down();

        assert_eq!(app.answer_scroll, 5);
        assert_eq!(app.selected_index, 3);
    }

    #[test]
    fn layout_preserves_recent_updates_space() {
        let constraints = layout_constraints(24, false, false);
        assert_eq!(constraints[0], Constraint::Length(7));
        assert_eq!(constraints[2], Constraint::Length(4));
    }

    #[test]
    fn rendered_screen_prioritizes_newest_visible_summaries() {
        let mut app = WatchApp::new(sample_bootstrap(60), sample_session_paths(), 0);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(app.updates_offset > 0);
        let request = app.build_summary_request().unwrap();
        assert_eq!(request.updates[0].offset, app.bootstrap.updates[59].offset);
    }

    #[test]
    fn readers_wrap_toggle_and_resize_keep_selection_visible() {
        let mut app = WatchApp::new(sample_bootstrap(60), sample_session_paths(), 0);
        for update in &mut app.bootstrap.updates {
            update.text = "Long original progress message. ".repeat(30);
        }
        for (width, height) in [(80, 24), (28, 16), (140, 50)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            for wrapped in [false, true] {
                app.wrap_updates = wrapped;
                terminal.draw(|frame| app.render(frame)).unwrap();
                assert!(app.updates_offset <= app.selected_index);
                app.selected_expanded = true;
                app.selected_scroll = u16::MAX;
                terminal.draw(|frame| app.render(frame)).unwrap();
                assert!(app.selected_scroll < u16::MAX);
                app.selected_expanded = false;
                app.selected_scroll = 0;
            }
        }
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            WatchAction::Continue
        ));
        assert!(!app.wrap_updates);
    }

    #[test]
    fn summary_request_only_enqueues_missing_updates() {
        let bootstrap = sample_bootstrap(3);
        let cached_key = update_key(&bootstrap.updates[0]);
        let mut app = WatchApp::new(bootstrap.clone(), sample_session_paths(), 0);
        app.summary_by_key.insert(
            cached_key.clone(),
            UpdateSummary {
                key: cached_key,
                summary: "Cache hit".to_string(),
                source: "codex_exec".to_string(),
            },
        );

        let request = app.build_summary_request().unwrap();

        assert_eq!(request.updates.len(), 2);
        assert_eq!(app.summary_inflight_keys.len(), 2);
    }

    #[test]
    fn new_summary_work_preempts_backlog_between_batches() {
        let make = |token, count| SummaryRequest {
            session_token: token,
            session_path: PathBuf::from("/tmp/rollout.jsonl"),
            updates: sample_bootstrap(count).updates,
        };
        let mut incoming = make(1, 1);
        incoming.updates[0].text = "Newest arrival".into();
        let same = prioritize_summary_request(make(1, 30), incoming);
        assert_eq!(same.updates.len(), 31);
        assert_eq!(same.updates[0].text, "Newest arrival");
        let switched = prioritize_summary_request(make(1, 30), make(2, 2));
        assert_eq!(switched.session_token, 2);
        assert_eq!(switched.updates.len(), 2);
    }

    #[test]
    fn summary_mode_moves_from_hydrating_to_codex() {
        let bootstrap = sample_bootstrap(2);
        let mut app = WatchApp::new(bootstrap.clone(), sample_session_paths(), 0);

        let request = app.build_summary_request().unwrap();
        assert_eq!(app.summary_mode(), "HYDRATING");

        let summaries = request
            .updates
            .iter()
            .map(|update| {
                let key = update_key(update);
                (
                    key.clone(),
                    UpdateSummary {
                        key,
                        summary: "Hydrated line".to_string(),
                        source: "codex_exec".to_string(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let keys = request.updates.iter().map(update_key).collect::<Vec<_>>();
        app.apply_summary_message(SummaryMessage::Hydrated {
            session_token: 0,
            keys,
            summaries,
        });

        assert_eq!(app.summary_mode(), "CODEX");
    }

    #[test]
    fn mixed_cached_and_hydrated_request_reaches_codex() {
        let mut app = WatchApp::new(sample_bootstrap(2), sample_session_paths(), 0);
        let request = app.build_summary_request().unwrap();
        let summaries = request
            .updates
            .iter()
            .map(|update| {
                let key = update_key(update);
                (
                    key.clone(),
                    UpdateSummary {
                        key,
                        summary: "Parser tests passed".into(),
                        source: "codex_exec".into(),
                    },
                )
            })
            .collect::<Vec<_>>();
        app.apply_summary_message(SummaryMessage::Cached {
            session_token: 0,
            summaries: HashMap::from([summaries[0].clone()]),
        });
        assert_eq!(app.summary_mode(), "HYDRATING");
        app.apply_summary_message(SummaryMessage::Hydrated {
            session_token: 0,
            keys: vec![summaries[1].0.clone()],
            summaries: HashMap::from([summaries[1].clone()]),
        });
        assert_eq!(app.summary_mode(), "CODEX");
        assert!(app.summary_inflight_keys.is_empty());
    }

    #[test]
    fn duplicate_summary_identity_does_not_stay_hydrating() {
        let mut bootstrap = sample_bootstrap(2);
        bootstrap.updates[1].text = bootstrap.updates[0].text.clone();
        let mut app = WatchApp::new(bootstrap, sample_session_paths(), 0);
        let request = app.build_summary_request().unwrap();
        assert_eq!(request.updates.len(), 1);
        let key = update_key(&request.updates[0]);
        app.apply_summary_message(SummaryMessage::Hydrated {
            session_token: 0,
            keys: vec![key.clone()],
            summaries: HashMap::from([(
                key.clone(),
                UpdateSummary {
                    key,
                    summary: "Parser tests passed".into(),
                    source: "codex_exec".into(),
                },
            )]),
        });
        assert_eq!(app.summary_mode(), "CODEX");
    }

    #[test]
    fn summary_failure_reports_fallback() {
        let mut app = WatchApp::new(sample_bootstrap(2), sample_session_paths(), 0);
        let request = app.build_summary_request().unwrap();
        let keys = request.updates.iter().map(update_key).collect::<Vec<_>>();

        app.apply_summary_message(SummaryMessage::Failed {
            session_token: 0,
            keys,
            message: "codex exec unavailable".to_string(),
        });

        assert_eq!(app.summary_mode(), "FALLBACK");
    }

    #[test]
    fn switch_session_resets_summary_state_and_updates_thread_position() {
        let mut app = WatchApp::new(sample_bootstrap(2), sample_session_paths(), 0);
        let request = app.build_summary_request().unwrap();
        let keys = request.updates.iter().map(update_key).collect::<Vec<_>>();
        app.summary_inflight_keys.extend(keys);

        let mut second = sample_bootstrap(3);
        second.session_path = PathBuf::from("/tmp/rollout-2.jsonl");
        app.switch_session(second, 1);

        assert_eq!(app.current_session_index, 1);
        assert_eq!(app.session_token, 1);
        assert!(app.summary_inflight_keys.is_empty());
        assert_eq!(app.thread_position_label().as_deref(), Some("thread 2/2"));
    }

    #[test]
    fn switching_back_renders_cached_rows_before_background_work() {
        let first = sample_bootstrap(2);
        let mut app = WatchApp::new(first.clone(), sample_session_paths(), 0);
        for update in &first.updates {
            let key = update_key(update);
            app.summary_by_key.insert(
                key.clone(),
                UpdateSummary {
                    key,
                    summary: "Cached parser fix verified".into(),
                    source: "codex_exec".into(),
                },
            );
        }
        let mut second = sample_bootstrap(1);
        second.session_path = PathBuf::from("/tmp/rollout-2.jsonl");
        app.switch_session(second, 1);
        assert!(app.summary_by_key.is_empty());
        let mut placeholder = first.clone();
        placeholder.updates.clear();
        placeholder.latest_final_answer = None;
        app.switch_session(placeholder, 0);

        assert_eq!(app.bootstrap, first);
        assert_eq!(app.summary_mode(), "CODEX");
        assert!(app.build_summary_request().is_none());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 32)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Cached parser fix verified"));

        let mut changed = first;
        changed.updates[0].text = "Changed original must not reuse stale text.".into();
        app.apply_snapshot(changed);
        assert_eq!(app.build_summary_request().unwrap().updates.len(), 1);
    }

    #[test]
    fn session_working_set_is_bounded() {
        let mut app = WatchApp::new(sample_bootstrap(1), sample_session_paths(), 0);
        for index in 1..20 {
            let mut next = sample_bootstrap(1);
            next.session_path = PathBuf::from(format!("/tmp/session-{index}.jsonl"));
            app.switch_session(next, index);
        }
        assert_eq!(app.recent_sessions.len(), 8);
        assert_eq!(
            app.recent_sessions.front().unwrap().0.session_path,
            PathBuf::from("/tmp/session-11.jsonl")
        );
    }

    fn sample_bootstrap(updates: usize) -> RecentUpdatesBootstrap {
        RecentUpdatesBootstrap {
            schema_version: 1,
            slice_type: "recent_updates_bootstrap",
            session_path: PathBuf::from("/tmp/rollout.jsonl"),
            cwd: PathBuf::from("/tmp/project"),
            target_updates: updates,
            scanned_complete_lines: 64,
            malformed_lines_skipped: 0,
            updates: (0..updates)
                .map(|index| RecentMessage {
                    offset: (index as u64 + 1) * 128,
                    timestamp: DateTime::parse_from_rfc3339("2026-04-13T18:00:00+00:00").unwrap(),
                    text: format!("Progress update {}.", index + 1),
                    phase: "commentary".to_string(),
                })
                .collect(),
            latest_final_answer: Some(RecentMessage {
                offset: 9_999,
                timestamp: DateTime::parse_from_rfc3339("2026-04-13T18:01:00+00:00").unwrap(),
                text: "Latest answer body.".to_string(),
                phase: "final_answer".to_string(),
            }),
        }
    }

    fn sample_session_paths() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/tmp/rollout-1.jsonl"),
            PathBuf::from("/tmp/rollout-2.jsonl"),
        ]
    }
}
