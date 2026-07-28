#[cfg(any(target_os = "macos", target_os = "linux"))]
mod common;
#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
pub use darwin::HostFS;
#[cfg(target_os = "linux")]
pub use linux::HostFS;
