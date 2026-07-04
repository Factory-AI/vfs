use anyhow::Result;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// Maximum length for error messages sent through the daemon pipe.
const MAX_ERROR_MSG_LEN: usize = 4096;

/// Daemonize the current process and run a function in the daemon.
///
/// This function forks the process, detaches from the terminal, and runs the
/// provided `daemon_fn` in the child process. The parent process waits for
/// the daemon to signal readiness before returning.
///
/// # Arguments
/// * `daemon_fn` - The function to run in the daemon process (should block until done)
/// * `ready_check` - A function that polls for readiness (returns true when ready)
/// * `timeout` - How long to wait for the ready_check to succeed
///
/// # Returns
/// * `Ok(())` in the parent process if the daemon started successfully
/// * Never returns in the child process (exits with appropriate code)
pub fn daemonize<F, R>(daemon_fn: F, ready_check: R, timeout: Duration) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
    R: Fn() -> bool,
{
    // Create pipe for child->parent signaling
    let mut pipe_fds: [libc::c_int; 2] = [0; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("Failed to create pipe: {}", std::io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

    match unsafe { libc::fork() } {
        -1 => {
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            anyhow::bail!("Fork failed: {}", std::io::Error::last_os_error());
        }
        0 => {
            // Child process
            unsafe { libc::close(read_fd) };

            // Create new session (detach from terminal)
            if unsafe { libc::setsid() } == -1 {
                let _ = signal_parent(write_fd, Err("Failed to create new session".to_string()));
                std::process::exit(1);
            }

            let (daemon_thread, error_msg) = start_daemon(daemon_fn);

            // Wait for readiness, but fail early if daemon thread exits
            let start = std::time::Instant::now();
            let ready = loop {
                if ready_check() {
                    break true;
                }
                if daemon_thread.is_finished() {
                    break false;
                }
                if start.elapsed() >= timeout {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            };

            // Signal parent with result
            let signal_result = if ready {
                Ok(())
            } else {
                // Try to get the error message from the daemon thread
                let err_msg = error_msg
                    .lock()
                    .ok()
                    .and_then(|guard| guard.clone())
                    .unwrap_or_else(|| "Daemon failed to start".to_string());
                Err(err_msg)
            };
            let _ = signal_parent(write_fd, signal_result);
            unsafe { libc::close(write_fd) };

            if !ready {
                std::process::exit(1);
            }

            // Wait for daemon thread (blocks until done)
            match daemon_thread.join() {
                Ok(Ok(())) => std::process::exit(0),
                _ => std::process::exit(1),
            }
        }
        _child_pid => {
            // Parent process
            unsafe { libc::close(write_fd) };

            // Wait for child to signal readiness
            let result = wait_for_signal(read_fd);
            unsafe { libc::close(read_fd) };

            match result {
                Ok(()) => Ok(()),
                Err(msg) => anyhow::bail!("{}", msg),
            }
        }
    }
}

/// Signal parent process via pipe with optional error message.
///
/// Retries on EINTR to handle signal interruption during write.
fn signal_parent(fd: libc::c_int, result: Result<(), String>) -> Result<()> {
    // Protocol: first byte is success (0) or failure (1)
    // If failure, followed by 4-byte length (big-endian) and error message
    let buf = match &result {
        Ok(()) => vec![0u8],
        Err(msg) => {
            let msg_bytes = msg.as_bytes();
            let len = msg_bytes.len().min(MAX_ERROR_MSG_LEN);
            let mut buf = Vec::with_capacity(1 + 4 + len);
            buf.push(1u8);
            buf.extend_from_slice(&(len as u32).to_be_bytes());
            buf.extend_from_slice(&msg_bytes[..len]);
            buf
        }
    };

    let mut written_total = 0;
    while written_total < buf.len() {
        let written = unsafe {
            libc::write(
                fd,
                buf[written_total..].as_ptr() as *const libc::c_void,
                buf.len() - written_total,
            )
        };
        if written > 0 {
            written_total += written as usize;
        } else if written == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            anyhow::bail!("Failed to signal parent: {}", err);
        } else {
            anyhow::bail!("Unexpected write result: {}", written);
        }
    }
    Ok(())
}

/// Wait for signal from child process.
///
/// Returns Ok(()) on success, Err with error message on failure.
/// Retries on EINTR to handle signal interruption during read.
fn wait_for_signal(fd: libc::c_int) -> Result<(), String> {
    // Read first byte to determine success/failure
    let status = match read_exact(fd, 1) {
        Some(buf) => buf[0],
        None => return Err("Daemon process terminated unexpectedly".to_string()),
    };

    if status == 0 {
        return Ok(());
    }

    // Read 4-byte length
    let len_bytes = match read_exact(fd, 4) {
        Some(buf) => buf,
        None => return Err("Daemon failed to start".to_string()),
    };

    let len = u32::from_be_bytes(len_bytes.try_into().unwrap()) as usize;
    // Cap length to prevent allocation attacks from malformed messages
    let len = len.min(MAX_ERROR_MSG_LEN);
    if len == 0 {
        return Err("Daemon failed to start".to_string());
    }

    // Read error message
    match read_exact(fd, len) {
        Some(buf) => Err(String::from_utf8_lossy(&buf).into_owned()),
        None => Err("Daemon failed to start".to_string()),
    }
}

/// Read exactly `n` bytes from fd, retrying on EINTR.
fn read_exact(fd: libc::c_int, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut read_total = 0;

    while read_total < n {
        let result = unsafe {
            libc::read(
                fd,
                buf[read_total..].as_mut_ptr() as *mut libc::c_void,
                n - read_total,
            )
        };
        if result > 0 {
            read_total += result as usize;
        } else if result == 0 {
            // EOF
            return None;
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
    }
    Some(buf)
}

/// Start the daemon function in a separate thread, capturing any error message.
fn start_daemon<F>(
    daemon_fn: F,
) -> (
    std::thread::JoinHandle<Result<()>>,
    Arc<Mutex<Option<String>>>,
)
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    redirect_stdio();
    let error_msg: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let daemon_thread = {
        let error_msg = error_msg.clone();
        std::thread::spawn(move || {
            let result = daemon_fn();
            if let Err(ref e) = result {
                if let Ok(mut guard) = error_msg.lock() {
                    *guard = Some(format!("{:#}", e));
                }
            }
            result
        })
    };
    (daemon_thread, error_msg)
}

/// Redirect stdio to /dev/null for daemon
fn redirect_stdio() {
    unsafe {
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }
}
