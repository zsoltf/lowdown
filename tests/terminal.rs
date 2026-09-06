use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Terminal {
    child: Box<dyn Child + Send + Sync>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    reader: Option<JoinHandle<()>>,
    output: Receiver<Vec<u8>>,
    parser: vt100::Parser,
    exited: bool,
}

impl Terminal {
    fn open(session: &Path, cache: &Path) -> Self {
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
        let writer = pair.master.take_writer().unwrap();
        let mut input = pair.master.try_clone_reader().unwrap();
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let (tx, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if tx.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            master: Some(pair.master),
            writer: Some(writer),
            reader: Some(reader),
            output,
            parser: vt100::Parser::new(32, 100, 0),
            exited: false,
        }
    }

    fn pump(&mut self) {
        if let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(20)) {
            self.parser.process(&bytes);
        }
    }

    fn until(&mut self, label: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            let screen = self.parser.screen().contents();
            if predicate(&screen) {
                return screen;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "Child exited during {label}:\n{screen}"
            );
            assert!(
                Instant::now() < deadline,
                "Timed out during {label}:\n{screen}"
            );
        }
    }

    fn send(&mut self, keys: &[u8]) {
        let writer = self.writer.as_mut().unwrap();
        writer.write_all(keys).unwrap();
        writer.flush().unwrap();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows, cols);
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
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.exited = true;
                assert!(status.success(), "Unclean quit: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "Quit blocked");
        }
        while (self.parser.screen().alternate_screen() || self.parser.screen().hide_cursor())
            && Instant::now() < deadline
        {
            self.pump();
        }
        assert!(
            !self.parser.screen().alternate_screen(),
            "Alternate screen not restored"
        );
        assert!(!self.parser.screen().hide_cursor(), "Cursor not restored");
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.writer.take();
        self.master.take();
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
fn real_terminal_handles_history_readers_resize_and_streaming() {
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
    let mut terminal = Terminal::open(&path, &directory.path().join("cache"));
    println!("Terminal child PID: {:?}", terminal.child.process_id());
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
    });
    terminal.resize(32, 100);
    terminal.until("large resize", |s| {
        s.lines().next().is_some_and(|l| l.chars().count() == 100)
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
