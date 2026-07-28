//! CLI logging defaults.
//!
//! The binary installs the tracing subscriber, but the default filter must
//! cover every first-party Vfs crate so warnings are visible even when
//! `RUST_LOG` is unset.

const DEFAULT_ENV_FILTER: &str = concat!(
    "vfs=info,",
    "vfs_cli=info,",
    "vfs_core=info,",
    "vfs_fuse=info,",
    "vfs_nfs=info,",
    "vfs_mount=info"
);

pub fn default_env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| DEFAULT_ENV_FILTER.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use tracing::{event, Level};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::{Layer, Registry};

    #[derive(Clone, Default)]
    struct SeenTargets {
        targets: Arc<Mutex<BTreeSet<String>>>,
    }

    impl<S> Layer<S> for SeenTargets
    where
        S: tracing::Subscriber,
        S: for<'lookup> LookupSpan<'lookup>,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            self.targets
                .lock()
                .unwrap()
                .insert(event.metadata().target().to_string());
        }
    }

    #[test]
    fn default_env_filter_covers_all_vfs_crates() {
        let previous = std::env::var_os("RUST_LOG");
        std::env::remove_var("RUST_LOG");

        let seen = SeenTargets::default();
        let targets = ["vfs_core", "vfs_fuse", "vfs_nfs", "vfs_mount", "vfs_cli"];
        let subscriber = Registry::default()
            .with(default_env_filter())
            .with(seen.clone());

        tracing::dispatcher::with_default(&subscriber.into(), || {
            event!(target: "vfs_core", Level::WARN, "default filter coverage probe");
            event!(target: "vfs_fuse", Level::WARN, "default filter coverage probe");
            event!(target: "vfs_nfs", Level::WARN, "default filter coverage probe");
            event!(target: "vfs_mount", Level::WARN, "default filter coverage probe");
            event!(target: "vfs_cli", Level::WARN, "default filter coverage probe");
        });

        match previous {
            Some(value) => std::env::set_var("RUST_LOG", value),
            None => std::env::remove_var("RUST_LOG"),
        }

        let seen = seen.targets.lock().unwrap();
        for target in targets {
            assert!(
                seen.contains(target),
                "default EnvFilter did not enable warning target {target}"
            );
        }
    }
}
