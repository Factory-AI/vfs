use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use clap::ValueEnum;

use crate::opts::CompletionsCommand;

/// Current shell completions supported by `clap_complete`
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    PowerShell,
}

impl fmt::Display for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Shell::Bash => write!(f, "bash"),
            Shell::Zsh => write!(f, "zsh"),
            Shell::Fish => write!(f, "fish"),
            Shell::Elvish => write!(f, "elvish"),
            Shell::PowerShell => write!(f, "powershell"),
        }
    }
}

impl Shell {
    /// Detect the current shell from the `SHELL` environment variable
    fn detect() -> Option<Shell> {
        let shell_path = crate::config::current_shell_path()?;
        let shell_name = shell_path.rsplit('/').next()?;
        match shell_name {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "elvish" => Some(Shell::Elvish),
            "pwsh" | "powershell" => Some(Shell::PowerShell),
            _ => None,
        }
    }

    /// Get the config file path for this shell
    fn config_path(&self) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        match self {
            Shell::Bash => Some(home.join(".bashrc")),
            Shell::Zsh => Some(home.join(".zshrc")),
            Shell::Fish => Some(dirs::config_dir()?.join("fish/config.fish")),
            Shell::Elvish => Some(dirs::config_dir()?.join("elvish/rc.elv")),
            Shell::PowerShell => {
                let config = dirs::config_dir()?;
                Some(config.join("powershell/Microsoft.PowerShell_profile.ps1"))
            }
        }
    }

    /// Get the completion source line for this shell
    fn completion_line(&self) -> &'static str {
        match self {
            Shell::Bash => "source <(COMPLETE=bash agentfs)",
            Shell::Zsh => "source <(COMPLETE=zsh agentfs)",
            Shell::Fish => "COMPLETE=fish agentfs | source",
            Shell::Elvish => "eval (COMPLETE=elvish agentfs | slurp)",
            Shell::PowerShell => "$env:COMPLETE = \"powershell\"; agentfs | Out-String | Invoke-Expression; Remove-Item Env:\\COMPLETE",
        }
    }
}

pub fn handle_completions(command: CompletionsCommand) {
    match command {
        CompletionsCommand::Install { shell } => {
            let shell = match shell.or_else(Shell::detect) {
                Some(s) => s,
                None => {
                    eprintln!(
                        "Error: Could not detect current shell. Please specify a shell explicitly."
                    );
                    std::process::exit(1)
                }
            };
            if let Err(err) = install(shell) {
                eprintln!("Error: {err}");
                std::process::exit(1)
            }
        }
        CompletionsCommand::Uninstall { shell } => {
            let shell = match shell.or_else(Shell::detect) {
                Some(s) => s,
                None => {
                    eprintln!(
                        "Error: Could not detect current shell. Please specify a shell explicitly."
                    );
                    std::process::exit(1)
                }
            };
            if let Err(err) = uninstall(shell) {
                eprintln!("Error: {err}");
                std::process::exit(1)
            }
        }
        CompletionsCommand::Show => show(),
    }
}

fn install(shell: Shell) -> io::Result<()> {
    // Warn if shell doesn't match current shell
    if let Some(current) = Shell::detect() {
        if current != shell {
            eprintln!(
                "Warning: Installing completions for {} but your current shell is {}",
                shell, current
            );
        }
    }

    // Get config path
    let config_path = shell.config_path().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "Could not determine config path")
    })?;

    let completion_line = shell.completion_line();

    // Check if already installed
    if let Ok(contents) = fs::read_to_string(&config_path) {
        if contents.contains(completion_line) {
            println!("Completions already installed in {}", config_path.display());
            return Ok(());
        }
    }

    // Create parent dirs if needed
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Append completion line
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)?;
    writeln!(file, "\n{}", completion_line)?;

    println!(
        "Installed {} completions in {}",
        shell,
        config_path.display()
    );
    println!(
        "Restart your shell or run: source {}",
        config_path.display()
    );
    Ok(())
}

fn uninstall(shell: Shell) -> io::Result<()> {
    // Get config path
    let config_path = shell.config_path().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "Could not determine config path")
    })?;

    let completion_line = shell.completion_line();

    // Read file
    let contents = fs::read_to_string(&config_path)?;

    if !contents.contains(completion_line) {
        println!("No completions found in {}", config_path.display());
        return Ok(());
    }

    // Filter out the completion line
    let lines: Vec<&str> = contents
        .lines()
        .filter(|line| !line.contains(completion_line))
        .collect();

    // Write back
    fs::write(&config_path, lines.join("\n") + "\n")?;
    println!("Removed completions from {}", config_path.display());
    println!("Restart your shell to apply changes.");
    Ok(())
}

fn show() {
    println!("Add one of the following lines to your shell configuration file:\n");

    println!("Bash (~/.bashrc):");
    println!("  {}\n", Shell::Bash.completion_line());

    println!("Zsh (~/.zshrc):");
    println!("  {}\n", Shell::Zsh.completion_line());

    println!("Fish (~/.config/fish/config.fish):");
    println!("  {}\n", Shell::Fish.completion_line());

    println!("Elvish (~/.config/elvish/rc.elv):");
    println!("  {}\n", Shell::Elvish.completion_line());

    println!("PowerShell (~/.config/powershell/Microsoft.PowerShell_profile.ps1):");
    println!("  {}\n", Shell::PowerShell.completion_line());

    println!("Then restart your shell or source your config file.");
}
