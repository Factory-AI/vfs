//! Signal-aware child supervision helpers for mount-owning commands.
//!
//! Children get a parent-death signal on Linux, are placed in their own
//! process group when requested, and interrupted workloads get a bounded
//! TERM-then-KILL teardown before the mount is unmounted.

#[cfg(test)]
use anyhow::Context;
use anyhow::Result;
use std::future::Future;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::pin::Pin;
use std::process::ExitStatus;
use std::time::Duration;

use crate::MountHandle;

/// Outcome of a supervised command.
pub enum ChildOutcome {
    Exited(ExitStatus),
    Interrupted(i32),
}

/// Outcome of supervised async work that is not itself a child process.
pub enum SupervisedTaskOutcome<T> {
    Completed(T),
    Interrupted(i32),
}

/// Options for mount-owning child supervision.
#[derive(Debug, Clone, Copy)]
pub struct SuperviseOpts {
    /// Time to wait after TERM before escalating to KILL.
    pub term_grace: Duration,
    /// Put the child in a new process group and signal the whole group.
    pub kill_process_group: bool,
}

impl Default for SuperviseOpts {
    fn default() -> Self {
        Self {
            term_grace: Duration::from_secs(5),
            kill_process_group: true,
        }
    }
}

/// Run a command with a mounted filesystem and always unmount afterward.
pub async fn run_supervised(
    handle: MountHandle,
    command: tokio::process::Command,
) -> Result<ExitStatus> {
    run_supervised_with_opts(handle, command, SuperviseOpts::default()).await
}

/// Run a command with custom supervision options and always unmount afterward.
pub async fn run_supervised_with_opts(
    handle: MountHandle,
    mut command: tokio::process::Command,
    opts: SuperviseOpts,
) -> Result<ExitStatus> {
    configure_supervised_command(&mut command, opts);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            handle.unmount().await?;
            return Err(error.into());
        }
    };

    let status = wait_for_supervised_child(&mut child, opts).await;
    let unmount = handle.unmount().await;
    unmount?;
    status
}

/// Run arbitrary async work with a mounted filesystem and always unmount afterward.
///
/// This covers mount-owning flows whose lifetime is not represented by one
/// child process, such as `agentfs clone`'s mount plus bulk-import pipeline.
pub async fn run_supervised_task<T, F>(
    handle: MountHandle,
    task: F,
) -> Result<SupervisedTaskOutcome<T>>
where
    F: Future<Output = Result<T>>,
{
    let outcome = tokio::select! {
        result = task => result.map(SupervisedTaskOutcome::Completed),
        signal = crate::termination_signal() => {
            signal
                .map(SupervisedTaskOutcome::Interrupted)
                .map_err(anyhow::Error::from)
        },
    };

    let unmount = handle.unmount().await;
    unmount?;
    outcome
}

/// Wait for a foreground mount to end, then unmount through the shared path.
pub async fn supervise_mount(handle: MountHandle) -> Result<()> {
    tokio::select! {
        result = crate::shutdown_signal() => result.map_err(anyhow::Error::from),
        _ = async {
            loop {
                if handle.is_finished() || !crate::is_mountpoint(handle.mountpoint()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        } => Ok(()),
    }?;
    handle.unmount().await
}

/// Supervise a command without owning a mount.
pub async fn supervise_command(mut command: tokio::process::Command) -> Result<ChildOutcome> {
    let opts = SuperviseOpts::default();
    configure_supervised_command(&mut command, opts);

    let mut child = command.spawn()?;
    let signo = tokio::select! {
        status = child.wait() => {
            let status = status?;
            if let Some(signo) = signal_from_status(&status) {
                return Ok(ChildOutcome::Interrupted(signo));
            }
            return Ok(ChildOutcome::Exited(status));
        },
        signal = crate::termination_signal() => signal?,
    };

    terminate_child(&mut child, opts.kill_process_group, signo);
    wait_or_kill(&mut child, opts).await?;
    Ok(ChildOutcome::Interrupted(signo))
}

/// Hooks for supervising a forked child.
#[cfg(unix)]
#[derive(Default)]
pub struct SuperviseHooks {
    profile_checkpoint: Option<Box<dyn FnMut() + Send + 'static>>,
}

#[cfg(unix)]
impl SuperviseHooks {
    pub fn with_profile_checkpoint(hook: impl FnMut() + Send + 'static) -> Self {
        Self {
            profile_checkpoint: Some(Box::new(hook)),
        }
    }
}

/// Run an already-forked child with a mounted filesystem and always unmount afterward.
#[cfg(unix)]
pub async fn run_supervised_pid_with_hooks(
    handle: MountHandle,
    pid: libc::pid_t,
    opts: SuperviseOpts,
    hooks: SuperviseHooks,
) -> Result<ExitStatus> {
    let status = supervise_pid_with_hooks(pid, opts, hooks).await;
    let unmount = handle.unmount().await;
    unmount?;
    status
}

/// Run an already-forked child with default supervision and a mounted filesystem.
#[cfg(unix)]
pub async fn run_supervised_pid(handle: MountHandle, pid: libc::pid_t) -> Result<ExitStatus> {
    run_supervised_pid_with_hooks(
        handle,
        pid,
        SuperviseOpts::default(),
        SuperviseHooks::default(),
    )
    .await
}

/// Supervise an already-forked child without owning a mount.
#[cfg(unix)]
pub async fn supervise_pid_with_hooks(
    pid: libc::pid_t,
    opts: SuperviseOpts,
    mut hooks: SuperviseHooks,
) -> Result<ExitStatus> {
    use tokio::signal::unix::{signal, Signal, SignalKind};

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut child = signal(SignalKind::child())?;
    let mut usr1 = if hooks.profile_checkpoint.is_some() {
        Some(signal(SignalKind::user_defined1())?)
    } else {
        None
    };

    loop {
        if let Some(status) = try_wait_pid(pid)? {
            return Ok(status);
        }

        tokio::select! {
            _ = term.recv() => return terminate_pid_and_wait(pid, opts, libc::SIGTERM).await,
            _ = int.recv() => return terminate_pid_and_wait(pid, opts, libc::SIGINT).await,
            _ = hup.recv() => return terminate_pid_and_wait(pid, opts, libc::SIGHUP).await,
            _ = child.recv() => {},
            _ = recv_optional_signal(&mut usr1) => {
                if let Some(checkpoint) = hooks.profile_checkpoint.as_mut() {
                    checkpoint();
                }
            },
        }
    }

    async fn recv_optional_signal(signal: &mut Option<Signal>) {
        match signal {
            Some(signal) => {
                let _ = signal.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    }
}

/// Prepare a child created by a command-specific `fork()` for shared supervision.
#[cfg(unix)]
pub fn prepare_forked_child(kill_process_group: bool) -> std::io::Result<()> {
    supervised_child_pre_exec(kill_process_group)
}

/// Convert a child status to the process exit code used by shell-like commands.
#[cfg(unix)]
pub fn exit_code_for_status(status: ExitStatus) -> i32 {
    signal_from_status(&status)
        .map(|signal| 128 + signal)
        .or_else(|| status.code())
        .unwrap_or(1)
}

#[cfg(not(unix))]
pub fn exit_code_for_status(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(test)]
pub type ShutdownFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

#[cfg(test)]
pub trait MountedCommandBackend {
    fn mountpoint(&self) -> &Path;
    fn unmount(&mut self) -> Result<()>;
    fn shutdown_server(&mut self) -> ShutdownFuture<'_>;
    fn remove_mountpoint(&mut self) -> Result<()>;
}

#[cfg(test)]
pub async fn supervise_mounted_command<B>(
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

#[cfg(test)]
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

async fn wait_for_supervised_child(
    child: &mut tokio::process::Child,
    opts: SuperviseOpts,
) -> Result<ExitStatus> {
    let signo = tokio::select! {
        status = child.wait() => return Ok(status?),
        signal = crate::termination_signal() => signal?,
    };

    terminate_child(child, opts.kill_process_group, signo);
    let status = wait_or_kill(child, opts).await?;
    if let Some(signal) = signal_from_status(&status) {
        tracing_signal_teardown(signo, signal);
    }
    Ok(interrupted_status(signo))
}

async fn wait_or_kill(
    child: &mut tokio::process::Child,
    opts: SuperviseOpts,
) -> Result<ExitStatus> {
    match tokio::time::timeout(opts.term_grace, child.wait()).await {
        Ok(status) => Ok(status?),
        Err(_) => {
            terminate_child(child, opts.kill_process_group, libc::SIGKILL);
            Ok(child.wait().await?)
        }
    }
}

#[cfg(unix)]
async fn terminate_pid_and_wait(
    pid: libc::pid_t,
    opts: SuperviseOpts,
    signal: i32,
) -> Result<ExitStatus> {
    terminate_pid(pid, opts.kill_process_group, signal);
    let status = match tokio::time::timeout(opts.term_grace, wait_for_pid_exit(pid)).await {
        Ok(status) => status,
        Err(_) => {
            terminate_pid(pid, opts.kill_process_group, libc::SIGKILL);
            wait_for_pid_exit(pid).await
        }
    }?;
    if let Some(child_signal) = signal_from_status(&status) {
        tracing_signal_teardown(signal, child_signal);
    }
    Ok(interrupted_status(signal))
}

#[cfg(unix)]
async fn wait_for_pid_exit(pid: libc::pid_t) -> Result<ExitStatus> {
    let mut child = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    loop {
        if let Some(status) = try_wait_pid(pid)? {
            return Ok(status);
        }
        let _ = child.recv().await;
    }
}

#[cfg(unix)]
fn interrupted_status(signal: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatus::from_raw(signal)
}

#[cfg(unix)]
fn try_wait_pid(pid: libc::pid_t) -> Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;

    let mut status: libc::c_int = 0;
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if rc == pid {
        return Ok(Some(ExitStatus::from_raw(status)));
    }
    if rc == 0 {
        return Ok(None);
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EINTR) {
        return Ok(None);
    }
    Err(error.into())
}

fn terminate_child(child: &mut tokio::process::Child, kill_process_group: bool, signal: i32) {
    let Some(pid) = child.id() else {
        return;
    };
    terminate_pid(pid as i32, kill_process_group, signal);
}

#[cfg(unix)]
fn terminate_pid(pid: libc::pid_t, kill_process_group: bool, signal: i32) {
    let rc = if kill_process_group {
        unsafe { libc::killpg(pid, signal) }
    } else {
        unsafe { libc::kill(pid, signal) }
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(pid, %error, "failed to signal supervised child");
        }
    }
}

fn configure_supervised_command(command: &mut tokio::process::Command, opts: SuperviseOpts) {
    unsafe {
        command.pre_exec(move || supervised_child_pre_exec(opts.kill_process_group));
    }
}

fn tracing_signal_teardown(parent_signal: i32, child_signal: i32) {
    tracing::debug!(
        parent_signal,
        child_signal,
        "supervised child exited after process-group teardown"
    );
}

#[cfg(target_os = "linux")]
pub fn set_parent_death_signal(command: &mut tokio::process::Command) {
    unsafe {
        command.pre_exec(move || supervised_child_pre_exec(false));
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_parent_death_signal(_command: &mut tokio::process::Command) {}

#[cfg(target_os = "linux")]
pub fn set_parent_death_signal_std(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(move || supervised_child_pre_exec(false));
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_parent_death_signal_std(_command: &mut std::process::Command) {}

#[cfg(unix)]
fn supervised_child_pre_exec(kill_process_group: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    parent_death_signal_hook()?;

    if kill_process_group && unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

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

    #[tokio::test]
    async fn process_group_teardown_kills_grandchild() {
        let temp = tempfile::tempdir().expect("tempdir");
        let child_pid_path = temp.path().join("child.pid");
        let grandchild_pid_path = temp.path().join("grandchild.pid");
        let ready_path = temp.path().join("ready");

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(
                "printf '%s\\n' \"$$\" > \"$1\"; \
                 (while :; do sleep 1; done) & \
                 printf '%s\\n' \"$!\" > \"$2\"; \
                 : > \"$3\"; \
                 wait",
            )
            .arg("agentfs-supervise-group")
            .arg(&child_pid_path)
            .arg(&grandchild_pid_path)
            .arg(&ready_path);

        let opts = SuperviseOpts {
            term_grace: Duration::from_secs(2),
            kill_process_group: true,
        };
        configure_supervised_command(&mut command, opts);
        let mut child = command.spawn().expect("spawn supervised shell");
        wait_for_path(&ready_path).await;

        let child_pid = read_pid(&child_pid_path);
        let grandchild_pid = read_pid(&grandchild_pid_path);
        assert!(process_exists(child_pid), "child should be running");
        assert!(
            process_exists(grandchild_pid),
            "grandchild should be running before process-group teardown"
        );

        terminate_child(&mut child, opts.kill_process_group, libc::SIGTERM);
        wait_or_kill(&mut child, opts)
            .await
            .expect("wait for supervised process-group teardown");
        wait_for_process_exit(grandchild_pid).await;

        assert!(
            !process_exists(child_pid),
            "child process {child_pid} survived process-group teardown"
        );
        assert!(
            !process_exists(grandchild_pid),
            "grandchild process {grandchild_pid} survived process-group teardown"
        );
    }

    #[test]
    fn parent_signal_status_preserves_interrupted_exit_code() {
        let status = interrupted_status(libc::SIGTERM);
        assert_eq!(
            exit_code_for_status(status),
            128 + libc::SIGTERM,
            "parent-directed teardown reports the parent signal even if the child handles TERM"
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

    async fn wait_for_process_exit(pid: i32) {
        tokio::time::timeout(Duration::from_secs(5), async move {
            while process_exists(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("process {pid} did not exit within timeout"));
    }

    fn read_pid(path: &Path) -> i32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read pid file {}: {error}", path.display()))
            .trim()
            .parse::<i32>()
            .unwrap_or_else(|error| panic!("parse pid file {}: {error}", path.display()))
    }

    fn process_exists(pid: i32) -> bool {
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
