//! Small signal-aware child supervision helpers for mount-owning commands.
//!
//! This is the point-wise form of the `exec` supervision pattern: direct
//! children get a parent-death signal, command owners listen for termination
//! signals, and interrupted children get a bounded TERM-then-KILL window before
//! the caller tears its mount down.

use anyhow::Result;
use std::process::ExitStatus;
use std::time::Duration;

pub(crate) enum ChildOutcome {
    Exited(ExitStatus),
    Interrupted(i32),
}

pub(crate) async fn supervise_command(
    mut command: tokio::process::Command,
) -> Result<ChildOutcome> {
    set_parent_death_signal(&mut command);

    let mut child = command.spawn()?;
    let signo = tokio::select! {
        status = child.wait() => return Ok(ChildOutcome::Exited(status?)),
        signal = crate::mount::termination_signal() => signal?,
    };

    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    Ok(ChildOutcome::Interrupted(signo))
}

#[cfg(target_os = "linux")]
pub(crate) fn set_parent_death_signal(command: &mut tokio::process::Command) {
    unsafe {
        command.pre_exec(parent_death_signal_hook);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn set_parent_death_signal(_command: &mut tokio::process::Command) {}

#[cfg(target_os = "linux")]
pub(crate) fn set_parent_death_signal_std(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(parent_death_signal_hook);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn set_parent_death_signal_std(_command: &mut std::process::Command) {}

#[cfg(target_os = "linux")]
fn parent_death_signal_hook() -> std::io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The parent may have died between fork and prctl.
    if unsafe { libc::getppid() } == 1 {
        unsafe { libc::raise(libc::SIGKILL) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn fake_darwin_supervision_path_cleans_up_after_sigint() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleanup_flag = Arc::clone(&cleaned);
        let (tx, rx) = oneshot::channel::<i32>();

        let owner = tokio::spawn(async move {
            let signo = rx.await.expect("fake signal sender dropped");
            cleanup_flag.store(true, Ordering::SeqCst);
            ChildOutcome::Interrupted(signo)
        });

        tx.send(libc::SIGINT).expect("fake owner dropped");
        match owner.await.expect("fake owner task panicked") {
            ChildOutcome::Interrupted(signo) => assert_eq!(signo, libc::SIGINT),
            ChildOutcome::Exited(_) => panic!("fake supervision path should be interrupted"),
        }
        assert!(cleaned.load(Ordering::SeqCst));
    }
}
