//! Execute trusted local scripts without allowing their output to corrupt the CLI protocol.
//!
//! Each attempt owns a process group and a single deadline. Nonblocking pipes make
//! the deadline apply even when a script does not read stdin or a descendant keeps
//! stdout open after the shell exits. Script side effects are still at-least-once:
//! they cannot be committed atomically with the workflow's SQLite transaction.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{path::Path, time::Duration};

const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Run a workflow command, retrying failed attempts `retry` times.
///
/// `SKFLOW_ATTEMPT` is one-based and starts again at one on a later CLI invocation.
/// A successful command must emit exactly one JSON value to stdout. Diagnostics
/// belong on stderr. Every attempt has its own full timeout budget.
pub fn run(
    command: &str,
    cwd: &Path,
    input: &Value,
    timeout: Duration,
    retry: u32,
) -> Result<Value> {
    if timeout.is_zero() {
        bail!("script timeout must be greater than zero");
    }
    let mut bytes = serde_json::to_vec(input).context("serialize script stdin")?;
    bytes.push(b'\n');
    let attempts = u64::from(retry) + 1;
    let mut failure = None;
    for attempt in 1..=attempts {
        match run_once(command, cwd, &bytes, timeout, attempt) {
            Ok(value) => return Ok(value),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.expect("at least one attempt"))
        .with_context(|| format!("script failed after {attempts} attempt(s)"))
}

#[cfg(not(unix))]
fn run_once(
    _command: &str,
    _cwd: &Path,
    _input: &[u8],
    _timeout: Duration,
    _attempt: u64,
) -> Result<Value> {
    bail!("local script execution currently requires a Unix host with /bin/sh")
}

#[cfg(unix)]
fn run_once(
    command: &str,
    cwd: &Path,
    input: &[u8],
    timeout: Duration,
    attempt: u64,
) -> Result<Value> {
    use std::{
        io::{ErrorKind, Write},
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Command, Stdio},
        time::Instant,
    };

    let deadline = Instant::now()
        .checked_add(timeout)
        .context("script timeout is too large")?;
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("SKFLOW_ATTEMPT", attempt.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("spawn workflow script")?;
    let mut child = ProcessGroup::new(child);
    let mut stdin = Some(child.child.stdin.take().context("open script stdin")?);
    let mut stdout = Some(child.child.stdout.take().context("open script stdout")?);
    let mut stderr = Some(child.child.stderr.take().context("open script stderr")?);
    nonblocking(stdin.as_ref().unwrap().as_raw_fd())?;
    nonblocking(stdout.as_ref().unwrap().as_raw_fd())?;
    nonblocking(stderr.as_ref().unwrap().as_raw_fd())?;

    let mut written = 0;
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let mut status = None;
    loop {
        if Instant::now() >= deadline {
            bail!("script timed out after {timeout:?}");
        }
        drain(&mut stdout, &mut output, MAX_STDOUT_BYTES, "stdout")?;
        drain(&mut stderr, &mut diagnostics, MAX_STDERR_BYTES, "stderr")?;

        if let Some(pipe) = stdin.as_mut() {
            // Do at most one write per turn so a large stdin cannot starve the
            // deadline or output pipes. Partial writes resume on the next turn.
            match pipe.write(&input[written..]) {
                Ok(count) => written += count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                // A script may legitimately produce a constant result without
                // consuming stdin. Its exit status and JSON remain authoritative.
                Err(error) if error.kind() == ErrorKind::BrokenPipe => written = input.len(),
                Err(error) => return Err(error).context("write script stdin"),
            }
            if written == input.len() {
                stdin.take();
            }
        }

        if status.is_none() {
            status = child.child.try_wait().context("wait for workflow script")?;
        }
        if let Some(exit_status) = status {
            stdin.take();
            if stdout.is_none() && stderr.is_none() {
                // Also clean up detached background work in the same process
                // group, even when it closed its output pipes before shell exit.
                child.finish();
                if !exit_status.success() {
                    let message = String::from_utf8_lossy(&diagnostics);
                    bail!(
                        "script exited with {exit_status}; stderr: {}",
                        message.trim()
                    );
                }
                return serde_json::from_slice(&output).with_context(|| {
                    format!(
                        "script stdout must contain exactly one JSON value; stderr: {}",
                        String::from_utf8_lossy(&diagnostics).trim()
                    )
                });
            }
        }

        let mut descriptors = Vec::with_capacity(3);
        if let Some(pipe) = stdin.as_ref() {
            descriptors.push(poll_descriptor(pipe.as_raw_fd(), libc::POLLOUT));
        }
        if let Some(pipe) = stdout.as_ref() {
            descriptors.push(poll_descriptor(pipe.as_raw_fd(), libc::POLLIN));
        }
        if let Some(pipe) = stderr.as_ref() {
            descriptors.push(poll_descriptor(pipe.as_raw_fd(), libc::POLLIN));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        // Short polls also notice child exit when all output descriptors closed.
        let milliseconds = remaining.as_millis().clamp(1, 25) as libc::c_int;
        // SAFETY: descriptors contains initialized pollfd values and remains
        // alive/unmoved for the duration of this blocking syscall.
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                milliseconds,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != ErrorKind::Interrupted {
                return Err(error).context("poll script pipes");
            }
        }
    }
}

#[cfg(unix)]
fn poll_descriptor(fd: libc::c_int, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

#[cfg(unix)]
fn nonblocking(fd: libc::c_int) -> Result<()> {
    // SAFETY: the descriptor is owned by a live Child pipe in run_once.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("read script pipe flags");
    }
    // SAFETY: same live descriptor, with original flags preserved.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("set nonblocking script pipe");
    }
    Ok(())
}

#[cfg(unix)]
fn drain<R: std::io::Read>(
    pipe: &mut Option<R>,
    bytes: &mut Vec<u8>,
    limit: usize,
    name: &str,
) -> Result<()> {
    use std::io::ErrorKind;
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    let mut buffer = [0; 8192];
    // Limit each turn as well as total allocation so an endlessly writing child
    // cannot hold this read loop and prevent the deadline from being checked.
    for _ in 0..16 {
        match reader.read(&mut buffer) {
            Ok(0) => {
                pipe.take();
                break;
            }
            Ok(count) => {
                if bytes.len() + count > limit {
                    bail!("script {name} exceeds {limit} byte limit");
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => return Err(error).with_context(|| format!("read script {name}")),
        }
    }
    Ok(())
}

#[cfg(unix)]
struct ProcessGroup {
    child: std::process::Child,
    group: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessGroup {
    fn new(child: std::process::Child) -> Self {
        let group = Some(child.id() as libc::pid_t);
        Self { child, group }
    }

    fn finish(&mut self) {
        if let Some(group) = self.group.take() {
            // SAFETY: the child was started as leader of its own process group;
            // the negative ID addresses only that group, never the CLI's group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            // kill() also covers an unusual shell that changed its own group.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, time::Instant};
    use tempfile::tempdir;

    #[test]
    fn transports_json_and_keeps_stderr_separate() {
        let directory = tempdir().unwrap();
        let value = json!({"message": "literal $(touch dangerous) ' \"", "count": 3});
        let result = run(
            "echo diagnostic >&2; cat",
            directory.path(),
            &value,
            Duration::from_secs(2),
            0,
        )
        .unwrap();
        assert_eq!(result, value);
        assert!(!directory.path().join("dangerous").exists());
    }

    #[test]
    fn retries_nonzero_exit_and_exposes_attempt() {
        let directory = tempdir().unwrap();
        let value = run(
            "printf x >> attempts; if [ \"$SKFLOW_ATTEMPT\" = 1 ]; then echo not-ready >&2; exit 7; fi; printf '{\"ok\":true}'",
            directory.path(),
            &Value::Null,
            Duration::from_secs(2),
            1,
        )
        .unwrap();
        assert_eq!(value, json!({"ok": true}));
        assert_eq!(fs::read(directory.path().join("attempts")).unwrap(), b"xx");
    }

    #[test]
    fn rejects_nonzero_and_multiple_json_values() {
        let directory = tempdir().unwrap();
        let error = run(
            "echo context >&2; printf '{}'; exit 3",
            directory.path(),
            &Value::Null,
            Duration::from_secs(2),
            0,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("context"));
        let error = run(
            "printf '{} {}'",
            directory.path(),
            &Value::Null,
            Duration::from_secs(2),
            0,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("exactly one JSON"));
    }

    #[test]
    fn timeout_covers_blocked_stdin_and_descendant_pipes() {
        let directory = tempdir().unwrap();
        let input = Value::String("x".repeat(1024 * 1024));
        for command in ["sleep 10", "sleep 10 & printf '{}'"] {
            let start = Instant::now();
            let error = run(
                command,
                directory.path(),
                &input,
                Duration::from_millis(120),
                0,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("timed out"));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn timeout_kills_descendant_before_delayed_side_effect() {
        let directory = tempdir().unwrap();
        let error = run(
            "(sleep 0.4; touch escaped) & wait",
            directory.path(),
            &Value::Null,
            Duration::from_millis(100),
            0,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        std::thread::sleep(Duration::from_millis(500));
        assert!(!directory.path().join("escaped").exists());
    }

    #[test]
    fn refuses_unbounded_stderr() {
        let directory = tempdir().unwrap();
        let error = run(
            "yes diagnostic >&2",
            directory.path(),
            &Value::Null,
            Duration::from_secs(2),
            0,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("stderr exceeds"));
    }
}
