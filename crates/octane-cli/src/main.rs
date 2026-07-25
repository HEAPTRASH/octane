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

    /// Run one tool directly, bypassing the model.
    ///
    /// The fastest way to answer "is the sandbox actually on?" and "why did that
    /// edit fail?" without spending a token. Permissions and containment apply
    /// exactly as they would mid-session.
    Tool {
        /// Tool name, e.g. `read` or `bash`.
        name: String,
        /// Arguments as JSON, e.g. '{"path":"src/main.rs"}'.
        input: String,
    },
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
        Some(Command::Tool { name, input }) => {
            // Built here rather than in main so the async runtime is only started
            // when something actually needs it.
            tokio::runtime::Runtime::new()?
                .block_on(run_tool(&name, &input, &workspace, sandbox, mode))
        }
        None => tokio::runtime::Runtime::new()?.block_on(interactive(
            &workspace,
            sandbox,
            mode,
            cli.model.as_deref(),
        )),
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

/// Execute a single tool, applying the same policy and containment a turn would.
async fn run_tool(
    name: &str,
    input: &str,
    workspace: &Utf8PathBuf,
    sandbox: SandboxPolicy,
    mode: PermissionMode,
) -> Result<()> {
    use octane_permission::{Decision, Policy, Resource, Scope};
    use octane_protocol::{SessionId, ToolCallId};
    use octane_tools::{ToolContext, ToolRegistry};

    let tracker = std::sync::Arc::new(octane_tools::FileTracker::new());
    let mut registry = ToolRegistry::new();
    octane_tools::register_all(&mut registry, tracker, sandbox);

    let tool = registry.get(name).ok_or_else(|| {
        anyhow::anyhow!(
            "no tool named {name:?}. Available: {}",
            registry.names().collect::<Vec<_>>().join(", ")
        )
    })?;

    let ctx = ToolContext {
        session_id: SessionId::new(),
        call_id: ToolCallId::new(),
        agent: "build".into(),
        workspace: workspace.clone(),
        cwd: workspace.clone(),
        cancel: Default::default(),
    };

    let (policy, rule_errors) = Policy::builder()
        .workspace_root(workspace.as_str())
        .with_baseline_denies()
        .ask("command(*)", Scope::User)
        .build();
    for error in &rule_errors {
        tracing::warn!("ignoring malformed permission rule: {error}");
    }

    // Same order as the turn runner: resolve every resource before executing, so
    // a refusal happens before anything runs rather than halfway through.
    for resource in tool.required_permissions(input, &ctx) {
        let Ok(parsed) = resource.parse::<Resource>() else {
            continue;
        };
        let verdict = policy.evaluate(&parsed, mode);
        match verdict.decision {
            Decision::Allow => {}
            Decision::Deny => anyhow::bail!("denied: {parsed} ({:?})", verdict.reason),
            // No TUI to prompt with yet. Refusing beats silently proceeding.
            Decision::Ask => anyhow::bail!(
                "{parsed} needs approval, and this subcommand cannot prompt. \
                 Add an allow rule, or re-run with --mode accept-edits for file writes."
            ),
        }
    }

    match tool.execute(input, &ctx).await {
        Ok(outcome) => {
            println!("{}", outcome.output);
            Ok(())
        }
        Err(error) => anyhow::bail!("{error}"),
    }
}

/// The interactive session.
///
/// Drives the TUI directly. Model inference is not wired up yet, so submissions
/// that need it say so rather than hanging — but `!` shell commands and `/`
/// commands work end to end, which makes the whole input path exercisable.
async fn interactive(
    workspace: &Utf8PathBuf,
    sandbox: SandboxPolicy,
    mode: PermissionMode,
    model: Option<&str>,
) -> Result<()> {
    use octane_protocol::{Item, ItemId, ItemKind, ItemStatus, ToolCallId, TurnId};
    use octane_tui::{App, AppEvent, StatusLine, Submission};

    let contained = sandbox.is_contained();

    // Logo, tagline, and hints go to stdout *before* the inline viewport starts,
    // so they land in real scrollback and scroll away naturally as the session
    // grows. The animation has to happen here too: once content is committed to
    // scrollback it is never redrawn.
    let theme = octane_tui::Theme::default();
    let glyphs = octane_tui::Glyphs::detect();
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        octane_tui::banner::draw(
            &mut stdout,
            width,
            &theme,
            &glyphs,
            octane_tui::banner::should_animate(is_tty, theme.depth),
        )?;
        write!(stdout, "\n{}", octane_tui::banner::tips(width, &theme, &glyphs, workspace.as_str(), contained))?;
        stdout.flush()?;
    }

    let mut app = App::new(StatusLine {
        mode,
        model: model.unwrap_or("unset").to_string(),
        glyphs,
        ..Default::default()
    })?;
    // The detected set must reach the transcript too, or the fallback applies to
    // the banner and nothing else.
    app.options_mut().glyphs = glyphs;
    app.options_mut().theme = theme;

    let completed = |kind: ItemKind| {
        octane_protocol::Event::Item(octane_protocol::ItemEvent::Completed {
            turn_id: TurnId::new(),
            item: Item { id: ItemId::new(), kind, status: ItemStatus::Completed },
        })
    };

    loop {
        app.draw()?;

        let Some(event) = app.poll()? else { continue };

        match event {
            AppEvent::Exit => break,
            AppEvent::Interrupt => {}
            AppEvent::ModeChanged(_) => {}

            AppEvent::Submit(Submission::Command { name, .. }) => {
                app.push_event(&completed(ItemKind::UserMessage { text: format!("/{name}") }))?;
                let body = match name.as_str() {
                    "help" => HELP.to_string(),
                    "exit" | "quit" => break,
                    other => format!("Unknown command /{other}. Try /help."),
                };
                app.push_event(&completed(ItemKind::AgentMessage { text: body }))?;
            }

            AppEvent::Submit(Submission::Shell { command }) => {
                app.push_event(&completed(ItemKind::UserMessage {
                    text: format!("!{command}"),
                }))?;
                let output = run_shell(&command, workspace, &sandbox).await;
                app.push_event(&completed(ItemKind::ToolExecution {
                    call_id: ToolCallId::new(),
                    name: "bash".into(),
                    input: serde_json::json!({
                        "command": command,
                        "description": "user-issued shell command"
                    })
                    .to_string(),
                }))?;
                app.push_event(&completed(ItemKind::AgentMessage { text: output }))?;
            }

            AppEvent::Submit(Submission::Prompt { text, .. }) => {
                app.push_event(&completed(ItemKind::UserMessage { text }))?;
                app.push_event(&completed(ItemKind::Error {
                    message: "No model is wired up yet. `!command` and `/help` work."
                        .into(),
                }))?;
            }
        }
    }

    app.restore()?;
    Ok(())
}

const HELP: &str = "\
  /help              show this
  /exit              quit
  !<command>         run a shell command
  @path              reference a file in a prompt

  shift+tab          cycle permission mode
  shift+enter        newline
  ctrl+u             clear the input
  esc                interrupt while working
  ctrl+c             exit";

/// Run a shell command through the `bash` tool, so it gets the same containment
/// and bounds an agent-issued command would.
async fn run_shell(
    command: &str,
    workspace: &Utf8PathBuf,
    sandbox: &SandboxPolicy,
) -> String {
    use octane_protocol::{SessionId, ToolCallId};
    use octane_tools::{BashTool, Tool, ToolContext};

    let tool = BashTool::new(sandbox.clone());
    let ctx = ToolContext {
        session_id: SessionId::new(),
        call_id: ToolCallId::new(),
        agent: "build".into(),
        workspace: workspace.clone(),
        cwd: workspace.clone(),
        cancel: Default::default(),
    };
    let input = serde_json::json!({
        "command": command,
        "description": "user-issued shell command"
    })
    .to_string();

    match tool.execute(&input, &ctx).await {
        Ok(outcome) => outcome.output,
        Err(error) => error.to_string(),
    }
}
