//! Runs external commands with a time limit.
//!
//! Every child starts in its own process group. On timeout the whole group gets
//! SIGTERM, then SIGKILL if it is still alive 5 seconds later, so a script's own
//! children die with it. Output is kept to the last `cap` bytes per stream so a
//! chatty command cannot exhaust memory; callers that must see all output (git
//! listings Picard parses for safety decisions) pass a large cap and treat
//! `truncated` as an error.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Enough for a script's log tail.
pub const SCRIPT_OUTPUT_CAP: usize = 64 * 1024;
const POLL: Duration = Duration::from_millis(20);
const KILL_GRACE: Duration = Duration::from_secs(5);
// Readers finish when every process holding the pipe exits. A grandchild that
// escaped the process group could hold it open forever, so reading is bounded.
const READ_GRACE: Duration = Duration::from_secs(2);

/// What a finished (or killed) command produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Exit code, or `None` if a signal ended the process.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// True if either stream exceeded the cap and lost its beginning.
    pub truncated: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// A short description of a failure for logs and reports.
    pub fn failure_summary(&self, timeout: Duration) -> String {
        let tail = last_lines(&self.stderr, 3);
        let what = if self.timed_out {
            format!("timed out after {}s", timeout.as_secs())
        } else {
            match self.code {
                Some(c) => format!("exit {c}"),
                None => "killed by a signal".to_string(),
            }
        };
        if tail.is_empty() {
            what
        } else {
            format!("{what}: {tail}")
        }
    }
}

/// Runs `cmd` to completion or until `timeout`, keeping the last `cap` bytes of
/// stdout and of stderr. stdin is closed so a command waiting for input cannot hang.
pub fn run(mut cmd: Command, timeout: Duration, cap: usize) -> io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let stdout = spawn_reader(child.stdout.take(), cap);
    let stderr = spawn_reader(child.stderr.take(), cap);

    let (status, timed_out) = match wait_until(&mut child, Instant::now() + timeout)? {
        Some(status) => (status, false),
        None => (terminate_group(&mut child)?, true),
    };
    let (stdout, out_cut) = collect(stdout);
    let (stderr, err_cut) = collect(stderr);
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
        timed_out,
        truncated: out_cut || err_cut,
    })
}

fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

fn terminate_group(child: &mut Child) -> io::Result<ExitStatus> {
    signal_group(child, libc::SIGTERM);
    if let Some(status) = wait_until(child, Instant::now() + KILL_GRACE)? {
        // The leader exited; make sure nothing it started is left behind.
        signal_group(child, libc::SIGKILL);
        return Ok(status);
    }
    signal_group(child, libc::SIGKILL);
    child.wait()
}

fn signal_group(child: &Child, signal: libc::c_int) {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        return;
    };
    // SAFETY: kill with a negative pid signals the process group we created via
    // process_group(0). ESRCH (group already gone) is expected and ignored.
    unsafe {
        libc::kill(-pgid, signal);
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: Option<R>,
    cap: usize,
) -> mpsc::Receiver<(String, bool)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Some(mut stream) = stream else {
            let _ = tx.send((String::new(), false));
            return;
        };
        let mut kept: Vec<u8> = Vec::new();
        let mut cut = false;
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    kept.extend_from_slice(&buf[..n]);
                    if kept.len() > cap {
                        let excess = kept.len() - cap;
                        kept.drain(..excess);
                        cut = true;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // The receiver may have given up waiting; nothing to do then.
        let _ = tx.send((String::from_utf8_lossy(&kept).into_owned(), cut));
    });
    rx
}

/// The stream's text and whether it was cut. A reader that never reports back
/// (its pipe held open by an escaped grandchild) counts as cut, so callers that
/// need complete output do not trust an empty string.
fn collect(rx: mpsc::Receiver<(String, bool)>) -> (String, bool) {
    rx.recv_timeout(READ_GRACE)
        .unwrap_or_else(|_| (String::new(), true))
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn captures_output_and_exit_code() {
        let out = run(
            sh("echo hi; echo oops >&2; exit 3"),
            Duration::from_secs(5),
            SCRIPT_OUTPUT_CAP,
        )
        .unwrap();
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "oops\n");
        assert_eq!(out.code, Some(3));
        assert!(!out.success());
        assert_eq!(out.failure_summary(Duration::from_secs(5)), "exit 3: oops");
    }

    #[test]
    fn timeout_kills_the_child_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let started = Instant::now();
        let out = run(sh(&script), Duration::from_millis(300), SCRIPT_OUTPUT_CAP).unwrap();
        assert!(out.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            out.failure_summary(Duration::from_millis(300))
                .starts_with("timed out")
        );

        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        thread::sleep(Duration::from_millis(100));
        // SAFETY: signal 0 only checks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "grandchild {pid} survived the timeout");
    }

    #[test]
    fn keeps_only_the_tail_of_large_output() {
        let out = run(
            sh("yes x | head -c 200000; echo END"),
            Duration::from_secs(10),
            SCRIPT_OUTPUT_CAP,
        )
        .unwrap();
        assert!(out.stdout.len() <= SCRIPT_OUTPUT_CAP);
        assert!(out.stdout.ends_with("END\n"));
        assert!(out.truncated);
    }

    #[test]
    fn stdin_is_closed() {
        let out = run(
            sh("cat; echo done"),
            Duration::from_secs(5),
            SCRIPT_OUTPUT_CAP,
        )
        .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "done\n");
    }

    #[test]
    fn missing_program_is_an_error() {
        assert!(run(Command::new("/no/such/program"), Duration::from_secs(1), 1).is_err());
    }
}
