//! Process execution for typed capabilities.
//!
//! `git.status()` and `testing.run(...)` both need to run a real program, but
//! neither accepts a command string from the model. The model supplies
//! structured arguments; the capability decides which binary runs and which
//! arguments are allowed. That is the whole point of having typed capabilities
//! instead of an `exec`.
//!
//! Every child is started in its own process group and killed as a group on
//! timeout or cancellation. Killing only the direct child would leave whatever
//! it spawned running: `cargo test` forks test binaries, and an orphaned test
//! holding a lock is worse than a failed test.

use std::process::Stdio;
use std::time::Instant;

use dex_protocol::OutputStream;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Duration;

use super::{CapabilityCtx, CapabilityError};
use crate::capability::CapabilityErrorKind;

#[derive(Clone, Debug)]
pub struct ProcessOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub truncated: bool,
}

impl ProcessOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Run `program` with `args` in the session working directory, bounded by
/// `limit` and by cancellation.
///
/// `args` must already be validated by the calling capability. Nothing here
/// interprets a string as a shell, so there is no quoting or metacharacter
/// surface to get wrong.
///
/// The deadline is enforced inside the wait loop rather than by wrapping this
/// call in a timeout. That distinction is load-bearing: abandoning the future
/// would drop the `Child`, and `kill_on_drop` signals only the direct child, so
/// whatever it spawned would be orphaned. Handling the deadline here means every
/// exit path — success, timeout, cancellation — passes through the same
/// process-group kill.
pub async fn run(
    ctx: &CapabilityCtx,
    program: &str,
    args: &[String],
    limit: Duration,
) -> Result<ProcessOutcome, CapabilityError> {
    ctx.emit_process_started(program, args);

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(ctx.working_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A fresh process group means the whole tree can be signalled at once.
    #[cfg(unix)]
    {
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|e| {
        CapabilityError::new(
            CapabilityErrorKind::OperationFailed,
            format!("could not start {program}: {e}"),
        )
    })?;
    let pid = child.id();

    let cap = ctx.budget.limits().output_bytes as usize;
    let (tx, mut rx) = mpsc::channel::<(OutputStream, Vec<u8>)>(16);
    if let Some(pipe) = child.stdout.take() {
        spawn_reader(pipe, OutputStream::Stdout, tx.clone());
    }
    if let Some(pipe) = child.stderr.take() {
        spawn_reader(pipe, OutputStream::Stderr, tx);
    }

    // Drain pipe output while the child runs, emitting as it arrives.
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut truncated = false;
    let started = Instant::now();
    // Absolute, not a relative duration. The wait loop ticks every 25ms and
    // would otherwise rebuild a fresh full-length sleep on each tick, so the
    // deadline would never arrive.
    let deadline = tokio::time::Instant::now() + limit;

    let status = loop {
        // Reap the child as soon as it exits, then keep draining whatever is
        // still buffered in the pipes.
        if let Some(status) = child.try_wait()? {
            break status;
        }
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                kill_group(pid).await;
                let _ = child.wait().await;
                return Err(CapabilityError::cancelled(format!("{program} was cancelled")));
            }
            _ = tokio::time::sleep_until(deadline) => {
                // Same group kill as cancellation: whatever the child spawned
                // dies with it, so a timed-out `cargo test` leaves no orphaned
                // test binaries behind.
                kill_group(pid).await;
                let _ = child.wait().await;
                return Err(CapabilityError::new(
                    CapabilityErrorKind::Timeout,
                    format!("{program} exceeded its {}ms deadline", limit.as_millis()),
                ));
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            Some(chunk) = rx.recv() => {
                let (stream, bytes) = chunk;
                let buffer = match stream {
                    OutputStream::Stdout => &mut stdout,
                    OutputStream::Stderr => &mut stderr,
                };
                // Stop retaining once the cap is reached, but keep reporting
                // that the output was cut rather than truncating silently.
                if buffer.len() < cap {
                    let room = cap - buffer.len();
                    let take = room.min(bytes.len());
                    buffer.extend_from_slice(&bytes[..take]);
                    truncated |= bytes.len() > take;
                    ctx.emit_process_output(stream, String::from_utf8_lossy(&bytes[..take]).into_owned());
                } else {
                    truncated = true;
                }
            }
        }
    };

    // Collect anything still buffered in the pipes after the child exited.
    while let Ok((stream, bytes)) = rx.try_recv() {
        match stream {
            OutputStream::Stdout => stdout.extend(bytes),
            OutputStream::Stderr => stderr.extend(bytes),
        }
    }
    // Let the reader tasks observe EOF and finish.
    drop(rx);

    let duration_ms = started.elapsed().as_millis() as u64;
    let exit_code = status.code();
    ctx.emit_process_finished(exit_code, duration_ms, truncated);

    Ok(ProcessOutcome {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        duration_ms,
        truncated,
    })
}

/// Pump one pipe into the channel until EOF.
fn spawn_reader<R>(mut pipe: R, stream: OutputStream, tx: mpsc::Sender<(OutputStream, Vec<u8>)>)
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send((stream, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Signal the child's whole process group.
#[cfg(unix)]
async fn kill_group(pid: Option<u32>) {    let Some(pid) = pid else { return };
    // Negative pid targets the group, so descendants die with the parent.
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    // If the group signal found no group (the child already exited), fall back
    // to the child itself.
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(unix))]
async fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Authority;
    use crate::budget::{BudgetMeter, ExecutionBudget};
    use crate::capability::PathGuard;
    use crate::events::EventSink;
    use crate::memory::MemoryStore;
    use dex_protocol::{CallId, SessionId};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn ctx(root: &std::path::Path) -> CapabilityCtx {
        CapabilityCtx::new(
            PathGuard::new(root).expect("guard"),
            Arc::new(Authority::parse("process.start=*").expect("authority")),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(root.join("memory"))),
            CancellationToken::new(),
            CallId(1),
        )
    }

    #[tokio::test]
    async fn captures_stdout_stderr_and_the_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(dir.path());
        let outcome = run(
            &ctx,
            "sh",
            &["-c".into(), "echo out; echo err 1>&2; exit 3".into()],
            Duration::from_secs(30),
        )
        .await
        .expect("run");
        assert_eq!(outcome.exit_code, Some(3));
        assert!(!outcome.succeeded());
        assert!(outcome.stdout.contains("out"));
        assert!(outcome.stderr.contains("err"));
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn reports_a_timeout_rather_than_waiting_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(dir.path());
        let err = run(
            &ctx,
            "sh",
            &["-c".into(), "sleep 30".into()],
            Duration::from_millis(300),
        )
        .await
        .expect_err("must time out");
        assert_eq!(err.kind, CapabilityErrorKind::Timeout);
    }

    #[tokio::test]
    async fn cancellation_stops_a_running_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(dir.path());
        let token = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            token.cancel();
        });
        let err = run(&ctx, "sh", &["-c".into(), "sleep 30".into()], Duration::from_secs(60))
            .await
            .expect_err("must cancel");
        assert_eq!(err.kind, CapabilityErrorKind::Cancelled);
    }

    #[tokio::test]
    async fn killing_a_parent_kills_its_descendants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("child-ran");
        let ctx = ctx(dir.path());

        // The shell spawns a grandchild that would outlive it. If only the
        // direct child were signalled, the grandchild would still be running
        // after the timeout and would write the marker.
        let script = format!("( sleep 1; touch {} ) & sleep 30", marker.display());
        let err = run(
            &ctx,
            "sh",
            &["-c".into(), script],
            Duration::from_millis(300),
        )
        .await
        .expect_err("must time out");
        assert_eq!(err.kind, CapabilityErrorKind::Timeout);

        // Give a surviving grandchild ample time to prove it was not killed.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "a descendant survived the process-group kill and wrote {marker:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_binary_is_an_operation_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(dir.path());
        let err = run(
            &ctx,
            "definitely-not-a-real-binary-xyz",
            &[],
            Duration::from_secs(5),
        )
        .await
        .expect_err("must fail");
        assert_eq!(err.kind, CapabilityErrorKind::OperationFailed);
    }

    #[tokio::test]
    async fn large_output_is_capped_and_reported_as_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ctx = ctx(dir.path());
        ctx.budget = BudgetMeter::new(ExecutionBudget {
            output_bytes: 1024,
            ..ExecutionBudget::default()
        });
        let outcome = run(
            &ctx,
            "sh",
            &["-c".into(), "head -c 100000 /dev/zero | tr '\\0' 'x'".into()],
            Duration::from_secs(30),
        )
        .await
        .expect("run");
        assert!(outcome.truncated, "truncation must be reported");
        assert!(outcome.stdout.len() <= 1024, "got {} bytes", outcome.stdout.len());
    }
}
