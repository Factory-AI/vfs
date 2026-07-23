//! Generated clap <-> MANUAL command-reference parity.
//!
//! The command/option sections of docs/MANUAL.md are rendered from the clap
//! command tree in `opts`, so the manual cannot drift from `agentfs --help`.
//! The checked-in manual is generated from the Linux CLI surface (the
//! first-tier platform); `docs::tests::manual_help_parity` fails when the
//! generated region is stale and rewrites it under `AGENTFS_UPDATE_MANUAL=1`.

use clap::{ArgAction, CommandFactory};

use crate::opts::Args;

#[cfg(all(test, target_os = "linux"))]
const MANUAL_REGEN_ENV: &str = "AGENTFS_UPDATE_MANUAL";
const MANUAL_REGEN_COMMAND: &str = "AGENTFS_UPDATE_MANUAL=1 cargo +nightly test -p agentfs-cli --lib docs::tests::manual_help_parity -- --exact";
const GENERATED_BEGIN: &str = "<!-- BEGIN GENERATED COMMAND REFERENCE (do not edit by hand) -->";
const GENERATED_END: &str = "<!-- END GENERATED COMMAND REFERENCE -->";

/// Render the generated command-reference region of docs/MANUAL.md,
/// markers included.
fn generated_manual_commands() -> String {
    let cmd = Args::command();
    let mut out = String::new();
    out.push_str(GENERATED_BEGIN);
    out.push('\n');
    out.push_str("<!-- Regenerate with: `");
    out.push_str(MANUAL_REGEN_COMMAND);
    out.push_str("` -->\n\n");
    out.push_str("## Commands\n\n");
    out.push_str(
        "Every section below is generated from the clap definitions the binary \
         actually parses; `agentfs <command> --help` and this reference cannot \
         disagree.\n",
    );
    for sub in visible_subcommands(&cmd) {
        render_command(&mut out, sub, &CommandContext::root(), 3);
    }
    out.push('\n');
    out.push_str(GENERATED_END);
    out.push('\n');
    out
}

fn visible_subcommands(cmd: &clap::Command) -> impl Iterator<Item = &clap::Command> {
    cmd.get_subcommands()
        .filter(|sub| sub.get_name() != "help" && !sub.is_hide_set())
}

/// Heading path plus the invocation prefix (which, unlike the heading,
/// carries the parent commands' positional arguments).
struct CommandContext {
    path: String,
    invocation: String,
}

impl CommandContext {
    fn root() -> Self {
        Self {
            path: "agentfs".to_string(),
            invocation: "agentfs".to_string(),
        }
    }

    fn child(&self, cmd: &clap::Command) -> Self {
        let path = format!("{} {}", self.path, cmd.get_name());
        let mut invocation = format!("{} {}", self.invocation, cmd.get_name());
        for arg in cmd.get_arguments() {
            if arg.is_positional() && !arg.is_hide_set() {
                invocation.push(' ');
                invocation.push_str(&positional_token(arg));
            }
        }
        Self { path, invocation }
    }
}

fn render_command(out: &mut String, cmd: &clap::Command, parent: &CommandContext, depth: usize) {
    let context = parent.child(cmd);

    out.push('\n');
    for _ in 0..depth {
        out.push('#');
    }
    out.push_str(&format!(" {}\n", context.path));

    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        out.push_str(&format!("\n{}\n", about.to_string().trim_end()));
    }

    out.push_str(&format!(
        "\n```\n{}\n```\n",
        synopsis(cmd, &parent.invocation)
    ));

    let positionals: Vec<&clap::Arg> = cmd
        .get_arguments()
        .filter(|arg| arg.is_positional() && !arg.is_hide_set())
        .collect();
    if !positionals.is_empty() {
        out.push_str("\n**Arguments:**\n\n");
        for arg in positionals {
            out.push_str(&format!(
                "- `{}`{}\n",
                positional_token(arg),
                arg_details(arg)
            ));
        }
    }

    let options: Vec<&clap::Arg> = cmd
        .get_arguments()
        .filter(|arg| !arg.is_positional() && !arg.is_hide_set() && !is_builtin(arg))
        .collect();
    if !options.is_empty() {
        out.push_str("\n**Options:**\n\n");
        for arg in options {
            out.push_str(&format!("- `{}`{}\n", option_token(arg), arg_details(arg)));
        }
    }

    for sub in visible_subcommands(cmd) {
        render_command(out, sub, &context, depth + 1);
    }
}

fn is_builtin(arg: &clap::Arg) -> bool {
    matches!(arg.get_id().as_str(), "help" | "version")
}

fn synopsis(cmd: &clap::Command, parent_invocation: &str) -> String {
    let mut parts = vec![format!("{parent_invocation} {}", cmd.get_name())];
    if cmd
        .get_arguments()
        .any(|arg| !arg.is_positional() && !arg.is_hide_set() && !is_builtin(arg))
    {
        parts.push("[OPTIONS]".to_string());
    }
    for arg in cmd.get_arguments() {
        if arg.is_positional() && !arg.is_hide_set() {
            parts.push(positional_token(arg));
        }
    }
    if cmd.has_subcommands() {
        parts.push(if cmd.is_subcommand_required_set() {
            "<COMMAND>".to_string()
        } else {
            "[COMMAND]".to_string()
        });
    }
    parts.join(" ")
}

fn positional_token(arg: &clap::Arg) -> String {
    let name = value_name(arg);
    let multiple = arg
        .get_num_args()
        .map(|range| range.max_values() > 1)
        .unwrap_or(false);
    let suffix = if multiple { "..." } else { "" };
    if arg.is_required_set() {
        format!("<{name}>{suffix}")
    } else {
        format!("[{name}]{suffix}")
    }
}

fn option_token(arg: &clap::Arg) -> String {
    let mut token = String::new();
    if let Some(short) = arg.get_short() {
        token.push_str(&format!("-{short}, "));
    }
    if let Some(long) = arg.get_long() {
        token.push_str(&format!("--{long}"));
    }
    if takes_value(arg) {
        token.push_str(&format!(" <{}>", value_name(arg)));
    }
    token
}

fn takes_value(arg: &clap::Arg) -> bool {
    !matches!(
        arg.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
    )
}

fn value_name(arg: &clap::Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_ascii_uppercase())
}

fn arg_details(arg: &clap::Arg) -> String {
    let mut details = String::new();

    if let Some(help) = arg.get_long_help().or_else(|| arg.get_help()) {
        let help = help.to_string();
        let help = help.trim_end();
        details.push_str(" — ");
        // Two-space indentation keeps multi-line help inside the list item.
        details.push_str(&help.replace('\n', "\n  "));
    }

    let mut meta = Vec::new();
    if takes_value(arg) {
        let possible: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|value| !value.is_hide_set())
            .map(|value| value.get_name().to_string())
            .collect();
        if !possible.is_empty() {
            meta.push(format!("possible values: {}", possible.join(", ")));
        }
        let defaults: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        if !defaults.is_empty() {
            meta.push(format!("default: {}", defaults.join(", ")));
        }
    }
    if let Some(env) = arg.get_env() {
        meta.push(format!("env: {}", env.to_string_lossy()));
    }
    if !meta.is_empty() {
        details.push_str(&format!(" [{}]", meta.join("; ")));
    }
    details
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::path::{Path, PathBuf};

    // The checked-in manual is generated from the Linux (first-tier) surface;
    // platform-dependent defaults such as --backend render differently on
    // macOS, so parity is asserted only where the doc is generated.
    #[cfg(target_os = "linux")]
    #[test]
    fn manual_help_parity() {
        let manual_path = repo_root().join("docs").join("MANUAL.md");
        let manual = std::fs::read_to_string(&manual_path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", manual_path.display()));
        let expected = generated_manual_commands();

        let begin = manual.find(GENERATED_BEGIN).unwrap_or_else(|| {
            panic!(
                "{} is missing the `{GENERATED_BEGIN}` marker; regenerate with:\n    {}",
                manual_path.display(),
                MANUAL_REGEN_COMMAND
            )
        });
        let end_marker = format!("{GENERATED_END}\n");
        let end = manual[begin..]
            .find(&end_marker)
            .map(|offset| begin + offset + end_marker.len())
            .unwrap_or_else(|| {
                panic!(
                    "{} is missing the `{GENERATED_END}` marker after the begin marker",
                    manual_path.display()
                )
            });

        if std::env::var_os(MANUAL_REGEN_ENV).is_some() {
            let updated = format!("{}{}{}", &manual[..begin], expected, &manual[end..]);
            std::fs::write(&manual_path, updated)
                .unwrap_or_else(|err| panic!("failed to rewrite {}: {err}", manual_path.display()));
        }

        let actual = std::fs::read_to_string(&manual_path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", manual_path.display()));
        let begin = actual
            .find(GENERATED_BEGIN)
            .expect("begin marker survives regeneration");
        let end = actual[begin..]
            .find(&end_marker)
            .map(|offset| begin + offset + end_marker.len())
            .expect("end marker survives regeneration");
        assert_eq!(
            &actual[begin..end],
            expected,
            "{} command reference is stale; regenerate with:\n    {}",
            manual_path.display(),
            MANUAL_REGEN_COMMAND
        );
    }

    #[test]
    fn generated_manual_documents_regeneration_command() {
        let generated = generated_manual_commands();
        assert!(
            generated.contains(MANUAL_REGEN_COMMAND),
            "generated command reference must document the one-command regeneration flow"
        );
    }

    #[test]
    fn generated_manual_covers_every_clap_command_path() {
        let generated = generated_manual_commands();
        let cmd = Args::command();
        let mut missing = Vec::new();
        collect_paths(&cmd, "agentfs", &mut |path| {
            let heading = format!(" {path}\n");
            if !generated.contains(&heading) {
                missing.push(path.to_string());
            }
        });
        assert!(
            missing.is_empty(),
            "generated command reference is missing sections for: {missing:?}"
        );
    }

    fn collect_paths(cmd: &clap::Command, parent: &str, visit: &mut impl FnMut(&str)) {
        for sub in visible_subcommands(cmd) {
            let path = format!("{parent} {}", sub.get_name());
            visit(&path);
            collect_paths(sub, &path, visit);
        }
    }

    #[cfg(target_os = "linux")]
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("agentfs-cli crate should live two levels below repo root")
            .to_path_buf()
    }
}
