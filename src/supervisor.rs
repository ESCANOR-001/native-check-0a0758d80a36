//! Opt-in native process supervisor. Owns only its explicit child and pipe.
//! Exit75 is a successful, fully cleaned up child's explicit restart request.
use std::{
    ffi::OsString,
    io,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};
pub const RESTART_EXIT: i32 = 75;
const HELLO: &[u8] = b"native-connect-supervisor-v1\n";

/// The child must complete the inherited pipe handshake before opening state.
/// EOF/invalid input asks it to stop; an abandoned supervisor cannot orphan it.
pub async fn control() -> io::Result<tokio::sync::oneshot::Receiver<()>> {
    use std::io::Read;
    let (ready, wait) = tokio::sync::oneshot::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("connect-parent".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut hello = [0; HELLO.len()];
            if input.read_exact(&mut hello).is_ok() && hello == HELLO {
                let _ = ready.send(());
                // Only the owning parent has the pipe writer. Any later byte or EOF
                // requests shutdown; no arbitrary command or agent input is parsed.
                let mut byte = [0];
                let _ = input.read(&mut byte);
            }
            let _ = stop.send(());
        })?;
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Supervisor handshake timed out"))?
        .map_err(|_| io::Error::other("Supervisor pipe handshake refused"))?;
    Ok(stopped)
}

pub async fn run(
    args: Vec<OsString>,
    stop: impl std::future::Future<Output = ()>,
) -> io::Result<i32> {
    // Pin the startup path. Never locate or adopt any other bridge process.
    let executable = std::env::current_exe()?.canonicalize()?;
    tokio::pin!(stop);
    let mut restarts = std::collections::VecDeque::new();
    loop {
        let mut command = Command::new(&executable);
        command
            .arg("--supervised-child")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command.spawn()?;
        let mut tree = crate::process_tree::ProcessTree::attach(&child)?;
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("Supervisor pipe unavailable"))?;
        tokio::time::timeout(Duration::from_secs(2), pipe.write_all(HELLO))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "Supervisor pipe unavailable")
            })??;
        let (status, stopping) = tokio::select! {
            biased;
            _ = &mut stop => {
                // Closing the private pipe works on Windows without a console.
                drop(pipe);
                let status = tokio::time::timeout(Duration::from_secs(20),child.wait()).await;
                match status {
                    Ok(status) => (status?,true),
                    Err(_) => {drop(tree);let _=child.kill().await;let _=child.wait().await;return Err(io::Error::new(io::ErrorKind::TimedOut,"Supervised bridge did not stop within its cleanup deadline"));}
                }
            },
            status = child.wait() => (status?,false),
        };
        // Child::wait has reaped the Unix PID. Do not signal that numeric PID's
        // group afterwards; the OS can reuse it. Windows still closes its owned
        // Job handle, which cannot be confused with an unrelated process.
        tree.reaped();
        drop(tree);
        if stopping {
            return Ok(if status.success() || status.code() == Some(RESTART_EXIT) {
                0
            } else {
                1
            });
        }
        if status.code() != Some(RESTART_EXIT) {
            return Ok(status
                .code()
                .filter(|n| *n > 0)
                .unwrap_or(if status.success() { 0 } else { 1 }));
        }
        let now = Instant::now();
        while restarts
            .front()
            .is_some_and(|at| now.duration_since(*at) > Duration::from_secs(60))
        {
            restarts.pop_front();
        }
        if restarts.len() >= 5 {
            return Err(io::Error::other(
                "Native restart rate limit reached; automatic restart stopped",
            ));
        }
        restarts.push_back(now);
    }
}
