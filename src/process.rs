//! Argv-only subprocess runner with timeout, per-stream caps and lossy UTF-8.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::model::RUN_MAX_BUFFER;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// Exit code; `None` when terminated by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// True when the process was killed (timeout or output cap exceeded).
    pub killed: bool,
}

/// Runs `file` with `args` (no shell). Spawn failures (missing binary, EACCES)
/// return `Err`; non-zero exits, timeouts and overflows return `Ok`.
pub fn run(
    file: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
    stdin: Option<&str>,
) -> io::Result<RunResult> {
    let mut command = Command::new(file);
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so a timeout kill also reaches grandchildren
        // holding the pipes open.
        .process_group(0);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn()?;
    let pid = child.id() as libc::pid_t;
    let overflow = Arc::new(AtomicBool::new(false));

    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let text = text.to_owned();
        thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    let out = spawn_reader(child.stdout.take(), Arc::clone(&overflow), pid);
    let err = spawn_reader(child.stderr.take(), Arc::clone(&overflow), pid);

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    // The direct child may exit while a grandchild still holds the inherited
    // pipes, so completion also requires both readers to reach EOF.
    let mut exited: Option<ExitStatus> = None;
    let status = loop {
        if exited.is_none() {
            exited = child.try_wait()?;
        }
        if let Some(status) = exited
            && out.is_finished()
            && err.is_finished()
        {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(pid);
            break match exited {
                Some(status) => status,
                None => child.wait()?,
            };
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let killed = timed_out || overflow.load(Ordering::SeqCst) || status.code().is_none();
    Ok(RunResult {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        killed,
    })
}

fn kill_group(pid: libc::pid_t) {
    // SAFETY: plain syscall; the group id is our child's pid.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    overflow: Arc<AtomicBool>,
    pid: libc::pid_t,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let Some(mut pipe) = pipe else { return buf };
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = RUN_MAX_BUFFER - buf.len();
                    if n > room {
                        buf.extend_from_slice(&chunk[..room]);
                        overflow.store(true, Ordering::SeqCst);
                        kill_group(pid);
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let r = run(
            "sh",
            &["-c", "echo out; echo err >&2; exit 3"],
            None,
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert_eq!(r.code, Some(3));
        assert_eq!(r.stdout, "out\n");
        assert_eq!(r.stderr, "err\n");
        assert!(!r.killed);
    }

    #[test]
    fn passes_stdin_and_cwd() {
        let r = run("cat", &[], None, Duration::from_secs(5), Some("hello")).unwrap();
        assert_eq!(r.stdout, "hello");
        let r = run(
            "pwd",
            &[],
            Some(Path::new("/")),
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert_eq!(r.stdout.trim(), "/");
    }

    #[test]
    fn timeout_kills_process_group() {
        let start = Instant::now();
        let r = run(
            "sh",
            &["-c", "sleep 30 & sleep 30"],
            None,
            Duration::from_millis(200),
            None,
        )
        .unwrap();
        assert!(r.killed);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn exited_parent_with_inherited_pipes_times_out() {
        let start = Instant::now();
        let r = run(
            "sh",
            &["-c", "sleep 10 & printf token"],
            None,
            Duration::from_millis(300),
            None,
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(r.stdout, "token");
        assert_eq!(r.code, Some(0));
        assert!(r.killed);
    }

    #[test]
    fn exited_parent_with_stderr_only_times_out() {
        let start = Instant::now();
        let r = run(
            "sh",
            &["-c", "sleep 10 >/dev/null & printf token"],
            None,
            Duration::from_millis(300),
            None,
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(r.stdout, "token");
        assert_eq!(r.code, Some(0));
        assert!(r.killed);
    }

    #[test]
    fn output_cap_still_kills_and_bounds_output() {
        let r = run("sh", &["-c", "yes x"], None, Duration::from_secs(5), None).unwrap();
        assert!(r.killed);
        assert_eq!(r.stdout.len(), RUN_MAX_BUFFER);
    }

    #[test]
    fn missing_binary_is_spawn_error() {
        assert!(
            run(
                "/nonexistent/jira-tern-bin",
                &[],
                None,
                Duration::from_secs(1),
                None
            )
            .is_err()
        );
    }
}
