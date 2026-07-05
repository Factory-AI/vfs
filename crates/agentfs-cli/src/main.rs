use agentfs_cli::{
    cmd::{self, completions::handle_completions},
    get_runtime,
    logging::default_env_filter,
    opts::{Args, Command, FsCommand, PruneCommand, ServeCommand, SyncCommand},
};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use tracing_subscriber::prelude::*;

/// Parse and validate encryption key and cipher options.
/// Both must be provided together or neither.
fn parse_encryption(
    key: Option<String>,
    cipher: Option<String>,
) -> anyhow::Result<Option<(String, String)>> {
    match (key, cipher) {
        (Some(key), Some(cipher)) => Ok(Some((key, cipher))),
        (Some(_), None) => anyhow::bail!("--cipher is required when using --key"),
        (None, Some(_)) => anyhow::bail!("--key is required when using --cipher"),
        (None, None) => Ok(None),
    }
}

fn partial_origin_policy(
    mode: Option<agentfs_cli::opts::PartialOriginMode>,
    threshold_bytes: Option<u64>,
) -> Option<agentfs_core::PartialOriginPolicy> {
    match (mode, threshold_bytes) {
        (None, None) => None,
        (Some(mode), threshold_bytes) => {
            let mut policy = agentfs_core::PartialOriginPolicy::new(mode.into());
            if let Some(threshold_bytes) = threshold_bytes {
                policy = policy.with_threshold_bytes(threshold_bytes);
            }
            Some(policy)
        }
        (None, Some(threshold_bytes)) => Some(
            agentfs_core::PartialOriginPolicy::new(agentfs_core::PartialOriginMode::Auto)
                .with_threshold_bytes(threshold_bytes),
        ),
    }
}

fn exit_with_error(message: impl std::fmt::Display) -> ! {
    eprintln!("Error: {message}");
    exit_with_code(1);
}

fn exit_with_code(code: i32) -> ! {
    agentfs_cli::profiling::emit_cli_report();
    std::process::exit(code);
}

fn main() {
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(default_env_filter())
        .try_init();

    reset_sigpipe();

    CompleteEnv::with_factory(Args::command).complete();
    let _profile_report = agentfs_cli::profiling::install_cli_sink();
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            exit_with_code(code);
        }
    };

    // The one CLI error reporter (Display formatting, exit 1). Child-status
    // passthrough inside run/exec/init -c is the only other sanctioned exit.
    if let Err(e) = dispatch(args) {
        exit_with_error(format_args!("{e:#}"));
    }
}

fn dispatch(args: Args) -> anyhow::Result<()> {
    match args.command {
        Command::Init {
            id,
            force,
            base,
            key,
            cipher,
            command,
            backend,
            sync,
        } => {
            let rt = get_runtime();
            let encryption_opts = parse_encryption(key, cipher)?
                .map(|(key, cipher)| cmd::init::EncryptionOptions { key, cipher });
            rt.block_on(cmd::init::init_database(
                id,
                sync,
                force,
                base,
                encryption_opts,
                command,
                backend,
            ))
        }
        Command::Sync {
            id_or_path,
            command,
        } => {
            let rt = get_runtime();
            match command {
                SyncCommand::Pull => rt.block_on(cmd::sync::handle_pull_command(id_or_path)),
                SyncCommand::Push => rt.block_on(cmd::sync::handle_push_command(id_or_path)),
                SyncCommand::Checkpoint => {
                    rt.block_on(cmd::sync::handle_checkpoint_command(id_or_path))
                }
                SyncCommand::Stats => rt.block_on(cmd::sync::handle_stats_command(
                    &mut std::io::stdout(),
                    id_or_path,
                )),
            }
        }
        Command::Run {
            allow,
            no_default_allows,
            session,
            system,
            partial_origin,
            partial_origin_threshold_bytes,
            key,
            cipher,
            command,
            args,
        } => {
            let encryption = parse_encryption(key, cipher)?
                .map(|(hex_key, cipher)| agentfs_core::EncryptionConfig { hex_key, cipher });
            let options = agentfs_cli::opts::RunOptions {
                allow,
                no_default_allows,
                session,
                system,
                encryption,
                partial_origin_policy: partial_origin_policy(
                    partial_origin,
                    partial_origin_threshold_bytes,
                ),
                command: command.unwrap_or_else(default_shell),
                args,
            };
            // No runtime here: the Linux backend must fork before tokio starts.
            cmd::handle_run_command(options)
        }
        #[cfg(unix)]
        Command::Exec {
            id_or_path,
            command,
            args,
            backend,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher)?;
            let rt = get_runtime();
            rt.block_on(cmd::exec::handle_exec_command(
                id_or_path, command, args, backend, encryption,
            ))
        }
        #[cfg(unix)]
        Command::Clone {
            id_or_path,
            source,
            name,
            backend,
            verify,
        } => {
            let rt = get_runtime();
            rt.block_on(cmd::clone::handle_clone_command(
                id_or_path, source, name, backend, verify,
            ))
        }
        Command::Mount {
            id_or_path,
            mountpoint,
            auto_unmount,
            allow_root,
            system,
            foreground,
            uid,
            gid,
            backend,
            partial_origin,
            partial_origin_threshold_bytes,
            key,
            cipher,
        } => match (id_or_path, mountpoint) {
            (Some(id_or_path), Some(mountpoint)) => cmd::mount(cmd::MountArgs {
                id_or_path,
                mountpoint,
                auto_unmount,
                allow_root,
                allow_other: system,
                foreground,
                uid,
                gid,
                backend,
                partial_origin_policy: partial_origin_policy(
                    partial_origin,
                    partial_origin_threshold_bytes,
                ),
                encryption: parse_encryption(key, cipher)?,
            }),
            (None, None) => {
                cmd::mount::list_mounts(&mut std::io::stdout());
                Ok(())
            }
            _ => anyhow::bail!("both ID_OR_PATH and MOUNTPOINT are required to mount"),
        },
        Command::Diff { id_or_path } => {
            let rt = get_runtime();
            rt.block_on(cmd::fs::diff_filesystem(id_or_path))
        }
        Command::Timeline {
            id_or_path,
            limit,
            filter,
            status,
            format,
        } => {
            let rt = get_runtime();
            let options = cmd::timeline::TimelineOptions {
                limit,
                filter,
                status,
                format,
            };
            rt.block_on(cmd::timeline::show_timeline(
                &mut std::io::stdout(),
                &id_or_path,
                &options,
            ))
        }
        Command::Fs {
            command,
            id_or_path,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher)?;
            let rt = get_runtime();
            match command {
                FsCommand::Ls { fs_path } => rt.block_on(cmd::fs::ls_filesystem(
                    &mut std::io::stdout(),
                    id_or_path,
                    &fs_path,
                    encryption.as_ref(),
                )),
                FsCommand::Cat { file_path } => rt.block_on(cmd::fs::cat_filesystem(
                    &mut std::io::stdout(),
                    id_or_path,
                    &file_path,
                    encryption.as_ref(),
                )),
                FsCommand::Write { file_path, content } => rt.block_on(cmd::fs::write_filesystem(
                    id_or_path,
                    &file_path,
                    &content,
                    encryption.as_ref(),
                )),
            }
        }
        Command::Completions { command } => handle_completions(command),
        #[cfg(unix)]
        Command::Nfs {
            id_or_path,
            bind,
            port,
            key,
            cipher,
        } => {
            eprintln!("Warning: `agentfs nfs` is deprecated, use `agentfs serve nfs` instead");
            let encryption = parse_encryption(key, cipher)?;
            let rt = get_runtime();
            rt.block_on(cmd::nfs::handle_nfs_command(
                id_or_path, bind, port, encryption,
            ))
        }
        Command::McpServer { id_or_path, tools } => {
            eprintln!(
                "Warning: `agentfs mcp-server` is deprecated, use `agentfs serve mcp` instead"
            );
            let rt = get_runtime();
            rt.block_on(cmd::mcp_server::handle_mcp_server_command(
                id_or_path, tools,
            ))
        }
        Command::Serve { command } => match command {
            #[cfg(unix)]
            ServeCommand::Nfs {
                id_or_path,
                bind,
                port,
                key,
                cipher,
            } => {
                let encryption = parse_encryption(key, cipher)?;
                let rt = get_runtime();
                rt.block_on(cmd::nfs::handle_nfs_command(
                    id_or_path, bind, port, encryption,
                ))
            }
            ServeCommand::Mcp { id_or_path, tools } => {
                let rt = get_runtime();
                rt.block_on(cmd::mcp_server::handle_mcp_server_command(
                    id_or_path, tools,
                ))
            }
        },
        Command::Ps => cmd::ps::list_ps(&mut std::io::stdout()),
        Command::Prune { command } => match command {
            PruneCommand::Mounts { force } => cmd::mount::prune_mounts(force),
        },
        Command::Integrity {
            id_or_path,
            json,
            require_portable,
            check_base,
            checkpoint,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher)?;
            let rt = get_runtime();
            rt.block_on(cmd::safety::handle_integrity_command(
                &mut std::io::stdout(),
                id_or_path,
                json,
                require_portable,
                check_base,
                checkpoint,
                encryption.as_ref(),
            ))
        }
        Command::Backup {
            id_or_path,
            target,
            verify,
            materialize,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher)?;
            let rt = get_runtime();
            rt.block_on(cmd::safety::handle_backup_command(
                &mut std::io::stdout(),
                id_or_path,
                target,
                verify,
                materialize,
                encryption.as_ref(),
            ))
        }
        Command::Materialize {
            id_or_path,
            output,
            verify,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher)?;
            let rt = get_runtime();
            rt.block_on(cmd::safety::handle_materialize_command(
                &mut std::io::stdout(),
                id_or_path,
                output,
                verify,
                encryption.as_ref(),
            ))
        }
        Command::Migrate {
            id_or_path,
            dry_run,
            copy,
            verify,
            overwrite_target,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher)?;
            let rt = get_runtime();
            if let Some(target) = copy {
                rt.block_on(cmd::migrate::handle_migrate_copy_command(
                    &mut std::io::stdout(),
                    id_or_path,
                    target,
                    verify,
                    overwrite_target,
                    encryption.as_ref(),
                ))
            } else {
                rt.block_on(cmd::migrate::handle_migrate_command(
                    &mut std::io::stdout(),
                    id_or_path,
                    dry_run,
                    encryption.as_ref(),
                ))
            }
        }
    }
}

/// Reset SIGPIPE to the default behavior (terminate the process) so that
/// piping output to tools like `head` doesn't cause a panic.
#[cfg(unix)]
fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

/// Returns the default shell for the current platform.
/// Linux uses bash, macOS uses zsh.
fn default_shell() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        std::path::PathBuf::from("zsh")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::path::PathBuf::from("bash")
    }
}

#[cfg(test)]
mod partial_origin {
    use super::partial_origin_policy;
    use agentfs_cli::opts::{Args, Command, PartialOriginMode};
    use clap::Parser;

    #[test]
    fn legacy_env_does_not_override_cli_off() {
        let key = concat!("AGENTFS_OVERLAY_", "PARTIAL_ORIGIN");
        let previous = std::env::var(key).ok();
        std::env::set_var(key, "1");

        let args = Args::try_parse_from([
            "agentfs",
            "run",
            "--partial-origin",
            "off",
            "--",
            "sh",
            "-c",
            "true",
        ])
        .expect("run args with --partial-origin off should parse");

        let (mode, threshold_bytes) = match args.command {
            Command::Run {
                partial_origin,
                partial_origin_threshold_bytes,
                ..
            } => (partial_origin, partial_origin_threshold_bytes),
            other => panic!("expected run command, got {other:?}"),
        };
        let policy = partial_origin_policy(mode, threshold_bytes)
            .expect("--partial-origin off should resolve an explicit policy");

        match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }

        eprintln!(
            "legacy partial-origin env set; CLI policy resolved to {:?}",
            policy.mode
        );
        assert_eq!(mode, Some(PartialOriginMode::Off));
        assert_eq!(policy.mode, agentfs_core::PartialOriginMode::Off);
    }
}
