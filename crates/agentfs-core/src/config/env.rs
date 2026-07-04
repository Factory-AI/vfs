use std::env::VarError;
use std::time::Duration;

/// Shared environment reader for runtime configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvReader;

impl EnvReader {
    pub const fn new() -> Self {
        Self
    }

    /// Read an optional string environment value.
    ///
    /// Non-Unicode values behave like an unset variable, matching the legacy
    /// `std::env::var(...).ok()` behavior at call sites that use ambient
    /// credentials or OS context rather than runtime boolean knobs.
    pub fn string(self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    /// Parse the shared boolean grammar:
    /// true = {1,true,yes,on}, false = {0,false,no,off}.
    pub fn parse_bool(value: &str) -> Option<bool> {
        match value.trim() {
            "1" => Some(true),
            "0" => Some(false),
            value
                if value.eq_ignore_ascii_case("true")
                    || value.eq_ignore_ascii_case("yes")
                    || value.eq_ignore_ascii_case("on") =>
            {
                Some(true)
            }
            value
                if value.eq_ignore_ascii_case("false")
                    || value.eq_ignore_ascii_case("no")
                    || value.eq_ignore_ascii_case("off") =>
            {
                Some(false)
            }
            _ => None,
        }
    }

    pub fn bool(self, name: &str, default: bool) -> bool {
        match std::env::var(name) {
            Ok(value) => Self::parse_bool(&value).unwrap_or_else(|| {
                tracing::warn!(
                    "Ignoring invalid {}={:?}; using default {}",
                    name,
                    value,
                    default
                );
                default
            }),
            Err(VarError::NotPresent) => default,
            Err(VarError::NotUnicode(value)) => {
                tracing::warn!(
                    "Ignoring non-Unicode {}={:?}; using default {}",
                    name,
                    value,
                    default
                );
                default
            }
        }
    }

    pub fn duration_millis(self, name: &str, default_ms: u64) -> Duration {
        match std::env::var(name) {
            Ok(value) => match value.parse::<u64>() {
                Ok(ms) => Duration::from_millis(ms),
                Err(_) => {
                    tracing::warn!(
                        "Ignoring invalid {}={:?}; using default {}ms",
                        name,
                        value,
                        default_ms
                    );
                    Duration::from_millis(default_ms)
                }
            },
            Err(VarError::NotPresent) => Duration::from_millis(default_ms),
            Err(VarError::NotUnicode(value)) => {
                tracing::warn!(
                    "Ignoring non-Unicode {}={:?}; using default {}ms",
                    name,
                    value,
                    default_ms
                );
                Duration::from_millis(default_ms)
            }
        }
    }

    pub fn positive_usize(self, name: &str, default_value: usize) -> usize {
        match std::env::var(name) {
            Ok(value) => match value.parse::<usize>().ok().filter(|value| *value > 0) {
                Some(parsed) => parsed,
                None => {
                    tracing::warn!(
                        "Ignoring invalid {}={:?}; using default {}",
                        name,
                        value,
                        default_value
                    );
                    default_value
                }
            },
            Err(VarError::NotPresent) => default_value,
            Err(VarError::NotUnicode(value)) => {
                tracing::warn!(
                    "Ignoring non-Unicode {}={:?}; using default {}",
                    name,
                    value,
                    default_value
                );
                default_value
            }
        }
    }
}
