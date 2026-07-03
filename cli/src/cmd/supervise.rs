//! Small signal-aware child supervision helpers for mount-owning commands.
//!
//! This is the point-wise form of the `exec` supervision pattern: direct
//! children get a parent-death signal, command owners listen for termination
//! signals, and interrupted children get a bounded TERM-then-KILL window before
//! the caller tears its mount down.

#[cfg(any(target_os = "macos", test))]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(target_os = "macos", test))]
use std::future::Future;
#[cfg(any(target_os = "macos", test))]
use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::pin::Pin;
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
        status = child.wait() => {
            let status = status?;
            if let Some(signo) = signal_from_status(&status) {
                return Ok(ChildOutcome::Interrupted(signo));
            }
            return Ok(ChildOutcome::Exited(status));
        },
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

#[cfg(any(target_os = "macos", test))]
pub(crate) type ShutdownFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

#[cfg(any(target_os = "macos", test))]
pub(crate) trait MountedCommandBackend {
    fn mountpoint(&self) -> &Path;
    fn unmount(&mut self) -> Result<()>;
    fn shutdown_server(&mut self) -> ShutdownFuture<'_>;
    fn remove_mountpoint(&mut self) -> Result<()>;
}

#[cfg(any(target_os = "macos", test))]
pub(crate) async fn supervise_mounted_command<B>(
    command: tokio::process::Command,
    mut backend: B,
) -> Result<ChildOutcome>
where
    B: MountedCommandBackend,
{
    let outcome = supervise_command(command).await;
    let cleanup = cleanup_mounted_backend(&mut backend).await;
    cleanup?;
    outcome
}

#[cfg(any(target_os = "macos", test))]
async fn cleanup_mounted_backend<B>(backend: &mut B) -> Result<()>
where
    B: MountedCommandBackend,
{
    let mountpoint = backend.mountpoint().display().to_string();
    let unmount = backend
        .unmount()
        .with_context(|| format!("failed to unmount Darwin/NFS mount {mountpoint}"));
    let shutdown = backend
        .shutdown_server()
        .await
        .with_context(|| format!("failed to shut down Darwin/NFS server for {mountpoint}"));
    let remove = backend
        .remove_mountpoint()
        .with_context(|| format!("failed to remove Darwin/NFS mountpoint {mountpoint}"));

    unmount?;
    shutdown?;
    remove?;
    Ok(())
}

#[cfg(unix)]
fn signal_from_status(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;

    status.signal()
}

#[cfg(not(unix))]
fn signal_from_status(_status: &ExitStatus) -> Option<i32> {
    None
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
    use anyhow::Context;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn fake_darwin_supervision_path_cleans_up_after_sigint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mountpoint = temp.path().join("darwin-nfs-mount");
        std::fs::create_dir(&mountpoint).expect("create fake mountpoint");
        let child_pid_path = temp.path().join("child.pid");
        let child_ready_path = temp.path().join("child.ready");

        let (backend, registry) = FakeDarwinMountBackend::mounted(mountpoint.clone());

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(
                "printf '%s\\n' \"$$\" > \"$1\"; : > \"$2\"; \
                 while :; do sleep 1; done",
            )
            .arg("agentfs-darwin-supervision-child")
            .arg(&child_pid_path)
            .arg(&child_ready_path)
            .current_dir(&mountpoint);

        let signal_ready_path = child_ready_path.clone();
        let signaler = tokio::spawn(async move {
            wait_for_path(&signal_ready_path).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let rc = unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
            assert_eq!(rc, 0, "failed to deliver real SIGINT to test process");
        });

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            supervise_mounted_command(command, backend),
        )
        .await
        .expect("Darwin/NFS supervision cleanup exceeded 10s")
        .expect("Darwin/NFS supervision failed");

        signaler.await.expect("signal task panicked");

        match outcome {
            ChildOutcome::Interrupted(signo) => assert_eq!(signo, libc::SIGINT),
            ChildOutcome::Exited(status) => {
                panic!("child exited without SIGINT supervision: {status}")
            }
        }

        let child_pid = std::fs::read_to_string(&child_pid_path)
            .expect("child pid file")
            .trim()
            .parse::<i32>()
            .expect("child pid");
        assert!(
            !process_exists(child_pid),
            "supervised child process {child_pid} survived cleanup"
        );

        let state = registry.snapshot();
        assert_eq!(state.unmount_calls, 1, "unmount must be called once");
        assert_eq!(
            state.shutdown_calls, 1,
            "NFS server shutdown must be called once"
        );
        assert_eq!(
            state.remove_mountpoint_calls, 1,
            "mountpoint removal must be called once"
        );
        assert!(
            state.unmount_elapsed.expect("unmount duration recorded") < Duration::from_secs(10),
            "fake unmount exceeded the 10s contract bound"
        );
        assert!(!state.mounted, "fake mount registry still marks mount live");
        assert!(
            !state.nfs_server_running,
            "fake NFS server registry still marks server live"
        );
        assert!(
            !mountpoint.exists(),
            "fake mountpoint should be removed after cleanup"
        );

        println!(
            "Darwin/NFS supervision path cleaned after real SIGINT: \
             child_pid={child_pid}, unmount_elapsed={:?}, registry={state:?}",
            state.unmount_elapsed
        );
    }

    #[derive(Debug, Clone, Default)]
    struct FakeMountState {
        mounted: bool,
        nfs_server_running: bool,
        unmount_calls: usize,
        shutdown_calls: usize,
        remove_mountpoint_calls: usize,
        unmount_elapsed: Option<Duration>,
    }

    #[derive(Clone)]
    struct FakeMountRegistry {
        state: Arc<Mutex<FakeMountState>>,
    }

    impl FakeMountRegistry {
        fn snapshot(&self) -> FakeMountState {
            self.state.lock().expect("fake registry poisoned").clone()
        }
    }

    struct FakeDarwinMountBackend {
        mountpoint: std::path::PathBuf,
        registry: FakeMountRegistry,
    }

    impl FakeDarwinMountBackend {
        fn mounted(mountpoint: std::path::PathBuf) -> (Self, FakeMountRegistry) {
            let registry = FakeMountRegistry {
                state: Arc::new(Mutex::new(FakeMountState {
                    mounted: true,
                    nfs_server_running: true,
                    ..FakeMountState::default()
                })),
            };
            (
                Self {
                    mountpoint,
                    registry: registry.clone(),
                },
                registry,
            )
        }
    }

    impl MountedCommandBackend for FakeDarwinMountBackend {
        fn mountpoint(&self) -> &Path {
            &self.mountpoint
        }

        fn unmount(&mut self) -> Result<()> {
            let started = Instant::now();
            let mut state = self.registry.state.lock().expect("fake registry poisoned");
            state.unmount_calls += 1;
            state.mounted = false;
            state.unmount_elapsed = Some(started.elapsed());
            Ok(())
        }

        fn shutdown_server(&mut self) -> ShutdownFuture<'_> {
            let registry = self.registry.clone();
            Box::pin(async move {
                let mut state = registry.state.lock().expect("fake registry poisoned");
                state.shutdown_calls += 1;
                state.nfs_server_running = false;
                Ok(())
            })
        }

        fn remove_mountpoint(&mut self) -> Result<()> {
            std::fs::remove_dir(&self.mountpoint)
                .with_context(|| format!("remove fake mountpoint {}", self.mountpoint.display()))?;
            let mut state = self.registry.state.lock().expect("fake registry poisoned");
            state.remove_mountpoint_calls += 1;
            Ok(())
        }
    }

    async fn wait_for_path(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if path.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child workload did not become ready");
    }

    fn process_exists(pid: i32) -> bool {
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
