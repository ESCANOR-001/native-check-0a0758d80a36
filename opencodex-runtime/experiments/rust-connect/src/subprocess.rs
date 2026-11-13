//! Bounded helper commands. Cancellation owns only the process tree we started.
use std::{io, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Debug)]
pub(crate) struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
}

pub(crate) async fn run(
    mut command: Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|_| "Helper command could not start")?;
    let tree = crate::process_tree::ProcessTree::attach(&child)
        .map_err(|_| "Cannot own the helper process tree")?;
    let stdout = child.stdout.take().ok_or("Helper output unavailable")?;
    let stderr = child.stderr.take().ok_or("Helper output unavailable")?;
    let result = tokio::time::timeout(timeout, async {
        async fn read(
            stream: impl tokio::io::AsyncRead + Unpin,
            limit: usize,
        ) -> io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            stream
                .take(limit.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > limit {
                return Err(io::Error::other("Helper output limit"));
            }
            Ok(bytes)
        }
        let (stdout, _) = tokio::try_join!(read(stdout, stdout_limit), read(stderr, stderr_limit))?;
        let code = child.wait().await?.code().unwrap_or(-1);
        Ok::<_, io::Error>(Output { code, stdout })
    })
    .await;
    // A successful parent can leave a descendant alive after closing its pipes.
    // Drop the owned group/job on success too, before releasing this helper slot.
    drop(tree);
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
        .map_err(|_| "Helper timed out")?
        .map_err(|_| "Helper failed or exceeded its output limit".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn shell(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }
    async fn gone(pid: &str) {
        for _ in 0..100 {
            let id: i32 = pid.parse().unwrap();
            if unsafe { libc::kill(id, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            #[cfg(target_os = "linux")]
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Owned helper descendant survived");
    }
    #[tokio::test]
    async fn bounded_output_and_exit_status_are_reported() {
        let o = run(shell("printf kept; exit 7"), Duration::from_secs(2), 32, 32)
            .await
            .unwrap();
        assert_eq!(o.code, 7);
        assert_eq!(o.stdout, b"kept");
        assert!(
            run(shell("printf 0123456789"), Duration::from_secs(2), 4, 4)
                .await
                .is_err()
        );
        assert!(
            run(shell("printf 0123456789 >&2"), Duration::from_secs(2), 4, 4)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn successful_parent_does_not_leave_background_helpers() {
        let output = run(
            shell("sleep 30 >/dev/null 2>&1 & echo $!; exit 0"),
            Duration::from_secs(2),
            32,
            32,
        )
        .await
        .unwrap();
        assert_eq!(output.code, 0);
        gone(std::str::from_utf8(&output.stdout).unwrap().trim()).await;
    }
    #[tokio::test]
    async fn timeout_and_cancellation_stop_owned_descendants() {
        let dir = tempfile::tempdir().unwrap();
        for cancel in [false, true] {
            let path = dir.path().join(if cancel { "cancel" } else { "timeout" });
            // Publish readiness only after the PID is fully written. Checking
            // existence of the redirected file raced its write on macOS CI,
            // so cancellation could read an empty PID instead of testing cleanup.
            let mut c = shell("sleep 30 & echo $! > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"; wait");
            c.arg("fixture").arg(&path);
            let task = tokio::spawn(run(
                c,
                Duration::from_millis(if cancel { 30000 } else { 400 }),
                32,
                32,
            ));
            for _ in 0..100 {
                if path.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let pid = std::fs::read_to_string(path).unwrap().trim().to_owned();
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert!(task.await.unwrap().is_err());
            }
            gone(&pid).await;
        }
    }
}
