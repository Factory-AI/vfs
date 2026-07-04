pub mod cmd;
pub mod config;
pub mod knobs;
pub mod opts;
pub mod profiling;
pub mod sandbox;

#[cfg(target_os = "linux")]
pub mod daemon;

#[cfg(unix)]
pub mod mount;

pub fn get_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("Internal error: failed to initialize runtime")
}
