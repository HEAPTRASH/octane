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
    /// List configured providers and models.
    Models,

    /// Print the resolved settings and where they came from.
    Settings,

    /// Set up a provider.
    ///
    /// Writes `.octane/providers/<name>.json` and says which environment
    /// variable to set. Run without a name to see what is available.
    Connect {
        /// Provider to configure, e.g. `anthropic`, `openrouter`, `ollama`.
        provider: Option<String>,
        /// Write to `~/.octane` instead of the project.
        #[arg(long)]
        user: bool,
    },

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

    // Settings are the baseline; command-line flags override them, because a
    // flag is a deliberate one-off and a file is a standing preference.
    let (settings, settings_errors) =
        octane_config::Settings::load(&octane_config::roots(&workspace));

    let sandboxed = !cli.no_sandbox && settings.sandbox.unwrap_or(true);
    let sandbox = if sandboxed {
        SandboxPolicy::workspace(workspace.clone(), std::env::var("TMPDIR").ok().map(Into::into))
    } else {
        SandboxPolicy::DangerFullAccess
    };

    // `--mode` has a default, so it cannot be distinguished from unset. The
    // settings file wins only when the flag was left at its default.
    let mode: PermissionMode = match (cli.mode, settings.mode) {
        (ModeArg::Default, Some(configured)) => configured,
        (flag, _) => flag.into(),
    };

    match cli.command {
        Some(Command::Models) => list_models(&workspace),
        Some(Command::Settings) => {
            print!("{}", render_settings(&workspace));
            Ok(())
        }
        Some(Command::Connect { provider, user }) => connect(&workspace, provider.as_deref(), user),
        Some(Command::Doctor) => doctor(
            &workspace,
            &sandbox,
            mode,
            cli.model.as_deref().or(settings.model.as_deref()),
        ),
        
        Some(Command::Tool { name, input }) => {
            // Built here rather than in main so the async runtime is only started
            // when something actually needs it.
            tokio::runtime::Runtime::new()?
                .block_on(run_tool(&name, &input, &workspace, sandbox, mode))
        }
        None => {
            // Resolved before the move, since the flag takes precedence and the
            // settings value is owned by what is about to be handed over.
            let model = cli.model.clone().or_else(|| settings.model.clone());
            tokio::runtime::Runtime::new()?.block_on(interactive(
                &workspace,
                sandbox,
                mode,
                model.as_deref(),
                settings,
                settings_errors,
            ))
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

    // Resolved, not echoed: printing back what was typed answers nothing.
    let registry = registry(workspace);
    match model {
        Some(reference) => match registry.resolve(reference) {
            Ok(resolved) => println!(
                "model         {}  ({}, {})",
                resolved.reference, resolved.model_id, resolved.api
            ),
            Err(error) => println!("model         {reference}  — {error}"),
        },
        None => match registry.resolve_role(None, octane_provider::Role::Primary) {
            Ok(resolved) => println!("model         {} (default)", resolved.reference),
            Err(_) => println!("model         <none configured>"),
        },
    }
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
    settings: octane_config::Settings,
    settings_errors: Vec<octane_config::SettingsError>,
) -> Result<()> {
    use octane_protocol::{ItemKind, ToolCallId};
    use octane_tui::{App, AppEvent, Candidate, StatusLine, Submission};

    let contained = sandbox.is_contained();

    // Resolved up front so the status line names a model that actually exists,
    // rather than echoing back whatever was typed on the command line.
    let registry = registry(workspace);
    let selected = match model {
        Some(reference) => registry.resolve(reference),
        None => registry.resolve_role(None, octane_provider::Role::Primary),
    };
    let model_label = match &selected {
        Ok(resolved) => resolved.reference.clone(),
        Err(_) => "none".to_string(),
    };

    let mut app = App::new(
        StatusLine { mode, model: model_label, ..Default::default() },
        workspace.to_string(),
        contained,
    )?;

    // Built once. A failure here is reported and the session continues without
    // a model, so `/connect` still works.
    let session: Option<std::sync::Arc<dyn octane_provider::LanguageModel>> = match &selected {
        Ok(resolved) => match octane_provider::connect(resolved.clone()) {
            Ok(model) => Some(model),
            Err(error) => {
                app.push_event(&completed_static(ItemKind::Error {
                    message: format!("{}: {error}", resolved.reference),
                }))?;
                None
            }
        },
        Err(_) => None,
    };
    let mut history: Vec<octane_protocol::Message> = Vec::new();
    // Runtime override for the model's configured default.
    let mut thinking = settings.thinking.unwrap_or_else(|| {
        selected.as_ref().map(|resolved| resolved.thinking).unwrap_or_default()
    });

    if settings.show_reasoning.unwrap_or(false) {
        app.options_mut().reasoning = octane_tui::render::Reasoning::Shown;
    }

    // Configuration problems are said once, at the top, rather than surfacing as
    // a confusing failure on the first prompt.
    for error in &settings_errors {
        app.push_event(&completed_static(ItemKind::Error { message: error.to_string() }))?;
    }
    for error in registry.errors() {
        app.push_event(&completed_static(ItemKind::Error { message: error.to_string() }))?;
    }
    if let Err(error) = &selected {
        app.push_event(&completed_static(ItemKind::Error {
            message: format!("{error}. Run `octane models` to see what is configured."),
        }))?;
    }

    app.set_commands(COMMANDS.iter().map(|(name, detail)| Candidate::new(*name, *detail)).collect());
    // Walked once at startup. A watcher would keep it live, but a stale entry
    // costs a "no such file" and a rescan costs a directory walk on every
    // keystroke — the wrong trade for a completion list.
    app.set_files(index_files(workspace));


    loop {
        app.draw()?;

        let Some(event) = app.poll()? else { continue };

        match event {
            AppEvent::Exit => break,
            AppEvent::Interrupt => {}
            AppEvent::ModeChanged(_) => {}

            AppEvent::Submit(Submission::Command { name, args }) => {
                let echoed =
                    if args.is_empty() { format!("/{name}") } else { format!("/{name} {args}") };
                app.push_event(&completed_static(ItemKind::UserMessage { text: echoed }))?;
                let body = match name.as_str() {
                    "thinking" => set_thinking(&mut app, &mut thinking, &args),
                    "help" => HELP.to_string(),
                    "models" => render_models(workspace),
                    "connect" => render_connect_list(),
                    "agents" => render_agents(workspace),
                    "settings" => render_settings(workspace),
                    "clear" => {
                        app.clear_transcript();
                        continue;
                    }
                    "exit" | "quit" => break,
                    other => format!("Unknown command /{other}. Try /help."),
                };
                app.push_event(&completed_static(ItemKind::AgentMessage { text: body }))?;
            }

            AppEvent::Submit(Submission::Shell { command }) => {
                app.push_event(&completed_static(ItemKind::UserMessage {
                    text: format!("!{command}"),
                }))?;
                app.push_event(&completed_static(ItemKind::ToolExecution {
                    call_id: ToolCallId::new(),
                    name: "bash".into(),
                    input: serde_json::json!({
                        "command": command,
                        "description": "user-issued shell command"
                    })
                    .to_string(),
                }))?;
                let output = run_shell(&command, workspace, &sandbox).await;
                app.push_event(&completed_static(ItemKind::AgentMessage { text: output }))?;
            }

            AppEvent::Submit(Submission::Prompt { text, file_references }) => {
                app.push_event(&completed_static(ItemKind::UserMessage { text: text.clone() }))?;

                // `@path` attaches the file's contents through the `read` tool,
                // so a reference gets the same limits a model-issued read does.
                let mut prompt = text.clone();
                for reference in &file_references {
                    match attach(reference, workspace).await {
                        Ok(attached) => {
                            app.push_event(&completed_static(ItemKind::ToolExecution {
                                call_id: ToolCallId::new(),
                                name: "read".into(),
                                input: serde_json::json!({ "path": reference }).to_string(),
                            }))?;
                            prompt.push_str("\n\n");
                            prompt.push_str(&attached);
                        }
                        Err(error) => {
                            app.push_event(&completed_static(ItemKind::Error {
                                message: format!("@{reference}: {error}"),
                            }))?;
                        }
                    }
                }

                let Some(model) = session.as_ref() else {
                    app.push_event(&completed_static(ItemKind::Error {
                        message: "No model is configured. Run `/connect` to set one up.".into(),
                    }))?;
                    continue;
                };

                history.push(octane_protocol::Message::text(
                    octane_protocol::Role::User,
                    prompt,
                ));

                let session = Session {
                    model,
                    workspace,
                    sandbox: &sandbox,
                    mode,
                    thinking,
                    permissions: &settings.permissions,
                };
                run_turn(&mut app, &session, &mut history).await?;
            }
        }
    }

    app.restore()?;
    Ok(())
}

/// Slash commands offered by completion.
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show the key reference"),
    ("/models", "list configured models"),
    ("/connect", "set up a provider"),
    ("/thinking", "show or set reasoning: off, low, medium, high"),
    ("/agents", "list available agents"),
    ("/settings", "show resolved settings"),
    ("/clear", "clear the transcript"),
    ("/exit", "quit octane"),
];

/// Config roots, least specific first, so a project file wins.
fn config_roots(workspace: &Utf8PathBuf) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(std::path::PathBuf::from(home).join(".octane"));
    }
    roots.push(workspace.join(".octane").into_std_path_buf());
    roots
}

fn registry(workspace: &Utf8PathBuf) -> octane_provider::Registry {
    octane_provider::Registry::load(&config_roots(workspace))
}

/// Render the model list, shared by `octane models` and `/models`.
fn render_models(workspace: &Utf8PathBuf) -> String {
    let registry = registry(workspace);
    let mut out = String::new();

    for error in registry.errors() {
        out.push_str(&format!("  ! {error}\n"));
    }
    if !registry.errors().is_empty() {
        out.push('\n');
    }

    // Said plainly, because a provider that silently vanishes is how someone
    // spends an hour on an unset variable they cannot see.
    let unavailable = registry.unavailable();
    if !unavailable.is_empty() {
        out.push_str("not available\n");
        for (provider, reason) in &unavailable {
            out.push_str(&format!("  {provider:<28} {reason}\n"));
        }
        out.push('\n');
    }

    let models = registry.models();
    if models.is_empty() && unavailable.is_empty() {
        out.push_str(
            "No providers are configured.\n\n\
             Set an API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY),\n\
             or drop a provider file in .octane/providers/<name>.json:\n\n",
        );
        out.push_str(EXAMPLE_PROVIDER);
        return out;
    }

    let mut provider = String::new();
    for model in models {
        if model.provider != provider {
            provider = model.provider.clone();
            out.push_str(&format!("\n{provider}\n"));
        }
        // The reference is what `--model` takes, so it leads.
        out.push_str(&format!(
            "  {:<28} {:<28} {}k ctx  {}\n",
            model.reference,
            model.display_name,
            model.context_window / 1000,
            model.api
        ));
    }
    out
}

fn list_models(workspace: &Utf8PathBuf) -> Result<()> {
    print!("{}", render_models(workspace));
    Ok(())
}

/// Set up a provider, or list what can be set up.
fn connect(workspace: &Utf8PathBuf, provider: Option<&str>, user_scope: bool) -> Result<()> {
    use octane_provider::connect as setup;

    let Some(key) = provider else {
        print!("{}", render_connect_list());
        return Ok(());
    };

    let recipe = setup::recipe(key).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider {key:?}. Run `octane connect` to see what is available."
        )
    })?;

    let root = if user_scope {
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".octane")
    } else {
        workspace.join(".octane").into_std_path_buf()
    };

    let config = setup::build_config(&recipe);
    let path = setup::write_config(&root, &recipe, &config)?;

    println!("Wrote {}", path.display());
    println!();
    for (key, model) in &config.models {
        println!("  {}/{key:<12} {}", recipe.key, model.name.as_deref().unwrap_or(""));
    }

    // The credential is a ${VAR} reference, so the file is safe to commit and
    // the secret stays in the environment. Say so, or people paste keys in.
    if let Some(variable) = recipe.credential.env_var() {
        println!();
        if setup::is_satisfied(&recipe, |name| std::env::var(name).ok()) {
            println!("  {variable} is set.");
        } else {
            println!("  Set {variable} to finish:");
            println!("    export {variable}=...        # {}", recipe.help_url);
            println!();
            println!("  The file references the variable rather than storing the key,");
            println!("  so it is safe to commit.");
        }
    }
    if let Some(note) = recipe.note {
        println!();
        println!("  Note: {note}");
    }
    Ok(())
}

/// The provider menu, shared by `octane connect` and `/connect`.
fn render_connect_list() -> String {
    use octane_provider::connect as setup;

    let mut out = String::from("Providers octane can set up:\n\n");
    for recipe in setup::recipes() {
        let state = match &recipe.credential {
            octane_provider::Credential::None => "ready".to_string(),
            octane_provider::Credential::TokenFile => "needs a token file".to_string(),
            octane_provider::Credential::ApiKey { env_var } => {
                if setup::is_satisfied(&recipe, |name| std::env::var(name).ok()) {
                    format!("{env_var} is set")
                } else {
                    format!("needs {env_var}")
                }
            }
        };
        out.push_str(&format!("  {:<12} {:<22} {state}\n", recipe.key, recipe.name));
    }
    out.push_str("\n  octane connect <name>          write .octane/providers/<name>.json\n");
    out.push_str("  octane connect <name> --user   write it to ~/.octane instead\n");
    out
}

const EXAMPLE_PROVIDER: &str = r#"{
  "api": "openai-completion",
  "baseUrl": "https://openrouter.ai/api/v1",
  "auth": { "type": "apiKey", "value": "${OPENROUTER_API_KEY}" },
  "defaults": { "primary": "sonnet", "faster": "haiku" },
  "models": {
    "sonnet": { "id": "anthropic/claude-sonnet-4.5", "contextWindow": 200000 },
    "haiku":  { "id": "anthropic/claude-haiku-4.5" }
  }
}
"#;

/// Walk the workspace for `@` completion candidates.
///
/// Bounded and gitignore-aware, so the list is files someone might mean rather
/// than every artifact in `target/`.
fn index_files(workspace: &Utf8PathBuf) -> Vec<String> {
    use octane_tools::walk::{self, WalkOptions};

    let options = WalkOptions { limit: 20_000, ..Default::default() };
    walk::walk(workspace, &options, |_, is_dir| !is_dir)
        .entries
        .into_iter()
        .map(|entry| walk::display_path(&entry.path, workspace))
        .collect()
}

/// Read a referenced file, fenced with its path.
///
/// Goes through the `read` tool rather than `std::fs`, so an `@` reference is
/// bounded by the same size limits and binary checks a model-issued read is.
async fn attach(reference: &str, workspace: &Utf8PathBuf) -> Result<String> {
    use octane_protocol::{SessionId, ToolCallId};
    use octane_tools::{FileTracker, ReadTool, Tool, ToolContext};

    let tool = ReadTool::new(std::sync::Arc::new(FileTracker::new()));
    let ctx = ToolContext {
        session_id: SessionId::new(),
        call_id: ToolCallId::new(),
        agent: "build".into(),
        workspace: workspace.clone(),
        cwd: workspace.clone(),
        cancel: Default::default(),
    };
    let input = serde_json::json!({ "path": reference }).to_string();

    // Awaited, not blocked on. `Handle::block_on` from inside the runtime panics
    // with "cannot block the current thread from within a runtime", which takes
    // the whole session down the first time anyone uses an `@` reference.
    let outcome = tool
        .execute(&input, &ctx)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    Ok(outcome.output)
}

const HELP: &str = "\
  input
    /               commands, with completion
    @path           attach a file, with fuzzy completion
    !command        run a shell command
    tab             accept a completion
    shift+enter     newline (the box grows)
    alt+enter       newline, on terminals without shift+enter
    \\ then enter    newline, anywhere
    ctrl+u          clear the input

  transcript
    pageup/pagedn   scroll
    shift+up/down   scroll a little
    ctrl+home/end   jump to top or bottom
    mouse wheel     scroll

  session
    shift+tab       cycle permission mode
    esc             interrupt while working
    ctrl+c          exit";

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

/// Wrap an item as a completed protocol event.
fn completed_static(kind: octane_protocol::ItemKind) -> octane_protocol::Event {
    use octane_protocol::{Item, ItemId, ItemStatus, TurnId};
    octane_protocol::Event::Item(octane_protocol::ItemEvent::Completed {
        turn_id: TurnId::new(),
        item: Item { id: ItemId::new(), kind, status: ItemStatus::Completed },
    })
}

/// `/agents` — what can be delegated to, and where each came from.
fn render_agents(workspace: &Utf8PathBuf) -> String {
    let (agents, errors) = octane_config::discover_agents(&octane_config::roots(workspace));

    let mut out = String::new();
    for error in &errors {
        out.push_str(&format!("  ! {error}\n"));
    }
    if !errors.is_empty() {
        out.push('\n');
    }

    // Sorted by mode before name, or the headers below repeat: printing one on
    // every change through an alphabetical list alternates primary/subagent.
    let mut offered: Vec<_> = agents.iter().filter(|agent| agent.is_offered()).collect();
    offered.sort_by_key(|agent| {
        (
            match agent.frontmatter.mode {
                octane_config::agent::AgentMode::Primary => 0,
                _ => 1,
            },
            agent.name.clone(),
        )
    });

    let mut last_mode = None;
    for agent in offered {
        let mode = agent.frontmatter.mode;
        if last_mode != Some(mode) {
            out.push_str(&format!(
                "\n{}\n",
                match mode {
                    octane_config::agent::AgentMode::Primary => "primary",
                    _ => "subagents",
                }
            ));
            last_mode = Some(mode);
        }
        out.push_str(&format!(
            "  {:<10} {:<10} {}\n",
            agent.name,
            agent.scope.label(),
            agent.frontmatter.description
        ));
    }

    out.push_str("\n  Define your own in .octane/agents/<name>.md\n");
    out
}

/// `/settings` — the resolved configuration, and where to change it.
fn render_settings(workspace: &Utf8PathBuf) -> String {
    let roots = octane_config::roots(workspace);
    let (settings, errors) = octane_config::Settings::load(&roots);

    let mut out = String::new();
    for error in &errors {
        out.push_str(&format!("  ! {error}\n"));
    }
    if !errors.is_empty() {
        out.push('\n');
    }

    let rendered = settings.to_toml();
    if rendered.trim().is_empty() {
        out.push_str("Nothing configured; every setting is at its default.\n");
    } else {
        out.push_str("Resolved settings\n\n");
        for line in rendered.lines() {
            out.push_str(&format!("  {line}\n"));
        }
        out.push('\n');
    }

    out.push_str("Files, later overriding earlier\n\n");
    for root in &roots {
        let path = root.join(octane_config::settings::SETTINGS_FILE);
        out.push_str(&format!(
            "  {:<52} {}\n",
            path,
            if path.is_file() { "present" } else { "absent" }
        ));
    }
    out
}

/// `/thinking` — with no argument, toggle whether reasoning is shown; with one,
/// set how hard the model thinks.
///
/// Two different things behind one command because users reach for the same word
/// for both, and separating them into `/reasoning` and `/thinking` would be a
/// distinction nobody remembers.
fn set_thinking(
    app: &mut octane_tui::App,
    thinking: &mut octane_provider::Thinking,
    args: &str,
) -> String {
    use octane_tui::render::Reasoning;

    if args.trim().is_empty() {
        let showing = app.options_mut().reasoning == Reasoning::Shown;
        app.options_mut().reasoning =
            if showing { Reasoning::Hidden } else { Reasoning::Shown };
        return format!(
            "Reasoning is now {}. Effort is {}.\n\n  /thinking off | low | medium | high | <tokens>\n",
            if showing { "hidden" } else { "shown" },
            thinking.label(),
        );
    }

    match args.parse::<octane_provider::Thinking>() {
        Ok(level) => {
            *thinking = level;
            let mut note = format!("Thinking effort set to {}.", level.label());
            // Said up front rather than discovered as a failed request: several
            // endpoints refuse to stop reasoning at all.
            if level.is_off() {
                note.push_str(concat!(
                    "\n\n",
                    "  Not every endpoint honours this. Gemini and GPT-OSS via OpenRouter\n",
                    "  both refuse outright and reason anyway. Effort levels do work there,\n",
                    "  so `/thinking low` is the lever that always does something.",
                ));
            }
            note
        }
        Err(error) => error.to_string(),
    }
}

/// Drive one turn, streaming events into the UI as they arrive.
///
/// The turn runs on its own task while this loop pumps both the event channel
/// and the terminal, so the UI keeps repainting and Esc keeps working while the
/// model is talking. Awaiting the turn directly would freeze the interface for
/// its whole duration.
/// Everything a turn needs that does not change between turns.
struct Session<'a> {
    model: &'a std::sync::Arc<dyn octane_provider::LanguageModel>,
    workspace: &'a Utf8PathBuf,
    sandbox: &'a SandboxPolicy,
    mode: PermissionMode,
    thinking: octane_provider::Thinking,
    permissions: &'a octane_config::settings::Permissions,
}

async fn run_turn(
    app: &mut octane_tui::App,
    session: &Session<'_>,
    history: &mut Vec<octane_protocol::Message>,
) -> Result<()> {
    let Session { model, workspace, sandbox, mode, thinking, permissions } = *session;
    use octane_core::{EventSink, ModelStepSource, TurnRunner};
    use octane_permission::{Policy, Scope};
    use octane_protocol::TurnId;
    use octane_tools::ToolRegistry;
    use octane_tui::{Activity, TuiApprover};

    let tracker = std::sync::Arc::new(octane_tools::FileTracker::new());
    let mut registry = ToolRegistry::new();
    octane_tools::register_all(&mut registry, tracker, sandbox.clone());
    let registry = std::sync::Arc::new(registry);

    // Configured rules layer on the baseline. Deny still beats everything, so a
    // permissive `allow` in a project file cannot widen past the baseline denies.
    let mut builder = Policy::builder()
        .workspace_root(workspace.as_str())
        .with_baseline_denies();
    for rule in &permissions.deny {
        builder = builder.deny(rule, Scope::Project);
    }
    for rule in &permissions.ask {
        builder = builder.ask(rule, Scope::Project);
    }
    for rule in &permissions.allow {
        builder = builder.allow(rule, Scope::Project);
    }
    // Nothing configured at all still asks before running commands.
    if permissions.ask.is_empty() && permissions.allow.is_empty() {
        builder = builder.ask("command(*)", Scope::User);
    }
    let (policy, _errors) = builder.build();

    let (approver, mut approvals) = TuiApprover::new();
    let (sink, mut events) = EventSink::new(TurnId::new());

    let budget = octane_context::Budget::for_model(model.info());
    let agent = octane_core::Agent::build();
    let mut runner = TurnRunner::new(agent, policy, registry.clone(), approver, budget);
    runner.mode = mode;
    runner.events = sink;

    let cancel = runner.cancel.clone();
    let tools = registry.schemas_where(|_| true);
    let source = ModelStepSource::new(model.clone(), tools).with_thinking(thinking);

    let turn_history = history.clone();
    let workspace_owned = workspace.clone();
    let mut turn = tokio::spawn(async move {
        runner
            .run(&source, turn_history, workspace_owned.clone(), workspace_owned)
            .await
    });

    app.status_mut().activity = Some(Activity {
        label: "Thinking".into(),
        elapsed_secs: 0,
        input_tokens: 0,
        output_tokens: 0,
    });

    let outcome = loop {
        app.draw()?;

        tokio::select! {
            // Biased so events and approvals are drained before the terminal is
            // polled; otherwise a fast stream starves the repaint.
            biased;

            Some(event) = events.recv() => {
                if let octane_protocol::Event::Usage(usage) = &event {
                    if let Some(activity) = app.status_mut().activity.as_mut() {
                        activity.input_tokens = usage.input_tokens;
                        activity.output_tokens = usage.output_tokens;
                    }
                    app.status_mut().cost_usd += usage.cost;
                }
                app.push_event(&event)?;
            }

            Some((prompt, responder)) = approvals.recv() => {
                app.set_approval(prompt, responder);
            }

            finished = &mut turn => {
                break finished.map_err(|error| anyhow::anyhow!("turn panicked: {error}"))?;
            }

            // Keeps the UI alive: Esc interrupts, and the spinner animates.
            // A short timeout rather than a full tick, so polling the terminal
            // never stalls the stream this loop exists to render.
            () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                if let Some(octane_tui::AppEvent::Interrupt) =
                    app.poll_for(std::time::Duration::from_millis(1))?
                {
                    cancel.cancel();
                }
            }
        }
    };

    // Drain whatever was still queued when the turn ended.
    while let Ok(event) = events.try_recv() {
        app.push_event(&event)?;
    }

    app.status_mut().activity = None;
    history.extend(outcome.messages);

    if !outcome.stop_reason.is_success() {
        app.push_event(&completed_static(octane_protocol::ItemKind::Error {
            message: outcome.stop_reason.summary(),
        }))?;
    }

    let used = octane_context::prune::estimate_tokens(history);
    let window = model.info().effective_context_window().max(1);
    app.status_mut().context_used = used as f64 / window as f64;

    Ok(())
}
