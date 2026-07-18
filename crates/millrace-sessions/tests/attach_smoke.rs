use std::{
    fs::{self, File},
    io::Write as _,
    os::fd::{BorrowedFd, FromRawFd},
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use assert_cmd::prelude::*;
use millrace_sessions_core::scrollback::TerminalSnapshot as DurableTerminalSnapshot;
use millrace_sessions_tui::TerminalEmulator;
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;

struct TempHost {
    state_dir: tempfile::TempDir,
}

impl TempHost {
    fn new() -> Self {
        Self {
            state_dir: tempfile::tempdir().expect("temp state dir"),
        }
    }

    fn state_dir(&self) -> &Path {
        self.state_dir.path()
    }
}

impl Drop for TempHost {
    fn drop(&mut self) {
        let host_json = self.state_dir.path().join("host.json");
        let Ok(raw) = fs::read_to_string(host_json) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let Some(pid) = value.get("pid").and_then(Value::as_u64) else {
            return;
        };

        let pid = Pid::from_raw(pid as i32);
        let _ = kill(pid, Signal::SIGTERM);
        for _ in 0..40 {
            if kill(pid, None).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = kill(pid, Signal::SIGKILL);
    }
}

struct PtyCockpit {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn std::io::Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    output: Arc<Mutex<Vec<u8>>>,
    output_paused: Arc<AtomicBool>,
    reader_thread: Option<JoinHandle<()>>,
    initial_termios: nix::sys::termios::Termios,
}

struct BoundedPtyWriter {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<usize>,
}

impl BoundedPtyWriter {
    fn finish_within(self, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        while !self.handle.is_finished() {
            if Instant::now() >= deadline {
                self.stop.store(true, Ordering::SeqCst);
                panic!("cockpit PTY writer did not finish within {timeout:?}");
            }
            thread::sleep(Duration::from_millis(25));
        }
        self.handle.join().expect("join bounded cockpit PTY writer")
    }
}

impl PtyCockpit {
    fn spawn(
        host: &TempHost,
        workspace: &Path,
        agent_argv: &[String],
        extra_env: &[(&str, &str)],
    ) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 44,
                cols: 220,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open cockpit PTY");
        let initial_termios = {
            let fd = pair.master.as_raw_fd().expect("cockpit PTY raw fd");
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            nix::sys::termios::tcgetattr(borrowed).expect("initial cockpit PTY termios")
        };
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let mut command = CommandBuilder::new(binary_override("MILLMUX_BIN", "millmux"));
        command.args(["cockpit", "--workspace"]);
        command.arg(workspace);
        command.arg("--no-start");
        command.arg("--");
        command.args(agent_argv);
        command.env("MILLMUX_STATE_DIR", host.state_dir());
        command.env(
            "MILLMUX_HOST_BIN",
            binary_override("MILLMUX_HOST_BIN", "millrace-sessiond"),
        );
        command.env(
            "MILLMUX_WORKER_BIN",
            binary_override("MILLMUX_WORKER_BIN", "millrace-session-worker"),
        );
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn cockpit in PTY");
        drop(pair.slave);

        let output = Arc::new(Mutex::new(Vec::new()));
        let output_paused = Arc::new(AtomicBool::new(false));
        let reader_output = Arc::clone(&output);
        let reader_paused = Arc::clone(&output_paused);
        let reader_thread = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                while reader_paused.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
                match std::io::Read::read(&mut reader, &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => reader_output
                        .lock()
                        .expect("PTY output lock")
                        .extend_from_slice(&buffer[..count]),
                }
            }
        });

        Self {
            master: Some(pair.master),
            writer: Some(writer),
            child: Some(child),
            output,
            output_paused,
            reader_thread: Some(reader_thread),
            initial_termios,
        }
    }

    fn output_len(&self) -> usize {
        self.output.lock().expect("PTY output lock").len()
    }

    fn pause_output(&self, paused: bool) {
        self.output_paused.store(paused, Ordering::SeqCst);
    }

    fn output_from(&self, offset: usize) -> Vec<u8> {
        self.output.lock().expect("PTY output lock")[offset..].to_vec()
    }

    fn wait_for_output_after(&mut self, offset: usize, needle: &[u8]) {
        for _ in 0..400 {
            if contains_bytes(&self.output_from(offset), needle) {
                return;
            }
            if self
                .child
                .as_mut()
                .expect("cockpit child")
                .try_wait()
                .expect("poll cockpit")
                .is_some()
            {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let output = self.output_from(offset);
        panic!(
            "cockpit output did not contain {:?}: {}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&output)
        );
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer
            .as_mut()
            .expect("cockpit PTY writer")
            .write_all(bytes)
            .expect("write cockpit PTY");
        self.writer
            .as_mut()
            .expect("cockpit PTY writer")
            .flush()
            .expect("flush cockpit PTY");
    }

    fn signal(&self, signal: Signal) {
        let pid = self
            .child
            .as_ref()
            .expect("cockpit child")
            .process_id()
            .expect("cockpit child pid");
        kill(Pid::from_raw(pid as i32), signal).expect("signal cockpit");
    }

    fn send_bounded_in_background(&self, bytes: Vec<u8>) -> BoundedPtyWriter {
        let fd = self
            .master
            .as_ref()
            .expect("cockpit PTY master")
            .as_raw_fd()
            .expect("cockpit PTY raw fd");
        let duplicate = unsafe { nix::libc::dup(fd) };
        assert!(
            duplicate >= 0,
            "duplicate cockpit PTY writer: {}",
            std::io::Error::last_os_error()
        );
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut writer = unsafe { File::from_raw_fd(duplicate) };
            let mut sent = 0;
            while sent < bytes.len() && !writer_stop.load(Ordering::SeqCst) {
                let mut poll_fd = nix::libc::pollfd {
                    fd: duplicate,
                    events: nix::libc::POLLOUT,
                    revents: 0,
                };
                let poll_result = unsafe { nix::libc::poll(&mut poll_fd, 1, 25) };
                if poll_result < 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::EINTR) {
                        continue;
                    }
                    break;
                }
                if poll_fd.revents & (nix::libc::POLLERR | nix::libc::POLLHUP | nix::libc::POLLNVAL)
                    != 0
                {
                    break;
                }
                if poll_result == 0 || poll_fd.revents & nix::libc::POLLOUT == 0 {
                    continue;
                }
                let end = (sent + 512).min(bytes.len());
                match writer.write(&bytes[sent..end]) {
                    Ok(0) => break,
                    Ok(written) => sent += written,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            sent
        });
        BoundedPtyWriter { stop, handle }
    }

    fn resize(&self, rows: u16, cols: u16) {
        self.master
            .as_ref()
            .expect("cockpit PTY master")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize cockpit PTY");
    }

    fn termios(&self) -> nix::sys::termios::Termios {
        let fd = self
            .master
            .as_ref()
            .expect("cockpit PTY master")
            .as_raw_fd()
            .expect("cockpit PTY raw fd");
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        nix::sys::termios::tcgetattr(borrowed).expect("cockpit PTY termios")
    }

    fn exited_within(&mut self, attempts: usize) -> bool {
        for _ in 0..attempts {
            if self
                .child
                .as_mut()
                .expect("cockpit child")
                .try_wait()
                .expect("poll cockpit")
                .is_some()
            {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn wait_for_exit(&mut self) {
        if self.exited_within(200) {
            return;
        }
        panic!("cockpit did not exit");
    }
}

impl Drop for PtyCockpit {
    fn drop(&mut self) {
        self.output_paused.store(false, Ordering::SeqCst);
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.writer.take();
        self.master.take();
        if let Some(reader_thread) = self.reader_thread.take() {
            let _ = reader_thread.join();
        }
    }
}

struct ManagedRawFixture {
    host: TempHost,
    workspace: tempfile::TempDir,
    daemon_id: String,
    agent_id: String,
    agent_argv: Vec<String>,
    input_log: std::path::PathBuf,
    resize_log: std::path::PathBuf,
    phase_log: std::path::PathBuf,
    resume_input: std::path::PathBuf,
    _lifetime_guard: MutexGuard<'static, ()>,
}

static MANAGED_RAW_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

impl ManagedRawFixture {
    fn new() -> Self {
        Self::with_input_mode(None)
    }

    fn new_with_slow_input() -> Self {
        Self::with_input_mode(Some("slow"))
    }

    fn new_with_stalled_input() -> Self {
        Self::with_input_mode(Some("stalled"))
    }

    fn new_with_resumable_input() -> Self {
        Self::with_input_mode(Some("resumable"))
    }

    fn new_silent() -> Self {
        Self::with_input_mode(Some("silent"))
    }

    fn with_input_mode(input_mode: Option<&str>) -> Self {
        let lifetime_guard = MANAGED_RAW_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let host = TempHost::new();
        let workspace = tempfile::tempdir().expect("managed raw workspace");
        let input_log = workspace.path().join("raw-input.log");
        let resize_log = workspace.path().join("raw-resize.log");
        let phase_log = workspace.path().join("managed-raw-phase.log");
        let resume_input = workspace.path().join("resume-input");
        let fixture = workspace.path().join("raw_fixture.py");
        fs::write(
            &fixture,
            r#"import os
import signal
import sys
import time
import tty

input_log = sys.argv[1]
resize_log = sys.argv[2]
input_mode = sys.argv[3] if len(sys.argv) > 3 else ""
resume_input = sys.argv[4] if len(sys.argv) > 4 else ""
slow_reads = input_mode == "slow"
stalled_input = input_mode == "stalled"
resumable_input = input_mode == "resumable"
silent = input_mode == "silent"

def record_size(*_):
    try:
        size = os.get_terminal_size(0)
        with open(resize_log, "a", encoding="ascii") as handle:
            handle.write(f"{size.lines}x{size.columns}\n")
            handle.flush()
        if (size.lines, size.columns) == (38, 42):
            os.write(1, b"\x1b[HRETURN_SIZE:38x42:\xe7\x95\x8c\x1b[6;11H")
        elif (size.lines, size.columns) in ((46, 210), (45, 77)):
            os.write(1, b"\r\nSIZE:" + f"{size.lines}x{size.columns}".encode("ascii") + b":\xe7\x95\x8c")
            if (size.lines, size.columns) == (45, 77):
                os.write(1, b"\r\nLIVE_WRAP_BEGIN:" + b"w" * 88 + b":LIVE_WRAP_END\r\n")
    except OSError:
        pass

signal.signal(signal.SIGWINCH, record_size)
tty.setraw(0)
if not silent:
    record_size()
    os.write(1, b"\x1b[?1049h\x1b[2J\x1b[HALT_READY")

if stalled_input or resumable_input:
  while not resumable_input or not os.path.exists(resume_input):
    time.sleep(1)

byte_count = 0
with open(input_log, "ab", buffering=0) as handle:
  while True:
    byte = os.read(0, 1)
    if not byte:
        break
    byte_count += 1
    if slow_reads and byte_count % 128 == 0:
        time.sleep(0.005)
    handle.write(byte.hex().encode("ascii") + b"\n")
    if not slow_reads and not resumable_input:
        os.write(1, b"\r\nBYTE:" + byte.hex().encode("ascii"))
    if resumable_input and byte == b"R":
        os.write(1, b"\r\nRESUME_OUTPUT:R\r\n")
    if byte == b"F":
        chunk = b"f" * 65536
        for _ in range(8):
            os.write(1, chunk)
    if byte == b"Q":
        break
"#,
        )
        .expect("write raw fixture");

        let daemon_id = start_session_with_role(
            &host,
            workspace.path(),
            "managed-daemon",
            "millrace-daemon",
            "printf 'daemon-ready\\n'; while :; do sleep 1; done",
        );
        let mut agent_argv = vec![
            resolve_path_executable("python3"),
            "-u".to_string(),
            fixture.display().to_string(),
            input_log.display().to_string(),
            resize_log.display().to_string(),
        ];
        if let Some(input_mode) = input_mode {
            agent_argv.push(input_mode.to_string());
            agent_argv.push(resume_input.display().to_string());
        }
        let agent_id = start_session_argv(
            &host,
            workspace.path(),
            "managed-agent",
            "millrace-agent",
            &agent_argv,
        );
        if input_mode != Some("silent") {
            wait_for_logs(&host, &agent_id, "ALT_READY");
        }

        Self {
            host,
            workspace,
            daemon_id,
            agent_id,
            agent_argv,
            input_log,
            resize_log,
            phase_log,
            resume_input,
            _lifetime_guard: lifetime_guard,
        }
    }

    fn spawn_cockpit(&self, extra_env: &[(&str, &str)]) -> PtyCockpit {
        fs::write(&self.phase_log, b"").expect("clear managed raw phase log");
        let phase_log = self.phase_log.to_str().expect("phase log path is UTF-8");
        let mut cockpit_env = extra_env.to_vec();
        cockpit_env.push(("MILLMUX_TEST_MANAGED_RAW_PHASE_FILE", phase_log));
        let mut cockpit = PtyCockpit::spawn(
            &self.host,
            self.workspace.path(),
            &self.agent_argv,
            &cockpit_env,
        );
        cockpit.wait_for_output_after(0, b"Agent Terminal");
        wait_for_attached_clients(&self.host, &self.agent_id, 1);
        cockpit
    }

    fn wait_for_phase(&self, phase: &str) {
        wait_for_file_contains(&self.phase_log, phase);
    }

    fn assert_phase_order(&self, earlier: &str, later: &str) {
        let phases = fs::read_to_string(&self.phase_log).expect("read managed raw phase log");
        let earlier_at = phases.find(earlier).expect("earlier managed raw phase");
        let later_at = phases.find(later).expect("later managed raw phase");
        assert!(
            earlier_at < later_at,
            "managed raw phase {earlier:?} must precede {later:?}: {phases:?}"
        );
    }

    fn assert_agent_running(&self) {
        let output = millmux_command(&self.host)
            .args(["status", &self.agent_id, "--json"])
            .output()
            .expect("agent status");
        assert!(output.status.success(), "{:?}", output);
        let value: Value = serde_json::from_slice(&output.stdout).expect("status json");
        assert_eq!(value["session"]["process_state"], "running");
    }

    fn owned_processes(&self) -> Vec<FixtureSessionProcesses> {
        [&self.agent_id, &self.daemon_id]
            .into_iter()
            .filter_map(|session_id| fixture_session_processes(&self.host, session_id))
            .collect()
    }
}

impl Drop for ManagedRawFixture {
    fn drop(&mut self) {
        let processes = self.owned_processes();
        for session_id in [&self.agent_id, &self.daemon_id] {
            let _ = millmux_command(&self.host)
                .args(["kill", "--json", session_id])
                .output();
        }
        stop_fixture_processes(&processes);
    }
}

#[derive(Clone, Copy, Debug)]
struct FixtureSessionProcesses {
    worker_pid: Option<u32>,
    child_pid: Option<u32>,
    child_pgid: Option<u32>,
}

#[test]
fn cli_smoke_send_logs_events_resize_and_stream_through_host() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let session_id = start_session(
        &host,
        workspace.path(),
        "interactive",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    wait_for_logs(&host, &session_id, "ready");
    assert_cli_json_attach_state_consistency(&host, &session_id, 0, Value::Null);

    millmux_command(&host)
        .args(["send", &session_id, "--text", "ping\n"])
        .assert()
        .success();
    wait_for_logs(&host, &session_id, "got:ping");

    let logs = millmux_command(&host)
        .args(["logs", &session_id, "--tail", "1", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs: Value = serde_json::from_slice(&logs).expect("logs json");
    assert_eq!(logs["lines"][0]["line"], "got:ping");

    let events = millmux_command(&host)
        .args(["events", &session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events: Value = serde_json::from_slice(&events).expect("events json");
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| { event["kind"] == "input_sent" }));

    let resize = millmux_command(&host)
        .args([
            "resize",
            &session_id,
            "--rows",
            "30",
            "--cols",
            "100",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let resize: Value = serde_json::from_slice(&resize).expect("resize json");
    assert_eq!(resize["rows"], 30);
    assert_eq!(resize["cols"], 100);

    let attach_id = start_session(
        &host,
        workspace.path(),
        "attach",
        "printf 'attach-ready\\n'; sleep 3",
    );
    wait_for_logs(&host, &attach_id, "attach-ready");
    wait_for_attach_output(&host, &attach_id, "attach-ready");
}

#[test]
fn cli_smoke_raw_attach_replay_none_preserves_live_bytes() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let session_id = start_session(
        &host,
        workspace.path(),
        "raw-attach",
        "printf 'ready-before-raw\\n'; while [ ! -f go-raw ]; do sleep 0.05; done; printf '\\377raw-live\\n'",
    );
    wait_for_logs(&host, &session_id, "ready-before-raw");

    let mut attach = millmux_command(&host);
    let attach = attach
        .args([
            "attach",
            &session_id,
            "--read-only",
            "--raw",
            "--replay",
            "none",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn raw attach");

    wait_for_attached_clients(&host, &session_id, 1);
    fs::write(workspace.path().join("go-raw"), b"go").expect("release raw fixture");

    let output = attach.wait_with_output().expect("wait for raw attach");
    assert!(
        output.status.success(),
        "raw attach failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        contains_bytes(&output.stdout, b"\xffraw-live"),
        "raw attach did not preserve invalid live bytes: {:?}",
        output.stdout
    );
    assert!(
        !contains_bytes(&output.stdout, b"ready-before-raw"),
        "raw attach --replay none unexpectedly replayed legacy scrollback: {:?}",
        output.stdout
    );
}

#[test]
fn cli_smoke_writable_attach_with_redirected_stdin_remains_output_only() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");
    let session_id = start_session(
        &host,
        workspace.path(),
        "redirected-stdin-attach",
        "printf 'redirected-stdin-ready\\n'; sleep 1",
    );
    wait_for_logs(&host, &session_id, "redirected-stdin-ready");

    let output = millmux_command(&host)
        .args(["attach", &session_id])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run writable attach with redirected stdin");

    assert!(
        output.status.success(),
        "writable attach rejected redirected stdin: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(contains_bytes(&output.stdout, b"redirected-stdin-ready"));
}

#[test]
fn managed_raw_fixture_drop_leaves_no_owned_workers_or_children() {
    let processes = {
        let fixture = ManagedRawFixture::new();
        let processes = fixture.owned_processes();
        assert_eq!(processes.len(), 2);
        assert_eq!(live_fixture_process_count(&processes), 4);
        processes
    };

    assert_eq!(live_fixture_process_count(&processes), 0);
}

#[test]
fn managed_raw_attach_local_detach_round_trip_preserves_child_and_terminal_ownership() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit = fixture.spawn_cockpit(&[]);
    wait_for_file_contains(&fixture.resize_log, "37x121");
    let cockpit_termios = cockpit.termios();
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    wait_for_file_contains(&fixture.resize_log, "44x220");
    cockpit.wait_for_output_after(transition_output, b"ALT_READY");
    cockpit.send(&[b'R', 0x03]);
    wait_for_file_byte_count(&fixture.input_log, "52", 1);
    wait_for_file_byte_count(&fixture.input_log, "03", 1);

    cockpit.resize(46, 210);
    wait_for_file_contains(&fixture.resize_log, "46x210");
    cockpit.resize(45, 77);
    wait_for_file_contains(&fixture.resize_log, "45x77");
    cockpit.wait_for_output_after(transition_output, b"LIVE_WRAP_END");
    cockpit.send(&[0x1d, b'd']);
    fixture.wait_for_phase("preview_reopened");
    wait_for_file_contains(&fixture.resize_log, "38x42");
    wait_for_terminal_snapshot(&fixture.host, &fixture.agent_id, 38, 42, 5, 10);

    assert_eq!(cockpit.termios(), cockpit_termios);
    let transition = cockpit.output_from(transition_output);
    assert!(contains_bytes(&transition, b"\x1b[?2004l"));
    assert!(contains_bytes(&transition, b"\x1b[?2004h"));
    assert!(
        contains_bytes(&transition, b"ALT_READY"),
        "managed raw transition output: {}",
        String::from_utf8_lossy(&transition)
    );
    let session_root = fixture
        .host
        .state_dir()
        .join("sessions")
        .join(&fixture.agent_id);
    let returned_snapshot_bytes = fs::read(session_root.join("terminal.snapshot.json"))
        .expect("returned structured snapshot");
    let returned_snapshot: Value =
        serde_json::from_slice(&returned_snapshot_bytes).expect("returned terminal snapshot json");
    assert_eq!(returned_snapshot["rows"], 38);
    assert_eq!(returned_snapshot["cols"], 42);
    assert_eq!(returned_snapshot["cursor_row"], 5);
    assert_eq!(returned_snapshot["cursor_col"], 10);
    assert_eq!(
        returned_snapshot["structured_screen"]["alternate_screen"],
        true
    );
    let durable_snapshot: DurableTerminalSnapshot =
        serde_json::from_slice(&returned_snapshot_bytes).expect("typed returned terminal snapshot");
    let structured = durable_snapshot
        .structured_screen
        .as_ref()
        .expect("returned structured screen");
    let replay = fs::read(session_root.join("pty.replay")).expect("returned raw replay ring");
    let suffix_start = usize::try_from(
        structured
            .source
            .pty_log_offset
            .saturating_sub(durable_snapshot.raw_replay_start_offset),
    )
    .expect("structured suffix start");
    let suffix_end = usize::try_from(
        durable_snapshot
            .pty_log_offset
            .saturating_sub(durable_snapshot.raw_replay_start_offset),
    )
    .expect("structured suffix end");
    let mut hydrated = TerminalEmulator::new(structured.rows, structured.cols, 4000);
    hydrated.adopt_screen_snapshot(structured);
    hydrated.process(
        replay
            .get(suffix_start..suffix_end)
            .expect("structured snapshot suffix is retained"),
    );
    let hydrated = hydrated.snapshot();
    assert_eq!((hydrated.cursor_row, hydrated.cursor_col), (5, 10));
    assert!(
        returned_snapshot["screen"]
            .to_string()
            .contains("RETURN_SIZE:38x42:\u{754c}"),
        "{returned_snapshot}"
    );
    fixture.assert_agent_running();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);

    let before = file_byte_count(&fixture.input_log, "4b");
    let live_output = cockpit.output_len();
    cockpit.send(b"K");
    wait_for_file_byte_count(&fixture.input_log, "4b", before + 1);
    cockpit.wait_for_output_after(live_output, b"BYTE:4b");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(file_byte_count(&fixture.input_log, "4b"), before + 1);

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn managed_raw_attach_external_sigint_returns_to_cockpit() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit = fixture.spawn_cockpit(&[]);
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    cockpit.wait_for_output_after(transition_output, b"ALT_READY");
    cockpit.signal(Signal::SIGINT);
    fixture.wait_for_phase("preview_reopened");

    fixture.assert_agent_running();
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
}

#[test]
fn managed_raw_attach_future_cancellation_joins_io_before_cockpit_return() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit = fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_CANCEL_MS", "150")]);
    let cockpit_termios = cockpit.termios();
    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    fixture.wait_for_phase("raw_inner_task_aborted");
    fixture.wait_for_phase("raw_cleanup_complete");
    fixture.wait_for_phase("preview_reopened");
    assert_eq!(cockpit.termios(), cockpit_termios);

    let before = file_byte_count(&fixture.input_log, "43");
    cockpit.send(b"C");
    wait_for_file_byte_count(&fixture.input_log, "43", before + 1);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(file_byte_count(&fixture.input_log, "43"), before + 1);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    fixture.assert_agent_running();

    assert_eq!(file_byte_count(&fixture.resize_log, "47x207"), 0);
    cockpit.resize(47, 207);
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        file_byte_count(&fixture.resize_log, "47x207"),
        0,
        "cancelled raw resize polling survived cockpit restoration"
    );

    cockpit.send(&[0x1d, b'd']);
    if !cockpit.exited_within(40) {
        cockpit.send(&[0x1d, b'd']);
        cockpit.wait_for_exit();
    }
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
}

#[test]
fn managed_raw_attach_one_worker_outer_drop_completes_cleanup_before_return() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit = fixture.spawn_cockpit(&[
        ("MILLMUX_TEST_CURRENT_THREAD_RUNTIME", "1"),
        ("MILLMUX_TEST_MANAGED_RAW_ABORT_OUTER", "1"),
    ]);
    let cockpit_termios = cockpit.termios();
    let before = file_byte_count(&fixture.input_log, "41");

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    fixture.wait_for_phase("raw_outer_transition_dropped");
    cockpit.send(b"A");
    fixture.wait_for_phase("raw_inner_task_aborted");
    fixture.wait_for_phase("raw_cleanup_complete");
    fixture.wait_for_phase("raw_outer_transition_cleanup_joined");
    fixture.wait_for_phase("terminal_resume_started");
    fixture.wait_for_phase("preview_reopened");
    fixture.assert_phase_order("raw_cleanup_complete", "terminal_resume_started");
    assert_eq!(cockpit.termios(), cockpit_termios);

    wait_for_file_byte_count(&fixture.input_log, "41", before + 1);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(file_byte_count(&fixture.input_log, "41"), before + 1);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    fixture.assert_agent_running();

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
}

#[test]
fn managed_raw_attach_stalled_negotiation_ctrl_c_restores_preview_and_terminal() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit =
        fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_ATTACH_RESPONSE_STALL", "1")]);
    let cockpit_termios = cockpit.termios();

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_attach_negotiation_waiting");
    cockpit.send(&[0x03]);
    fixture.wait_for_phase("preview_reopened");

    assert_eq!(cockpit.termios(), cockpit_termios);
    assert_eq!(file_byte_count(&fixture.input_log, "03"), 0);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    fixture.assert_agent_running();

    let before = file_byte_count(&fixture.input_log, "4e");
    cockpit.send(b"N");
    wait_for_file_byte_count(&fixture.input_log, "4e", before + 1);
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
}

#[test]
fn managed_raw_attach_stalled_fresh_status_sigint_keeps_preview_and_terminal() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit =
        fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_STATUS_RESPONSE_STALL", "1")]);
    let cockpit_termios = cockpit.termios();
    let preview_owner = session_input_owner(&fixture.host, &fixture.agent_id)
        .expect("preview must own input before fresh status validation");
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_fresh_status_waiting");
    cockpit.signal(Signal::SIGINT);
    fixture.wait_for_phase("raw_fresh_status_cancelled");

    assert_eq!(cockpit.termios(), cockpit_termios);
    let transition = cockpit.output_from(transition_output);
    assert!(!contains_bytes(&transition, b"\x1b[?2004l"));
    assert!(!contains_bytes(&transition, b"\x1b[?1049l"));
    assert_eq!(
        session_input_owner(&fixture.host, &fixture.agent_id).as_deref(),
        Some(preview_owner.as_str())
    );
    let before = file_byte_count(&fixture.input_log, "53");
    cockpit.send(b"S");
    wait_for_file_byte_count(&fixture.input_log, "53", before + 1);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(file_byte_count(&fixture.input_log, "53"), before + 1);

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn managed_raw_attach_silent_child_round_trips_through_snapshot_recovery() {
    let fixture = ManagedRawFixture::new_silent();
    let mut cockpit = fixture.spawn_cockpit(&[]);
    let cockpit_termios = cockpit.termios();

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    cockpit.send(&[0x1d, b'd']);
    fixture.wait_for_phase("preview_reopened");

    assert_eq!(cockpit.termios(), cockpit_termios);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    fixture.assert_agent_running();
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
}

#[test]
fn managed_raw_attach_full_stdout_deadline_restores_terminal_and_releases_ownership() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit =
        fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_STDOUT_TIMEOUT_MS", "250")]);
    let cockpit_termios = cockpit.termios();

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    cockpit.pause_output(true);
    cockpit.send(b"F");
    fixture.wait_for_phase("raw_stdout_write_timed_out");
    fixture.wait_for_phase("preview_reopened");
    cockpit.pause_output(false);

    assert_eq!(cockpit.termios(), cockpit_termios);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    fixture.assert_agent_running();
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    assert_eq!(session_input_owner(&fixture.host, &fixture.agent_id), None);
}

#[test]
fn managed_raw_attach_fresh_host_rejection_keeps_preview_and_terminal_untouched() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit = fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_STATUS_ERROR", "1")]);
    let cockpit_termios = cockpit.termios();
    let preview_owner = session_input_owner(&fixture.host, &fixture.agent_id)
        .expect("preview must own input before validation");
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    cockpit.wait_for_output_after(transition_output, b"validation");
    cockpit.wait_for_output_after(transition_output, b"error");

    assert_eq!(cockpit.termios(), cockpit_termios);
    let transition = cockpit.output_from(transition_output);
    assert!(!contains_bytes(&transition, b"\x1b[?2004l"));
    assert!(!contains_bytes(&transition, b"\x1b[?1049l"));
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    assert_eq!(
        session_input_owner(&fixture.host, &fixture.agent_id).as_deref(),
        Some(preview_owner.as_str())
    );

    let before = file_byte_count(&fixture.input_log, "56");
    cockpit.send(b"V");
    wait_for_file_byte_count(&fixture.input_log, "56", before + 1);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(file_byte_count(&fixture.input_log, "56"), before + 1);

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn managed_raw_attach_stalled_preview_close_sigint_preserves_terminal_and_recovers_preview() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit =
        fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_PREVIEW_CLOSE_TIMEOUT", "1")]);
    let cockpit_termios = cockpit.termios();
    let preview_owner = session_input_owner(&fixture.host, &fixture.agent_id)
        .expect("preview must own input before the close timeout");
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("preview_close_waiting");
    cockpit.signal(Signal::SIGINT);
    let replacement_owner =
        wait_for_replacement_input_owner(&fixture.host, &fixture.agent_id, &preview_owner);
    fixture.wait_for_phase("preview_reopened");

    assert_eq!(cockpit.termios(), cockpit_termios);
    let transition = cockpit.output_from(transition_output);
    assert!(!contains_bytes(&transition, b"\x1b[?2004l"));
    assert!(!contains_bytes(&transition, b"\x1b[?1049l"));
    assert_eq!(file_byte_count(&fixture.resize_log, "44x220"), 0);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    assert_ne!(replacement_owner, preview_owner);
    assert_eq!(
        session_input_owner(&fixture.host, &fixture.agent_id).as_deref(),
        Some(replacement_owner.as_str())
    );

    let before = file_byte_count(&fixture.input_log, "55");
    cockpit.send(b"U");
    wait_for_file_byte_count(&fixture.input_log, "55", before + 1);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(file_byte_count(&fixture.input_log, "55"), before + 1);
    fixture.assert_agent_running();

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
}

#[test]
fn managed_raw_attach_stalled_return_recovery_sigint_fails_closed_after_cleanup() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit =
        fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_RETURN_PREVIEW_STALL", "1")]);

    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    cockpit.send(&[0x1d, b'd']);
    fixture.wait_for_phase("return_preview_recovery_waiting");
    cockpit.signal(Signal::SIGINT);

    cockpit.wait_for_exit();
    assert_eq!(cockpit.termios(), cockpit.initial_termios);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn managed_raw_attach_context_persistence_rejection_keeps_preview_and_first_key() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit =
        fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_UI_CONTEXT_SET_ERROR", "1")]);
    let cockpit_termios = cockpit.termios();
    let preview_owner = session_input_owner(&fixture.host, &fixture.agent_id)
        .expect("preview must own input before persistence rejection");
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    cockpit.wait_for_output_after(transition_output, b"ui.context.set");

    assert_eq!(cockpit.termios(), cockpit_termios);
    let transition = cockpit.output_from(transition_output);
    assert!(!contains_bytes(&transition, b"\x1b[?2004l"));
    assert!(!contains_bytes(&transition, b"\x1b[?1049l"));
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
    assert_eq!(
        session_input_owner(&fixture.host, &fixture.agent_id).as_deref(),
        Some(preview_owner.as_str())
    );

    let before = file_byte_count(&fixture.input_log, "50");
    cockpit.send(b"P");
    wait_for_file_byte_count(&fixture.input_log, "50", before + 1);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(file_byte_count(&fixture.input_log, "50"), before + 1);

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn managed_raw_attach_persistent_snapshot_unavailable_fails_closed_and_exits() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit =
        fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_SNAPSHOT_UNAVAILABLE", "1")]);
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    cockpit.wait_for_output_after(transition_output, b"ALT_READY");
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_output_after(
        transition_output,
        b"managed raw return could not restore a coherent terminal snapshot",
    );
    cockpit.wait_for_output_after(transition_output, b"terminal snapshot is unavailable");
    cockpit.wait_for_exit();

    assert_eq!(cockpit.termios(), cockpit.initial_termios);
    let transition = cockpit.output_from(transition_output);
    assert!(contains_bytes(&transition, b"\x1b[?2004l"));
    let paste_enabled_at = transition
        .windows(b"\x1b[?2004h".len())
        .position(|window| window == b"\x1b[?2004h")
        .expect("bracketed paste restored after managed raw attach");
    assert!(contains_bytes(
        &transition[paste_enabled_at + b"\x1b[?2004h".len()..],
        b"\x1b[?2004l"
    ));
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    assert_eq!(session_input_owner(&fixture.host, &fixture.agent_id), None);
    fixture.assert_agent_running();
}

#[test]
fn cockpit_large_bracketed_paste_reaches_the_worker_in_bounded_lossless_frames() {
    let fixture = ManagedRawFixture::new_with_slow_input();
    let mut cockpit = fixture.spawn_cockpit(&[]);
    let payload_target = 8 * 1024 + 123;
    let mut payload = String::new();
    while payload.len() + 3 <= payload_target {
        let framed_offset = b"\x1b[200~".len() + payload.len();
        let boundary = (framed_offset / 512 + 1) * 512;
        let ascii = boundary.saturating_sub(framed_offset + 1);
        if payload.len() + ascii + "\u{754c}".len() > payload_target {
            break;
        }
        payload.push_str(&"a".repeat(ascii));
        payload.push('\u{754c}');
    }
    payload.push_str(&"a".repeat(payload_target - payload.len()));
    payload.push('\n');
    assert_eq!(payload.len(), payload_target + 1);

    let mut expected = b"\x1b[200~".to_vec();
    expected.extend_from_slice(payload.as_bytes());
    expected.extend_from_slice(b"\x1b[201~");
    let framed = std::str::from_utf8(&expected).expect("framed paste is UTF-8");
    let crossed_boundaries = (512..expected.len())
        .step_by(512)
        .filter(|boundary| !framed.is_char_boundary(*boundary))
        .count();
    assert!(crossed_boundaries >= 1, "crossed={crossed_boundaries}");
    cockpit.send(&expected);
    wait_for_input_log_bytes(&fixture.input_log, &expected);

    let received = input_log_bytes(&fixture.input_log);
    assert_eq!(received, expected);
    assert_eq!(
        received
            .windows(6)
            .filter(|window| *window == b"\x1b[200~")
            .count(),
        1
    );
    assert_eq!(
        received
            .windows(6)
            .filter(|window| *window == b"\x1b[201~")
            .count(),
        1
    );

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn cockpit_stalled_pty_paste_detach_releases_input_ownership() {
    let fixture = ManagedRawFixture::new_with_stalled_input();
    let mut cockpit = fixture.spawn_cockpit(&[]);

    let mut paste = b"\x1b[200~".to_vec();
    paste.extend(std::iter::repeat(b'x').take(64 * 1024));
    paste.extend_from_slice(b"\x1b[201~");
    let paste_len = paste.len();
    let paste_writer = cockpit.send_bounded_in_background(paste);
    assert_eq!(
        paste_writer.finish_within(Duration::from_secs(3)),
        paste_len,
        "the complete bracketed paste must reach the cockpit event reader"
    );

    cockpit.send(&[0x1d, b'd']);
    assert!(
        cockpit.exited_within(160),
        "cockpit did not detach after stalled PTY backpressure"
    );
    assert_eq!(cockpit.termios(), cockpit.initial_termios);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    assert_eq!(session_input_owner(&fixture.host, &fixture.agent_id), None);
    thread::sleep(Duration::from_millis(150));
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    assert_eq!(session_input_owner(&fixture.host, &fixture.agent_id), None);
    fixture.assert_agent_running();
}

#[test]
fn cockpit_resumable_stalled_pty_releases_writer_for_replacement_owner() {
    let fixture = ManagedRawFixture::new_with_resumable_input();
    let mut cockpit = fixture.spawn_cockpit(&[]);

    let mut paste = b"\x1b[200~".to_vec();
    paste.extend(std::iter::repeat(b'x').take(64 * 1024 - 1));
    paste.push(b'\n');
    paste.extend_from_slice(b"\x1b[201~");
    let expected = paste.clone();
    let paste_len = paste.len();
    let paste_writer = cockpit.send_bounded_in_background(paste);
    assert_eq!(
        paste_writer.finish_within(Duration::from_secs(3)),
        paste_len,
        "the complete bracketed paste must reach the cockpit event reader"
    );

    cockpit.send(&[0x1d, b'd']);
    fixture.wait_for_phase("preview_close_sent");
    fs::write(&fixture.resume_input, b"resume").expect("resume fixture input");
    assert!(
        cockpit.exited_within(160),
        "cockpit did not detach from the resumably stalled PTY"
    );
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    assert_eq!(session_input_owner(&fixture.host, &fixture.agent_id), None);
    wait_for_input_log_bytes(&fixture.input_log, &expected);

    let mut replacement = fixture.spawn_cockpit(&[]);
    replacement.send(b"R");
    let mut expected_after_replacement = expected;
    expected_after_replacement.push(b'R');
    wait_for_input_log_bytes(&fixture.input_log, &expected_after_replacement);
    wait_for_logs(&fixture.host, &fixture.agent_id, "RESUME_OUTPUT:R");

    replacement.send(&[0x1d, b'd']);
    replacement.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn managed_raw_attach_uses_controlling_tty_when_fd_zero_is_redirected() {
    let fixture = ManagedRawFixture::new();
    let mut cockpit = fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_REDIRECT_STDIN", "1")]);
    let cockpit_termios = cockpit.termios();
    let transition_output = cockpit.output_len();

    cockpit.send(&[0x1d, b'a']);
    cockpit.wait_for_output_after(transition_output, b"ALT_READY");
    let before = file_byte_count(&fixture.input_log, "54");
    cockpit.send(b"T");
    wait_for_file_byte_count(&fixture.input_log, "54", before + 1);
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_output_after(transition_output, b"Agent Terminal");

    assert_eq!(cockpit.termios(), cockpit_termios);
    assert_eq!(file_byte_count(&fixture.input_log, "1d"), 0);
    assert_eq!(file_byte_count(&fixture.input_log, "64"), 0);
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);

    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();
    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn managed_raw_attach_faults_share_cleanup_and_restore_first_cockpit_input_exactly_once() {
    let fixture = ManagedRawFixture::new();
    let frame_faults = [
        ("remote_close", b'D'),
        ("eof", b'E'),
        ("protocol_error", b'P'),
        ("read_error", b'G'),
        ("panic", b'H'),
    ];

    for (fault, first_key) in frame_faults {
        let mut cockpit = fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_FAULT", fault)]);
        let cockpit_termios = cockpit.termios();
        cockpit.send(&[0x1d, b'a']);
        fixture.wait_for_phase("raw_loop_entered");
        if fault == "panic" {
            fixture.wait_for_phase("raw_inner_task_panicked");
            fixture.wait_for_phase("raw_cleanup_complete");
        }
        fixture.wait_for_phase("preview_reopened");
        assert_eq!(cockpit.termios(), cockpit_termios, "{fault}");

        let hex = format!("{first_key:02x}");
        let before = file_byte_count(&fixture.input_log, &hex);
        cockpit.send(&[first_key]);
        wait_for_file_byte_count(&fixture.input_log, &hex, before + 1);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            file_byte_count(&fixture.input_log, &hex),
            before + 1,
            "{fault}"
        );
        wait_for_attached_clients(&fixture.host, &fixture.agent_id, 1);
        fixture.assert_agent_running();

        cockpit.send(&[0x1d, b'd']);
        cockpit.wait_for_exit();
        wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    }

    let mut cockpit = fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_FAULT", "write_error")]);
    let transition_output = cockpit.output_len();
    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    cockpit.wait_for_output_after(transition_output, b"ALT_READY");
    let rejected_before = file_byte_count(&fixture.input_log, "57");
    cockpit.send(b"W");
    fixture.wait_for_phase("preview_reopened");
    assert_eq!(file_byte_count(&fixture.input_log, "57"), rejected_before);
    let first_before = file_byte_count(&fixture.input_log, "49");
    cockpit.send(b"I");
    wait_for_file_byte_count(&fixture.input_log, "49", first_before + 1);
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();

    let mut cockpit = fixture.spawn_cockpit(&[("MILLMUX_TEST_MANAGED_RAW_FAULT", "resize_error")]);
    let transition_output = cockpit.output_len();
    cockpit.send(&[0x1d, b'a']);
    fixture.wait_for_phase("raw_loop_entered");
    cockpit.wait_for_output_after(transition_output, b"ALT_READY");
    cockpit.resize(48, 208);
    fixture.wait_for_phase("preview_reopened");
    let first_before = file_byte_count(&fixture.input_log, "4a");
    cockpit.send(b"J");
    wait_for_file_byte_count(&fixture.input_log, "4a", first_before + 1);
    cockpit.send(&[0x1d, b'd']);
    cockpit.wait_for_exit();

    wait_for_attached_clients(&fixture.host, &fixture.agent_id, 0);
    fixture.assert_agent_running();
}

#[test]
fn cli_follow_logs_and_events_stream_late_output() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let logs_session_id = start_session(
        &host,
        workspace.path(),
        "logs-follow",
        "printf 'first\\n'; sleep 1; printf 'second\\n'",
    );
    wait_for_logs(&host, &logs_session_id, "first");

    let logs = millmux_command(&host)
        .args(["logs", &logs_session_id, "--follow"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs = String::from_utf8_lossy(&logs);
    assert!(logs.contains("first"), "{logs}");
    assert!(logs.contains("second"), "{logs}");

    let events_session_id = start_session(
        &host,
        workspace.path(),
        "events-follow",
        "printf 'event-first\\n'; sleep 1; printf 'event-second\\n'",
    );
    wait_for_logs(&host, &events_session_id, "event-first");

    let events = millmux_command(&host)
        .args(["events", &events_session_id, "--follow", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = String::from_utf8_lossy(&events);
    let frames = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event follow json line"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .first()
            .and_then(|frame| frame.get("events"))
            .and_then(Value::as_array)
            .is_some(),
        "{events}"
    );
    assert!(
        frames
            .iter()
            .skip(1)
            .any(|frame| { frame["type"] == "event" && frame["event"]["kind"] == "output" }),
        "{events}"
    );
}

fn assert_cli_json_attach_state_consistency(
    host: &TempHost,
    session_id: &str,
    attached_clients: u64,
    input_owner: Value,
) {
    let listed = millmux_command(host)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: Value = serde_json::from_slice(&listed).expect("list json");
    let listed_session = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["session_id"] == session_id)
        .unwrap_or_else(|| panic!("missing session {session_id} in {listed:#}"));

    let status = millmux_command(host)
        .args(["status", session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status).expect("status json");

    let inspect = millmux_command(host)
        .args(["inspect", session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect: Value = serde_json::from_slice(&inspect).expect("inspect json");

    for session in [listed_session, &status["session"], &inspect["session"]] {
        assert_eq!(session["attached_clients"], attached_clients, "{session:#}");
        assert_eq!(session["input_owner"], input_owner, "{session:#}");
    }
}

fn start_session(host: &TempHost, workspace: &Path, name: &str, script: &str) -> String {
    start_session_with_role(host, workspace, name, "shell", script)
}

fn start_session_with_role(
    host: &TempHost,
    workspace: &Path,
    name: &str,
    role: &str,
    script: &str,
) -> String {
    start_session_argv(
        host,
        workspace,
        name,
        role,
        &["sh".to_string(), "-c".to_string(), script.to_string()],
    )
}

fn start_session_argv(
    host: &TempHost,
    workspace: &Path,
    name: &str,
    role: &str,
    argv: &[String],
) -> String {
    let mut command = millmux_command(host);
    command
        .args([
            "start",
            "--json",
            "--name",
            name,
            "--role",
            role,
            "--workspace",
        ])
        .arg(workspace)
        .arg("--cwd")
        .arg(workspace)
        .arg("--")
        .args(argv);
    let output = command.assert().success().get_output().stdout.clone();
    let value: Value = serde_json::from_slice(&output).expect("start json");
    value["session"]["session_id"].as_str().unwrap().to_string()
}

fn wait_for_logs(host: &TempHost, session_id: &str, needle: &str) {
    for _ in 0..120 {
        let output = millmux_command(host)
            .args(["logs", session_id])
            .output()
            .expect("run logs");
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("logs for {session_id} did not contain {needle:?}");
}

fn wait_for_attach_output(host: &TempHost, session_id: &str, needle: &str) {
    let mut last_output = String::new();
    for _ in 0..120 {
        let output = millmux_command(host)
            .args(["attach", session_id, "--read-only"])
            .output()
            .expect("run attach");
        last_output = String::from_utf8_lossy(&output.stdout).to_string();
        if output.status.success() && last_output.contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("attach output for {session_id} did not contain {needle:?}: {last_output}");
}

fn wait_for_attached_clients(host: &TempHost, session_id: &str, expected: u64) {
    for _ in 0..120 {
        let output = millmux_command(host)
            .args(["status", session_id, "--json"])
            .output()
            .expect("run status");
        if output.status.success() {
            let value: Value = serde_json::from_slice(&output.stdout).expect("status json");
            if value["session"]["attached_clients"] == expected {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("status for {session_id} did not report attached_clients={expected}");
}

fn fixture_session_processes(host: &TempHost, session_id: &str) -> Option<FixtureSessionProcesses> {
    let meta = fs::read(
        host.state_dir()
            .join("sessions")
            .join(session_id)
            .join("meta.json"),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())?;
    Some(FixtureSessionProcesses {
        worker_pid: meta["worker_pid"]
            .as_u64()
            .and_then(|pid| pid.try_into().ok()),
        child_pid: meta["child_pid"]
            .as_u64()
            .and_then(|pid| pid.try_into().ok()),
        child_pgid: meta["child_pgid"]
            .as_u64()
            .and_then(|pid| pid.try_into().ok()),
    })
}

fn stop_fixture_processes(processes: &[FixtureSessionProcesses]) {
    if wait_for_fixture_processes(processes, 80) {
        return;
    }

    for process in processes {
        if let Some(pgid) = process.child_pgid.and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
        } else if let Some(pid) = process.child_pid.and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        if let Some(pid) = process.worker_pid.and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
    let _ = wait_for_fixture_processes(processes, 80);
}

fn wait_for_fixture_processes(processes: &[FixtureSessionProcesses], attempts: usize) -> bool {
    for _ in 0..attempts {
        if live_fixture_process_count(processes) == 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    live_fixture_process_count(processes) == 0
}

fn live_fixture_process_count(processes: &[FixtureSessionProcesses]) -> usize {
    processes
        .iter()
        .flat_map(|process| [process.worker_pid, process.child_pid])
        .flatten()
        .filter(|pid| fixture_process_is_running(*pid))
        .count()
}

fn fixture_process_is_running(pid: u32) -> bool {
    i32::try_from(pid)
        .ok()
        .is_some_and(|pid| kill(Pid::from_raw(pid), None).is_ok())
}

fn session_input_owner(host: &TempHost, session_id: &str) -> Option<String> {
    let output = millmux_command(host)
        .args(["status", session_id, "--json"])
        .output()
        .expect("run status for input owner");
    assert!(output.status.success(), "{:?}", output);
    let value: Value = serde_json::from_slice(&output.stdout).expect("status json");
    value["session"]["input_owner"].as_str().map(str::to_string)
}

fn wait_for_replacement_input_owner(
    host: &TempHost,
    session_id: &str,
    previous_owner: &str,
) -> String {
    for _ in 0..120 {
        if let Some(owner) = session_input_owner(host, session_id) {
            if owner != previous_owner {
                return owner;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("status for {session_id} did not report a replacement input owner");
}

fn wait_for_file_contains(path: &Path, needle: &str) {
    for _ in 0..240 {
        if fs::read_to_string(path).is_ok_and(|contents| contents.contains(needle)) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{} did not contain {needle:?}: {:?}",
        path.display(),
        fs::read_to_string(path).ok()
    );
}

fn wait_for_terminal_snapshot(
    host: &TempHost,
    session_id: &str,
    rows: u64,
    cols: u64,
    cursor_row: u64,
    cursor_col: u64,
) {
    let path = host
        .state_dir()
        .join("sessions")
        .join(session_id)
        .join("terminal.snapshot.json");
    for _ in 0..240 {
        let matches = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|snapshot| {
                snapshot["rows"] == rows
                    && snapshot["cols"] == cols
                    && snapshot["cursor_row"] == cursor_row
                    && snapshot["cursor_col"] == cursor_col
            });
        if matches {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{} did not reach {rows}x{cols} cursor {cursor_row},{cursor_col}: {:?}",
        path.display(),
        fs::read_to_string(&path).ok()
    );
}

fn file_byte_count(path: &Path, hex_byte: &str) -> usize {
    fs::read_to_string(path)
        .map(|contents| contents.lines().filter(|line| *line == hex_byte).count())
        .unwrap_or(0)
}

fn input_log_bytes(path: &Path) -> Vec<u8> {
    fs::read_to_string(path)
        .map(|contents| {
            contents
                .lines()
                .map(|line| u8::from_str_radix(line, 16).expect("input log byte"))
                .collect()
        })
        .unwrap_or_default()
}

fn wait_for_input_log_bytes(path: &Path, expected: &[u8]) {
    for _ in 0..480 {
        if input_log_bytes(path) == expected {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let actual = input_log_bytes(path);
    let mismatch = actual
        .iter()
        .zip(expected)
        .position(|(actual, expected)| actual != expected);
    panic!(
        "{} did not receive expected input bytes: actual_len={}, expected_len={}, first_mismatch={mismatch:?}",
        path.display(),
        actual.len(),
        expected.len()
    );
}

fn wait_for_file_byte_count(path: &Path, hex_byte: &str, expected: usize) {
    for _ in 0..240 {
        if file_byte_count(path, hex_byte) == expected {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{} did not contain {expected} occurrences of {hex_byte}: {:?}",
        path.display(),
        fs::read_to_string(path).ok()
    );
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn millmux_command(host: &TempHost) -> Command {
    let mut command = Command::cargo_bin("millmux").expect("millmux binary");
    command.env("MILLMUX_STATE_DIR", host.state_dir());
    command.env(
        "MILLMUX_HOST_BIN",
        binary_override("MILLMUX_HOST_BIN", "millrace-sessiond"),
    );
    command.env(
        "MILLMUX_WORKER_BIN",
        binary_override("MILLMUX_WORKER_BIN", "millrace-session-worker"),
    );
    command
}

fn binary_override(name: &str, binary_name: &str) -> std::path::PathBuf {
    if let Some(value) = std::env::var_os(name) {
        let path = std::path::PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
        return workspace_root().join(path);
    }

    workspace_root()
        .join("target")
        .join("debug")
        .join(binary_name)
}

fn resolve_path_executable(name: &str) -> String {
    let path = std::env::var_os("PATH").expect("PATH is set for attach fixture");
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            let resolved =
                fs::canonicalize(&candidate).expect("canonicalize attach fixture executable");
            return resolved
                .to_str()
                .expect("attach fixture executable path is UTF-8")
                .to_string();
        }
    }
    panic!("{name} was not found on PATH for attach fixture");
}

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}
