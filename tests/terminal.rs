use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(10);

enum WatchEvent {
    Stage(String),
    Child(Box<dyn ChildKiller + Send + Sync>),
    ChildExited,
    Done,
}

struct Watchdog {
    events: Sender<WatchEvent>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn start(budget: Duration) -> Self {
        let (events, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + budget;
            let mut stage = String::from("setup");
            let mut killer: Option<Box<dyn ChildKiller + Send + Sync>> = None;
            loop {
                let event = if Instant::now() >= deadline {
                    Err(mpsc::RecvTimeoutError::Timeout)
                } else {
                    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                };
                match event {
                    Ok(WatchEvent::Stage(next)) => stage = next,
                    Ok(WatchEvent::Child(child)) => killer = Some(child),
                    Ok(WatchEvent::ChildExited) => killer = None,
                    Ok(WatchEvent::Done) => return,
                    Err(_) => {
                        let _ = writeln!(
                            std::io::stderr(),
                            "Terminal test hard deadline ({budget:?}) exceeded during {stage}"
                        );
                        if let Some(mut killer) = killer {
                            let (tx, rx) = mpsc::channel();
                            thread::spawn(move || {
                                let _ = tx.send(killer.kill());
                            });
                            // Even a stuck OS cleanup call must not defeat the deadline.
                            let _ = rx.recv_timeout(Duration::from_secs(2));
                        }
                        std::process::exit(124);
                    }
                }
            }
        });
        Self {
            events,
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.events.send(WatchEvent::Done);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stage(events: &Sender<WatchEvent>, label: &str) {
    let _ = events.send(WatchEvent::Stage(label.into()));
    // Bypass libtest's capture so a native deadlock leaves its last stage in CI.
    let _ = writeln!(std::io::stderr(), "Terminal stage: {label}");
}

#[derive(Default)]
struct CursorQuery(bool);

impl vte::Perform for CursorQuery {
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        self.0 =
            !ignore && intermediates.is_empty() && action == 'n' && params.iter().eq([&[6u16][..]]);
    }
}

struct TerminalScreen {
    parser: vt100::Parser,
    queries: vte::Parser,
}

impl TerminalScreen {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new(32, 100, 0),
            queries: vte::Parser::new(),
        }
    }

    fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        for byte in bytes {
            self.parser.process(std::slice::from_ref(byte));
            let mut query = CursorQuery::default();
            self.queries.advance(&mut query, *byte);
            if query.0 {
                let (row, col) = self.parser.screen().cursor_position();
                write!(replies, "\x1b[{};{}R", row + 1, col + 1).unwrap();
            }
        }
        replies
    }

    fn visible_text(&self) -> String {
        let screen = self.parser.screen();
        // contents() joins soft-wrapped rows for copy/paste. UI assertions
        // need physical screen rows, regardless of how ConPTY emitted them.
        screen
            .rows(0, screen.size().1)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

struct Terminal {
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<SharedWriter>,
    reader: Option<JoinHandle<()>>,
    output: Receiver<()>,
    screen: Arc<Mutex<TerminalScreen>>,
    events: Sender<WatchEvent>,
    exited: bool,
}

impl Terminal {
    fn open(session: &Path, cache: &Path, watchdog: &Watchdog) -> Self {
        stage(&watchdog.events, "create PTY");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 32,
                cols: 100,
                ..PtySize::default()
            })
            .unwrap();
        let binary = std::env::var_os("LOWDOWN_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_lowdown").into());
        let mut command = CommandBuilder::new(std::fs::canonicalize(binary).unwrap());
        command.args(["watch", "--poll-seconds", "0.2", "--session"]);
        command.arg(session);
        command.env("LOWDOWN_SUMMARY_PROVIDER", "fallback");
        command.env("LOWDOWN_CACHE_DIR", cache);
        command.env("CODEX_HOME", cache.join("codex"));
        command.env("TERM", "xterm-256color");
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let mut input = pair.master.try_clone_reader().unwrap();
        let (tx, output) = mpsc::channel();
        let screen = Arc::new(Mutex::new(TerminalScreen::new()));
        let reader_screen = Arc::clone(&screen);
        let reply_writer = Arc::clone(&writer);
        // ConPTY can request the inherited cursor before spawn, during resize,
        // or during close. Service it independently of the test's control loop.
        let reader = thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let replies = reader_screen.lock().unwrap().process(&buffer[..count]);
                        if !replies.is_empty() {
                            let mut writer = reply_writer.lock().unwrap();
                            if writer
                                .write_all(&replies)
                                .and_then(|()| writer.flush())
                                .is_err()
                            {
                                break;
                            }
                        }
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut terminal = Self {
            child: None,
            master: Some(pair.master),
            writer: Some(writer),
            reader: Some(reader),
            output,
            screen,
            events: watchdog.events.clone(),
            exited: false,
        };
        stage(&terminal.events, "spawn child");
        let child = pair.slave.spawn_command(command);
        drop(pair.slave);
        terminal.child = Some(child.unwrap());
        let _ = terminal.events.send(WatchEvent::Child(
            terminal.child.as_ref().unwrap().clone_killer(),
        ));
        terminal
    }

    fn pump(&mut self) {
        let _ = self.output.recv_timeout(Duration::from_millis(20));
    }

    fn until(&mut self, label: &str, predicate: impl Fn(&str) -> bool) -> String {
        stage(&self.events, label);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            let screen = self.screen.lock().unwrap().visible_text();
            if predicate(&screen) {
                return screen;
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                self.exited = true;
                let _ = self.events.send(WatchEvent::ChildExited);
                panic!("Child exited ({status}) during {label}:\n{screen}");
            }
            assert!(
                Instant::now() < deadline,
                "Timed out during {label}:\n{screen}"
            );
        }
    }

    fn send(&mut self, keys: &[u8]) {
        stage(&self.events, "send input");
        let mut writer = self.writer.as_ref().unwrap().lock().unwrap();
        writer.write_all(keys).unwrap();
        writer.flush().unwrap();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        stage(&self.events, &format!("resize {cols}x{rows}"));
        self.screen.lock().unwrap().parser.set_size(rows, cols);
        self.master
            .as_ref()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                ..PtySize::default()
            })
            .unwrap();
    }

    fn quit(&mut self) {
        self.send(b"q");
        stage(&self.events, "quit and restore terminal");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                self.exited = true;
                let _ = self.events.send(WatchEvent::ChildExited);
                assert!(status.success(), "Unclean quit: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "Quit blocked");
        }
        while !self.restored() && Instant::now() < deadline {
            self.pump();
        }
        assert!(self.restored(), "Alternate screen or cursor not restored");
    }

    fn restored(&self) -> bool {
        let screen = self.screen.lock().unwrap();
        !screen.parser.screen().alternate_screen() && !screen.parser.screen().hide_cursor()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        stage(&self.events, "reap child");
        if !self.exited
            && let Some(child) = self.child.as_mut()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.events.send(WatchEvent::ChildExited);
        stage(&self.events, "close PTY (background replies still active)");
        self.master.take();
        self.writer.take();
        stage(&self.events, "join PTY reader");
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn record(file: &mut File, payload: serde_json::Value) {
    writeln!(
        file,
        "{}",
        json!({
            "timestamp": "2026-09-06T12:00:00Z", "type": "event_msg", "payload": payload
        })
    )
    .unwrap();
    file.flush().unwrap();
}

fn update(file: &mut File, index: usize) {
    let details = (0..40)
        .map(|i| format!("DETAIL{i:02} Original progress text.\n"))
        .collect::<String>();
    record(
        file,
        json!({"type":"agent_message", "phase":"commentary",
        "message": format!("UPDATE{index:03} Testing the terminal.\n{details}")}),
    );
}

fn selected(screen: &str, marker: &str) -> bool {
    screen
        .lines()
        .nth(1)
        .is_some_and(|line| line.contains(marker))
}

fn loaded(screen: &str) -> usize {
    screen
        .lines()
        .find_map(|line| line.split(" | ").nth(1)?.split('/').nth(1)?.parse().ok())
        .unwrap_or(0)
}

#[test]
fn screen_assertions_preserve_physical_rows_after_soft_wraps() {
    for explicit_positioning in [false, true] {
        let mut screen = TerminalScreen::new();
        // Full-width rows can be emitted without newlines by a terminal host.
        for (row, text) in [
            "Selected update",
            "UPDATE119 Original text",
            "12:00 | 30/30 | LIVE",
        ]
        .into_iter()
        .enumerate()
        {
            if explicit_positioning {
                screen.process(format!("\x1b[{};1H", row + 1).as_bytes());
            }
            screen.process(format!("{text:<100}").as_bytes());
        }
        assert_eq!(screen.parser.screen().row_wrapped(0), !explicit_positioning);
        let text = screen.visible_text();
        assert!(selected(&text, "UPDATE119"), "{text:?}");
        assert_eq!(loaded(&text), 30);
    }
}

#[test]
fn cursor_queries_reply_at_the_current_position_across_read_boundaries() {
    let bytes = b"\x1b[6n\x1b[4;12H\x1b[6nhello\x1b[6n";
    for chunk_size in 1..=bytes.len() {
        let mut screen = TerminalScreen::new();
        let mut replies = Vec::new();
        for chunk in bytes.chunks(chunk_size) {
            replies.extend(screen.process(chunk));
        }
        assert_eq!(
            replies, b"\x1b[1;1R\x1b[4;12R\x1b[4;17R",
            "chunks of {chunk_size}"
        );
    }
}

#[test]
fn ordinary_text_and_other_controls_do_not_generate_cursor_replies() {
    let mut screen = TerminalScreen::new();
    assert!(
        screen
            .process(b"text [6n\x1b[?6n\x1b[5n\x1b[6;1n\x1b[6:1n")
            .is_empty()
    );
    screen.parser.set_size(16, 40);
    assert_eq!(screen.process(b"\x1b[16;40H\x1b[6n"), b"\x1b[16;40R");
}

#[test]
fn watchdog_exits_with_diagnostics_even_when_cleanup_blocks() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("watchdog.log");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "watchdog_blocked_cleanup_fixture",
            "--ignored",
            "--nocapture",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Watchdog failed to bound the blocked fixture");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let log = std::fs::read_to_string(log).unwrap();
    assert_eq!(status.code(), Some(124), "{log}");
    assert!(
        log.contains("exceeded during controlled cleanup stall"),
        "{log}"
    );
    assert!(
        log.contains("attempting controlled child termination"),
        "{log}"
    );
}

#[test]
#[ignore = "subprocess fixture for the hard-deadline regression"]
fn watchdog_blocked_cleanup_fixture() {
    #[derive(Debug)]
    struct StuckKiller;

    impl ChildKiller for StuckKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            let _ = writeln!(std::io::stderr(), "attempting controlled child termination");
            loop {
                thread::park();
            }
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self)
        }
    }

    let watchdog = Watchdog::start(Duration::from_millis(150));
    watchdog
        .events
        .send(WatchEvent::Child(Box::new(StuckKiller)))
        .unwrap();
    stage(&watchdog.events, "controlled cleanup stall");
    loop {
        thread::park();
    }
}

#[test]
fn real_terminal_handles_history_readers_resize_and_streaming() {
    let stream_seconds = std::env::var("LOWDOWN_TEST_STREAM_SECONDS")
        .map(|s| {
            s.parse::<u64>()
                .expect("LOWDOWN_TEST_STREAM_SECONDS must be an integer")
        })
        .unwrap_or(0);
    assert!(
        stream_seconds <= 600,
        "Streaming check is capped at ten minutes"
    );
    // Declared first so the deadline also covers Terminal::drop on panic.
    let watchdog = Watchdog::start(Duration::from_secs(120 + stream_seconds));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let mut file = File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"session_meta", "payload":{
            "id":"terminal-proof", "cwd":directory.path(), "timestamp":"2026-09-06T12:00:00Z"
        }})
    )
    .unwrap();
    for index in 0..120 {
        update(&mut file, index);
    }
    let answer = (0..60)
        .map(|i| format!("FINAL{i:02} Full answer text.\n"))
        .collect::<String>();
    record(
        &mut file,
        json!({"type":"agent_message", "phase":"final_answer",
        "message": format!("Final answer ready.\n{answer}")}),
    );
    drop(file);

    let start = Instant::now();
    let mut terminal = Terminal::open(&path, &directory.path().join("cache"), &watchdog);
    println!(
        "Terminal child PID: {:?}",
        terminal.child.as_ref().unwrap().process_id()
    );
    let initial = terminal.until("first paint", |s| {
        selected(s, "UPDATE119") && s.contains("30/30") && s.contains("Final answer ready.")
    });
    println!("First paint: {:?}\n{initial}", start.elapsed());
    assert!(
        !initial.contains("FINAL01"),
        "Answer should start collapsed"
    );

    terminal.send(b"k");
    terminal.until("selection", |s| {
        selected(s, "UPDATE118") && s.contains("SCROLL")
    });
    terminal.send(b"i");
    terminal.until("expanded original", |s| s.contains("DETAIL06"));
    terminal.send(b"\x1b[6~");
    terminal.until("page original", |s| {
        !selected(s, "UPDATE118") && s.contains("DETAIL15")
    });
    terminal.send(b"\x1b[5~");
    terminal.until("page original back", |s| selected(s, "UPDATE118"));
    terminal.send(b"io");
    terminal.until("expanded answer", |s| s.contains("FINAL01"));
    terminal.send(b"\x1b[6~");
    terminal.until("page answer", |s| {
        s.contains("FINAL06") && !s.contains("FINAL00") && selected(s, "UPDATE118")
    });
    terminal.send(b"\x1b[5~");
    terminal.until("page answer back", |s| s.contains("FINAL00"));
    terminal.send(b"op");
    terminal.until("two-line updates", |s| {
        s.lines().filter(|l| l.contains(">>")).count() == 2
    });
    terminal.send(b"p");
    terminal.until("cropped updates", |s| {
        s.lines().filter(|l| l.contains(">>")).count() == 1
    });

    terminal.resize(16, 40);
    terminal.until("small resize", |s| {
        s.lines().next().is_some_and(|l| l.chars().count() == 40)
            && s.lines().count() == 16
            && s.lines().last().is_some_and(|l| l.contains("o expand"))
    });
    terminal.resize(32, 100);
    terminal.until("large resize", |s| {
        s.lines().next().is_some_and(|l| l.chars().count() == 100)
            && s.lines().count() == 32
            && s.lines().last().is_some_and(|l| l.contains("o expand"))
    });
    for count in [60, 90, 120] {
        terminal.send(b"\x1b[5~\x1b[5~\x1b[5~\x1b[5~\x1b[5~\x1b[5~");
        terminal.until("older history", |s| loaded(s) >= count);
    }
    terminal.send(b"\x1b[5~".repeat(20).as_slice());
    terminal.until("oldest update", |s| selected(s, "UPDATE000"));
    terminal.send(b"\x1b[6~".repeat(20).as_slice());
    terminal.until("return to live", |s| {
        selected(s, "UPDATE119") && s.contains("LIVE")
    });

    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    let streaming = Instant::now();
    let mut index = 120;
    while index < 132 || streaming.elapsed() < Duration::from_secs(stream_seconds) {
        let appended = Instant::now();
        update(&mut file, index);
        terminal.until("live append", |s| selected(s, &format!("UPDATE{index:03}")));
        terminal.send(b"k");
        terminal.until("input during streaming", |s| {
            selected(s, &format!("UPDATE{:03}", index - 1))
        });
        terminal.send(b"j");
        terminal.until("resume following", |s| {
            selected(s, &format!("UPDATE{index:03}"))
        });
        println!("Append plus navigation {index}: {:?}", appended.elapsed());
        index += 1;
    }
    println!(
        "Streaming smoke: {:?} (not a soak test)",
        streaming.elapsed()
    );
    terminal.quit();
}
