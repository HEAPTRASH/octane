//! The `octane` binary.
//!
//! Deliberately thin: argument parsing, config resolution, and handing control to
//! a client. All agent logic lives in `octane-core`, which is a library with an
//! event stream — so a second surface (an RPC server, an editor extension) can be
//! added without touching the loop. Codex, opencode, and Antigravity all converged
//! on that split, and Codex's in-process TUI is the part they are refactoring away
//! from. Starting there costs nothing now and a rewrite later.

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use octane_permission::PermissionMode;
use octane_sandbox::SandboxPolicy;

#[derive(Debug, Parser)]
#[command(name = "octane", version, about = "An AI coding agent for the terminal")]
struct Cli {
    /// Starting permission mode. Shift+Tab cycles between the safe modes.
    #[arg(long, value_enum, default_value = "default")]
    mode: ModeArg,

    /// Model, as `provider/model`.
    #[arg(long, short = 'm', env = "OCTANE_MODEL")]
    model: Option<String>,

    /// Project root. Defaults to the working directory.
    #[arg(long)]
    workspace: Option<Utf8PathBuf>,

    /// Run commands without OS containment.
    ///
    /// Separate from `--mode` on purpose: policy and containment are independent
    /// layers, and collapsing them into one flag makes it impossible to have
    /// "ask me about everything, and also confine it".
    #[arg(long)]
    no_sandbox: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ModeArg {
    Default,
    Plan,
    AcceptEdits,
    /// Skips prompts. Deny rules and the sandbox still apply.
    Bypass,
}

impl From<ModeArg> for PermissionMode {
    fn from(arg: ModeArg) -> Self {
        match arg {
            ModeArg::Default => Self::Default,
            ModeArg::Plan => Self::Plan,
            ModeArg::AcceptEdits => Self::AcceptEdits,
            ModeArg::Bypass => Self::Bypass,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the resolved configuration and exit.
    ///
    /// Exists because "why did it ask me that?" and "what can it write to?" are
    /// the two questions users actually have, and both should be answerable
    /// without starting a session.
    Doctor,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OCTANE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let workspace = match cli.workspace {
        Some(path) => path,
        None => Utf8PathBuf::try_from(std::env::current_dir()?)?,
    };

    let sandbox = if cli.no_sandbox {
        SandboxPolicy::DangerFullAccess
    } else {
        SandboxPolicy::workspace(workspace.clone(), std::env::var("TMPDIR").ok().map(Into::into))
    };

    let mode: PermissionMode = cli.mode.into();

    match cli.command {
        Some(Command::Doctor) => doctor(&workspace, &sandbox, mode, cli.model.as_deref()),
        None => {
            // The interactive TUI is not wired up yet. Saying so plainly beats a
            // panic or a silent no-op.
            println!("octane {}", env!("CARGO_PKG_VERSION"));
            println!();
            println!("The interactive session is not wired up yet.");
            println!("Run `octane doctor` to inspect the resolved configuration.");
            Ok(())
        }
    }
}

fn doctor(
    workspace: &Utf8PathBuf,
    sandbox: &SandboxPolicy,
    mode: PermissionMode,
    model: Option<&str>,
) -> Result<()> {
    println!("octane {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("workspace     {workspace}");
    println!("model         {}", model.unwrap_or("<unset>"));
    println!("mode          {}", mode.label());

    println!();
    println!("sandbox");
    match sandbox {
        SandboxPolicy::DangerFullAccess => {
            println!("  containment  NONE — commands run unconfined");
        }
        SandboxPolicy::ExternalSandbox => {
            println!("  containment  external (not double-wrapped)");
        }
        SandboxPolicy::ReadOnly { network } => {
            println!("  containment  read-only");
            println!("  network      {network:?}");
        }
        SandboxPolicy::WorkspaceWrite { writable_roots, network } => {
            println!("  containment  workspace-write");
            println!("  network      {network:?}");
            for root in writable_roots {
                println!("  writable     {}", root.path);
                for carved in &root.read_only_subpaths {
                    // Surfaced explicitly: these carve-outs are what stop a
                    // "write a file in the project" grant from reaching
                    // .git/hooks and becoming code execution.
                    println!("    read-only  {carved}");
                }
            }
        }
    }

    println!();
    println!("memory files searched, in load order");
    for name in octane_memory::MEMORY_FILENAMES {
        println!("  {name}");
    }
    println!("  {}", octane_memory::LOCAL_MEMORY_FILENAME);

    Ok(())
}
