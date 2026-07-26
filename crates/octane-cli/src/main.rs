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
use octane_sandbox::{NetworkPolicy, SandboxPolicy};

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
    let mut sandbox = if sandboxed {
        SandboxPolicy::workspace(workspace.clone(), std::env::var("TMPDIR").ok().map(Into::into))
    } else {
        SandboxPolicy::DangerFullAccess
    };

    // Applied after construction because `workspace()` denies the network
    // unconditionally — the safe default, and the one worth having to opt out
    // of deliberately rather than passing at every call site.
    if settings.sandbox_network.unwrap_or(false) {
        if let SandboxPolicy::WorkspaceWrite { network, .. } = &mut sandbox {
            *network = NetworkPolicy::Allowed;
        }
    }

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
            settings.faster_model.as_deref(),
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
    faster: Option<&str>,
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
    // Reported but not yet used for anything: `Role::Faster` resolves, and no
    // caller asks for it, because the features that would (compaction, titles)
    // do not exist. Printed so the setting is at least verifiable rather than
    // silently inert.
    if let Some(reference) = faster {
        match registry.resolve(reference) {
            Ok(resolved) => {
                println!("faster        {} (configured, not yet used)", resolved.reference)
            }
            Err(error) => println!("faster        {reference}  — {error}"),
        }
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
    mut mode: PermissionMode,
    model: Option<&str>,
    mut settings: octane_config::Settings,
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
    // Compaction needs its own model call. `faster-model` names it when set;
    // otherwise the provider's `faster` default, resolved against the primary's
    // provider rather than the first usable one, which need not be the same.
    // Falling back to the primary is better than not compacting at all.
    let summarizer: Option<std::sync::Arc<octane_core::ModelSummarizer>> = {
        let resolved = settings
            .faster_model
            .as_deref()
            .and_then(|reference| registry.resolve(reference).ok())
            .or_else(|| {
                let provider = selected.as_ref().ok().map(|primary| primary.provider.clone());
                registry.resolve_role(provider.as_deref(), octane_provider::Role::Faster).ok()
            })
            .or_else(|| selected.as_ref().ok().cloned());

        resolved
            .and_then(|resolved| octane_provider::connect(resolved).ok())
            .map(|model| std::sync::Arc::new(octane_core::ModelSummarizer::new(model)))
    };

    // Assembled once. Skills are deliberately absent: `render_manifest` tells
    // the model to "load one with the `skill` tool", and no such tool is
    // registered — advertising it would buy failed calls.
    let assembler = {
        let mut assembler = octane_core::PromptAssembler::new(octane_core::BASE_INSTRUCTIONS)
            .sandbox(describe_sandbox(&sandbox))
            .environment(format!(
                "Working directory: {workspace}\nPlatform: {}\nThis is a snapshot taken at session start.",
                std::env::consts::OS,
            ));
        // A project's own instructions outrank nothing and inform everything, so
        // they ride as a developer message rather than being spliced into the
        // system prompt where they would break the cached prefix.
        let user_dir = octane_config::roots(workspace).first().cloned();
        if let Ok(snapshot) = octane_memory::discover(user_dir.as_deref(), workspace, workspace) {
            let rendered = snapshot.render();
            if !rendered.is_empty() {
                assembler = assembler.memory(rendered);
            }
        }
        assembler
    };

    // Snapshotted once: the registry is read at startup, so this is exactly the
    // set of models a `model` setting could name and have work.
    let model_names: Vec<String> =
        registry.models().iter().map(|model| model.reference.clone()).collect();
    let mut history: Vec<octane_protocol::Message> = Vec::new();
    let mut session_usage = SessionUsage::default();
    // Runtime override for the model's configured default.
    let mut thinking = settings.thinking.unwrap_or_else(|| {
        selected.as_ref().map(|resolved| resolved.thinking).unwrap_or_default()
    });

    if settings.show_reasoning.unwrap_or(false) {
        app.options_mut().reasoning = octane_tui::render::Reasoning::Shown;
    }
    if settings.ascii.unwrap_or(false) {
        app.options_mut().glyphs = octane_tui::glyphs::ASCII;
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

    // Commands plus every discovered skill, so `/` suggests both. Skills are
    // offered as commands rather than a separate trigger: a user reaching for a
    // capability does not care whether it was built in or dropped in a folder.
    let mut commands: Vec<Candidate> =
        COMMANDS.iter().map(|(name, detail)| Candidate::new(*name, *detail)).collect();
    commands.extend(skill_candidates(workspace));
    app.set_commands(commands);
    // Walked once at startup. A watcher would keep it live, but a stale entry
    // costs a "no such file" and a rescan costs a directory walk on every
    // keystroke — the wrong trade for a completion list.
    app.set_files(index_files(workspace));


    loop {
        app.draw()?;

        let Some(event) = app.poll()? else { continue };

        match event {
            AppEvent::Picked { kind: octane_tui::PickerKind::Provider, key } => {
                // Terminal: nothing opens from here, so the overlay closes.
                app.close_pickers();
                match connect_provider(workspace, &key) {
                    Ok(report) => {
                        app.push_event(&completed_static(ItemKind::AgentMessage {
                            text: report,
                        }))?;
                        // The registry is read at startup, so a provider added
                        // now is not usable until restart. Better said than
                        // discovered by a prompt that still reports no model.
                        app.push_event(&completed_static(ItemKind::AgentMessage {
                            text: "Restart octane to use it.".into(),
                        }))?;
                    }
                    Err(error) => {
                        app.push_event(&completed_static(ItemKind::Error {
                            message: error.to_string(),
                        }))?;
                    }
                }
            }

            // Choosing a setting opens the values it can take, rather than
            // cycling it in place: a four-value setting cycled blind takes
            // three keystrokes to inspect and one to overshoot.
            AppEvent::Picked { kind: octane_tui::PickerKind::Setting, key } => {
                // Pushed, not replaced: Esc from the values goes back to the
                // setting list with its filter and selection intact.
                match setting_value_picker(&settings, &model_names, &key) {
                    Some(picker) => app.set_picker(picker),
                    None => {
                        app.close_pickers();
                        app.push_event(&completed_static(ItemKind::Error {
                            message: format!("{key} is not an editable setting."),
                        }))?
                    }
                }
            }

            AppEvent::Picked { kind: octane_tui::PickerKind::SettingValue(setting), key: value } => {
                app.close_pickers();
                let outcome = apply_setting(
                    workspace,
                    &mut settings,
                    Live {
                        app: &mut app,
                        thinking: &mut thinking,
                        mode: &mut mode,
                        history: &mut history,
                    },
                    &model_names,
                    &setting,
                    &value,
                );
                match outcome {
                    Ok(report) => app
                        .push_event(&completed_static(ItemKind::AgentMessage { text: report }))?,
                    Err(error) => app.push_event(&completed_static(ItemKind::Error {
                        message: error.to_string(),
                    }))?,
                }
            }

            AppEvent::Picked { .. } => {}

            AppEvent::Exit => break,
            AppEvent::Interrupt => {}
            // The status line is not the permission mode. Before this, cycling
            // changed the label while the runner kept evaluating the mode it
            // started with — the dangerous direction being a user who believes
            // they are in plan mode and is not.
            AppEvent::ModeChanged(next) if next != mode => {
                octane_core::PromptAssembler::append_change_notice(
                    &mut history,
                    octane_core::mode_switch_notice(mode.label(), next.label()),
                );
                mode = next;
            }
            AppEvent::ModeChanged(_) => {}

            AppEvent::Submit(Submission::Command { name, args })
                if name == "cs" && !args.trim().is_empty() =>
            {
                app.push_event(&completed_static(ItemKind::UserMessage {
                    text: format!("/cs {args}"),
                }))?;

                let Some(model) = session.as_ref() else {
                    app.push_event(&completed_static(ItemKind::Error {
                        message: "No model is configured. Run `/connect` to set one up.".into(),
                    }))?;
                    continue;
                };

                history.push(octane_protocol::Message::text(
                    octane_protocol::Role::User,
                    codebase_search_prompt(&args),
                ));
                let session_ctx = Session {
                    model,
                    workspace,
                    sandbox: &sandbox,
                    mode,
                    thinking,
                    permissions: &settings.permissions,
                    prompt: &assembler,
                    summarizer: &summarizer,
                };
                run_turn(&mut app, &session_ctx, &mut history, &mut session_usage).await?;
            }

            AppEvent::Submit(Submission::Command { name, args }) => {
                let echoed =
                    if args.is_empty() { format!("/{name}") } else { format!("/{name} {args}") };
                app.push_event(&completed_static(ItemKind::UserMessage { text: echoed }))?;
                let body = match name.as_str() {
                    "thinking" => set_thinking(&mut app, &mut thinking, &args),
                    "help" => HELP.to_string(),
                    "models" => render_models(workspace),
                    "connect" => {
                        app.set_picker(connect_picker());
                        continue;
                    }
                    "agents" => render_agents(workspace),
                    "tools" => {
                        // Built fresh, because the turn's registry is
                        // constructed inside run_turn and is gone by now. It
                        // must include `task` or this lies by omission, so a
                        // stub delegate stands in: TaskTool stores it without
                        // calling it, and nothing here runs a turn.
                        let mut registry = octane_tools::ToolRegistry::new();
                        octane_tools::register_all(
                            &mut registry,
                            std::sync::Arc::new(octane_tools::FileTracker::new()),
                            sandbox.clone(),
                        );
                        let (agents, _) =
                            octane_config::discover_agents(&octane_config::roots(workspace));
                        registry.register(std::sync::Arc::new(octane_core::TaskTool::new(
                            agents,
                            std::sync::Arc::new(UnusedDelegate),
                        )));
                        render_tools(&registry, mode)
                    }
                    "stats" => match session.as_ref() {
                        Some(model) => render_stats(&session_usage, model.as_ref(), &history, &assembler),
                        None => "No model is configured. Run `/connect` to set one up.".into(),
                    },
                    other if skill_body(workspace, other).is_some() => {
                        // Tier 2: the body is read only now, on activation.
                        skill_body(workspace, other).unwrap_or_default()
                    }
                    "settings" => {
                        app.set_picker(settings_picker(&settings, &model_names));
                        continue;
                    }
                    "clear" => {
                        // The conversation, not just the screen. Clearing only
                        // the transcript looks identical and leaves every token
                        // still in the prompt, so a user reaching for it under
                        // context pressure gets a blank pane and the same bill.
                        history.clear();
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
                    prompt: &assembler,
                    summarizer: &summarizer,
                };
                run_turn(&mut app, &session, &mut history, &mut session_usage).await?;
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
    ("/cs", "search the codebase with parallel research agents"),
    ("/agents", "list available agents"),
    ("/tools", "list the tools the model can call this turn"),
    ("/stats", "token usage, cache hit rate, and cost"),
    ("/settings", "show resolved settings"),
    ("/clear", "start over: clear the conversation and the screen"),
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

/// The `/connect` overlay.
///
/// Providers whose credential is already present are listed first: they are the
/// ones that will work, and burying them under six that need a key first is the
/// wrong order to read in.
fn connect_picker() -> octane_tui::Picker {
    use octane_provider::connect as setup;
    use octane_tui::{PickerItem, PickerKind};

    let mut items: Vec<PickerItem> = setup::recipes()
        .into_iter()
        .map(|recipe| {
            let ready = setup::is_satisfied(&recipe, |name| std::env::var(name).ok());
            let state = match &recipe.credential {
                octane_provider::Credential::None => "ready".to_string(),
                octane_provider::Credential::TokenFile => "needs a token file".to_string(),
                octane_provider::Credential::ApiKey { env_var } => {
                    if ready {
                        format!("{env_var} is set")
                    } else {
                        format!("needs {env_var}")
                    }
                }
            };
            // Still selectable without the key: writing the file first and
            // exporting second is a perfectly reasonable order to work in.
            PickerItem::new(recipe.key, recipe.name).detail(recipe.help_url).state(state)
        })
        .collect();

    items.sort_by_key(|item| {
        let ready = item.state.as_deref().is_some_and(|state| {
            state == "ready" || state.ends_with("is set")
        });
        (!ready, item.label.clone())
    });

    octane_tui::Picker::new(PickerKind::Provider, "Connect a provider", items)
}

/// What the sandbox permits, in a sentence the model can act on.
///
/// Worth sending: a model that does not know it is confined reads a denial as a
/// broken command and "fixes" working code until the step budget runs out.
fn describe_sandbox(sandbox: &SandboxPolicy) -> String {
    match sandbox {
        SandboxPolicy::DangerFullAccess => {
            "Commands run unconfined. Nothing constrains what they can reach.".into()
        }
        SandboxPolicy::ExternalSandbox => {
            "Commands run inside an external sandbox managed by the host.".into()
        }
        SandboxPolicy::ReadOnly { network } => {
            format!("Commands run read-only; no path is writable. Network is {network:?}.")
        }
        SandboxPolicy::WorkspaceWrite { writable_roots, network } => {
            let roots: Vec<&str> = writable_roots.iter().map(|root| root.path.as_str()).collect();
            format!(
                "Commands run confined. Writable: {}. Everything else, including \
                 `.git/` and `.octane/` inside those roots, is read-only. Network is {network:?}. \
                 A denial is the sandbox, not a broken command — do not work around it.",
                roots.join(", "),
            )
        }
    }
}

/// `/settings` — which setting to change, and what it is worth now.
///
/// The rows carry the current values, so the picker is also the display the
/// old `/settings` printed. Two screens showing the same facts would drift.
fn settings_picker(settings: &octane_config::Settings, models: &[String]) -> octane_tui::Picker {
    use octane_tui::{PickerItem, PickerKind};

    let items = octane_config::edit::catalogue(models)
        .into_iter()
        .map(|editable| {
            let (value, configured) = editable.effective(settings);
            let state = if configured { value } else { format!("{value} (default)") };
            PickerItem::new(editable.key, editable.label).detail(editable.detail).state(state)
        })
        .collect();

    octane_tui::Picker::new(PickerKind::Setting, "Settings", items)
}

/// The values one setting can take, with the current one marked.
///
/// The title names the file the choice will be written to. That is the question
/// that follows immediately from changing a setting, and answering it anywhere
/// else means answering it after the write.
fn setting_value_picker(
    settings: &octane_config::Settings,
    models: &[String],
    key: &str,
) -> Option<octane_tui::Picker> {
    use octane_tui::{PickerItem, PickerKind};

    let editable = octane_config::edit::catalogue(models).into_iter().find(|e| e.key == key)?;
    let (current, configured) = editable.effective(settings);

    let items: Vec<PickerItem> = editable
        .choices
        .iter()
        .map(|choice| {
            let display = choice.value.display();
            // The radio says which is in effect; the state column only adds
            // the part a glyph cannot, which is whether it was chosen or
            // inherited.
            let chosen = display == current;
            let mut item =
                PickerItem::new(&display, &choice.label).detail(&choice.detail).radio(chosen);
            if chosen && !configured {
                item = item.state("default");
            }
            item
        })
        .collect();

    // Just the key. The trail already says "Settings", and the destination
    // path is long enough to crowd out the crumb it is appended to. The write
    // reports the file it landed in, which is when that actually matters.
    Some(octane_tui::Picker::new(
        PickerKind::SettingValue(key.to_string()),
        key.to_string(),
        items,
    ))
}

/// Write a chosen setting, then make the running session match where it can.
///
/// Writing without applying is the failure mode this exists to avoid: a change
/// that is correct in the file and invisible in the session reads as broken,
/// and the user's next move is to change it back.
/// The running session's mutable state, bundled because a setting change may
/// touch any of it and threading four `&mut`s through every call reads as an
/// accident waiting to happen.
struct Live<'a> {
    app: &'a mut octane_tui::App,
    thinking: &'a mut octane_provider::Thinking,
    mode: &'a mut PermissionMode,
    history: &'a mut Vec<octane_protocol::Message>,
}

fn apply_setting(
    workspace: &Utf8PathBuf,
    settings: &mut octane_config::Settings,
    live: Live<'_>,
    models: &[String],
    key: &str,
    value: &str,
) -> Result<String> {
    let Live { app, thinking, mode, history } = live;
    use octane_config::edit;

    let editable = edit::catalogue(models)
        .into_iter()
        .find(|editable| editable.key == key)
        .ok_or_else(|| anyhow::anyhow!("{key} is not an editable setting"))?;
    let choice = editable
        .choice(value)
        .ok_or_else(|| anyhow::anyhow!("{value} is not a value {key} can take"))?;

    let roots = octane_config::roots(workspace);
    let target = edit::target(&roots, key);
    edit::set(&target, key, &choice.value)?;

    // Reloaded rather than assigned field by field, so what the next picker
    // shows as current is what the files actually resolve to — layering,
    // overrides and all.
    let (reloaded, errors) = octane_config::Settings::load(&roots);
    *settings = reloaded;

    match key {
        "mode" => {
            let next = settings.mode.unwrap_or_default();
            if next != *mode {
                octane_core::PromptAssembler::append_change_notice(
                    history,
                    octane_core::mode_switch_notice(mode.label(), next.label()),
                );
            }
            // Both, or the label and the engine disagree about what is allowed.
            *mode = next;
            app.status_mut().mode = next;
        }
        "thinking" => *thinking = settings.thinking.unwrap_or_default(),
        "show-reasoning" => {
            app.options_mut().reasoning = if settings.show_reasoning.unwrap_or(false) {
                octane_tui::render::Reasoning::Shown
            } else {
                octane_tui::render::Reasoning::Hidden
            }
        }
        "ascii" => {
            app.options_mut().glyphs = if settings.ascii.unwrap_or(false) {
                octane_tui::glyphs::ASCII
            } else {
                octane_tui::glyphs::UNICODE
            }
        }
        // Everything else is read once at startup; the message below says so.
        _ => {}
    }

    let mut report = format!("`{key}` is now `{value}` in {target}.\n");
    if editable.applies == octane_config::Applies::OnRestart {
        report.push_str("\nRestart octane for it to take effect.\n");
    }
    for error in &errors {
        report.push_str(&format!("\n! {error}\n"));
    }
    Ok(report)
}

/// Write a provider file and say what remains.
fn connect_provider(workspace: &Utf8PathBuf, key: &str) -> Result<String> {
    use octane_provider::connect as setup;

    let recipe = setup::recipe(key)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {key:?}"))?;

    let root = workspace.join(".octane").into_std_path_buf();
    let config = setup::build_config(&recipe);
    let path = setup::write_config(&root, &recipe, &config)?;

    let mut report = format!("Wrote `{}`\n\n", path.display());
    for (name, model) in &config.models {
        report.push_str(&format!(
            "- `{}/{}` — {}\n",
            recipe.key,
            name,
            model.name.as_deref().unwrap_or("")
        ));
    }

    if let Some(variable) = recipe.credential.env_var() {
        report.push('\n');
        if setup::is_satisfied(&recipe, |name| std::env::var(name).ok()) {
            report.push_str(&format!("`{variable}` is set.\n"));
        } else {
            // Built line by line rather than with continuations: a `\` in a
            // Rust literal strips the newline but the indentation survives into
            // the rendered output as a run of spaces.
            report.push_str(&format!("Set `{variable}` to finish:\n\n"));
            report.push_str("```\n");
            report.push_str(&format!("export {variable}=...\n"));
            report.push_str("```\n\n");
            report.push_str(&format!("Get one at {}\n\n", recipe.help_url));
            report.push_str(
                "The file references the variable rather than storing the key, ",
            );
            report.push_str("so it is safe to commit.\n");
        }
    }

    if let Some(note) = recipe.note {
        report.push_str(&format!("\n> {note}\n"));
    }
    Ok(report)
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

/// Load a skill's body, if one by that name exists.
///
/// Tier 2 of progressive disclosure: read on activation, never at startup.
fn skill_body(workspace: &Utf8PathBuf, name: &str) -> Option<String> {
    let roots = octane_config::roots(workspace);
    let dirs: Vec<(camino::Utf8PathBuf, bool)> = roots
        .iter()
        .enumerate()
        .map(|(index, root)| (root.join("skills"), index + 1 == roots.len()))
        .collect();

    let (skills, _) = octane_skills::discover(&dirs);
    let skill = skills.into_iter().find(|skill| skill.name() == name)?;
    skill.load_body().ok().map(|body| body.render())
}

/// Discovered skills, offered in `/` completion.
///
/// Tier-1 only: name and description. Loading a body here would defeat
/// progressive disclosure, which is the whole reason the format is tiered
/// (`RESEARCH.md` §D).
fn skill_candidates(workspace: &Utf8PathBuf) -> Vec<octane_tui::Candidate> {
    let dirs: Vec<(camino::Utf8PathBuf, bool)> = octane_config::roots(workspace)
        .into_iter()
        .enumerate()
        .map(|(index, root)| {
            let is_project = index + 1 == octane_config::roots(workspace).len();
            (root.join("skills"), is_project)
        })
        .collect();

    let (skills, _errors) = octane_skills::discover(&dirs);
    skills
        .into_iter()
        .map(|skill| {
            let summary = skill.frontmatter.description.clone();
            // Truncated to one readable line: the popup is a menu, not the docs.
            let summary = summary.chars().take(70).collect::<String>();
            octane_tui::Candidate::new(format!("/{}", skill.name()), summary)
        })
        .collect()
}

/// `/cs <query>` — codebase search by fan-out.
///
/// Expands to a prompt rather than being a code path, which is what keeps the
/// extension model declarative (`RESEARCH.md` §F): anything octane can be told
/// to do, a user can put in their own command file.
///
/// The instruction to search several ways at once is the point. A single grep
/// finds what you already knew to look for; the interesting misses are the
/// places something is named differently, and only parallel angles catch those.
fn codebase_search_prompt(query: &str) -> String {
    format!(
        "Find everything relevant to this in the codebase, then explain it:\n\n         {query}\n\n         Use the `task` tool with the `research` agent, and launch several in one          message so they run concurrently. Give each a different angle — by symbol          name, by file or directory, by the concept described in prose, by tests          that exercise it. A single search finds only what you already knew to look          for; the useful misses are where something is named differently.\n\n         Then synthesise. Report:\n         - where the thing lives, as `path:line`\n         - how the pieces relate, not a list of what each file contains\n         - anything that looks like it should be involved but is not\n\n         If it is genuinely absent, say so plainly rather than reporting the          nearest match as though it were the answer."
    )
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
        // First sentence only. A description is written for the model choosing
        // an agent, so it carries "use when..." guidance the model needs and a
        // human scanning the list does not — and the full text is wide enough
        // that the transcript clips it mid-word anyway.
        let summary = agent
            .frontmatter
            .description
            .split_inclusive('.')
            .next()
            .unwrap_or(&agent.frontmatter.description)
            .trim();
        out.push_str(&format!(
            "  {:<10} {:<10} {summary}\n",
            agent.name,
            agent.scope.label(),
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
    let paths: Vec<_> = roots
        .iter()
        .map(|root| root.join(octane_config::settings::SETTINGS_FILE))
        .collect();
    // Padded past the longest path rather than to a guessed width: a project
    // nested a few directories deep overruns any fixed column, and the state
    // then runs into the path with no gap.
    let column = paths
        .iter()
        .map(|path| path.as_str().chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    for path in &paths {
        out.push_str(&format!(
            "  {:<column$} {}\n",
            path.as_str(),
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
/// Stands in for the real delegate when a `TaskTool` is built only to be
/// described. `TaskTool::new` stores the delegate without calling it, and
/// `/tools` never runs a turn, so this is unreachable rather than merely
/// unused.
struct UnusedDelegate;

#[async_trait::async_trait]
impl octane_core::Delegate for UnusedDelegate {
    async fn run(
        &self,
        _agent: &octane_config::AgentDefinition,
        _prompt: &str,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        Err("this TaskTool exists only to be listed".into())
    }
}

/// `/tools` - what the model can actually call this turn.
///
/// Tool availability is not fixed: plan mode omits mutating tools from the
/// schema set, and a subagent gets a further filter by name. Neither was
/// observable, so "why did it not just edit the file?" had no answer short of
/// reading the source.
///
/// Rows are indented two spaces and never begin with a markdown marker, since
/// the transcript renders what it is given.
fn render_tools(registry: &octane_tools::ToolRegistry, mode: PermissionMode) -> String {
    let mut out = String::from("Tools offered this turn\n\n");

    let names: Vec<&str> = registry.names().collect();
    let width = names.iter().map(|name| name.len()).max().unwrap_or(0).max(4) + 2;

    let mut hidden = Vec::new();
    for name in &names {
        let Some(tool) = registry.get(name) else { continue };
        if mode == PermissionMode::Plan && tool.as_ref().is_mutating() {
            hidden.push(*name);
            continue;
        }
        out.push_str(&format!("  {:<width$}offered\n", name));
    }

    if !hidden.is_empty() {
        out.push_str(&format!(
            "\nHidden by {} mode, because a denial ends the turn\n\n",
            mode.label()
        ));
        for name in hidden {
            out.push_str(&format!("  {:<width$}mutating\n", name));
        }
    }

    out
}

/// `/stats` - what the session has cost.
fn render_stats(
    usage: &SessionUsage,
    model: &dyn octane_provider::LanguageModel,
    history: &[octane_protocol::Message],
    prompt: &octane_core::PromptAssembler,
) -> String {
    let info = model.info();
    let budget = octane_context::Budget::for_model(info);
    // The assembled prompt, not the bare conversation. The turn loop measures
    // what it is about to send, which includes the system instructions, the
    // sandbox description and project memory. Reporting the conversation alone
    // understates the window by several thousand tokens on an empty session.
    let used = octane_context::prune::estimate_tokens(&prompt.assemble(history, None));

    let mut out = String::from("Session\n\n");
    out.push_str(&format!("  model            {}\n", info.display_name));
    out.push_str(&format!("  model calls      {}\n", usage.calls));
    out.push_str("\nTokens\n\n");
    out.push_str(&format!("  input            {}\n", thousands(usage.input)));
    out.push_str(&format!("  of which cached  {}\n", thousands(usage.cached_input)));
    out.push_str(&format!("  output           {}\n", thousands(usage.output)));
    out.push_str(&format!("  reasoning        {}\n", thousands(usage.reasoning)));

    out.push_str("\nContext\n\n");
    out.push_str(&format!(
        "  in use           {} of {} ({:.0}%)\n",
        thousands(used as u64),
        thousands(budget.effective_window() as u64),
        budget.utilization(used) * 100.0,
    ));

    out.push_str("\nCache\n\n");
    out.push_str(&format!("  hit rate         {:.0}%\n", usage.cache_hit_rate() * 100.0));
    out.push_str(&format!("  cost             ${:.4}\n", usage.cost));

    // Said plainly rather than left for someone to discover by comparing this
    // with their provider bill.
    out.push_str("\n  Delegated work is not counted: a subagent reports usage on\n");
    out.push_str("  its own event stream, which this session never sees.\n");
    out
}

/// Group digits. octane-tui has one of these and does not export it.
fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Running totals for `/stats`.
///
/// Subagent spend is deliberately absent: each subagent is given its own
/// `EventSink` (see `SubagentRunner::delegate`), so its usage never reaches
/// this loop. Reporting a total that silently omits delegated work would be
/// worse than reporting the primary turn's and saying so.
#[derive(Debug, Default, Clone, Copy)]
struct SessionUsage {
    input: u64,
    output: u64,
    cached_input: u64,
    reasoning: u64,
    cost: f64,
    /// Model calls, not user turns: usage is reported per step, and a turn
    /// that uses tools takes several.
    calls: u32,
}

impl SessionUsage {
    fn add(&mut self, reported: &octane_protocol::Usage) {
        self.input += reported.input_tokens;
        self.output += reported.output_tokens;
        self.cached_input += reported.cached_input_tokens;
        self.reasoning += reported.reasoning_tokens;
        self.cost += reported.cost;
        self.calls += 1;
    }

    /// Share of input tokens served from cache.
    ///
    /// Cached tokens are counted inside the input total by every codec, so
    /// this cannot exceed 1. It is the only observable signal that the
    /// cache-ordered prompt prefix is still intact: the ordering is an
    /// invariant with no other symptom when it breaks.
    fn cache_hit_rate(&self) -> f64 {
        if self.input == 0 { 0.0 } else { self.cached_input as f64 / self.input as f64 }
    }
}

/// Everything a turn needs that does not change between turns.
struct Session<'a> {
    model: &'a std::sync::Arc<dyn octane_provider::LanguageModel>,
    workspace: &'a Utf8PathBuf,
    sandbox: &'a SandboxPolicy,
    mode: PermissionMode,
    thinking: octane_provider::Thinking,
    permissions: &'a octane_config::settings::Permissions,
    /// Summarizes an old span when the context fills. Absent when no faster
    /// model resolved, in which case the turn fails at the threshold rather
    /// than pretending to compact.
    summarizer: &'a Option<std::sync::Arc<octane_core::ModelSummarizer>>,
    /// Built once for the session, not per turn. Rebuilding it would re-walk
    /// the filesystem and could change the prefix mid-session, which is the one
    /// thing the cache ordering in `PromptAssembler` exists to prevent.
    prompt: &'a octane_core::PromptAssembler,
}

async fn run_turn(
    app: &mut octane_tui::App,
    session: &Session<'_>,
    history: &mut Vec<octane_protocol::Message>,
    session_usage: &mut SessionUsage,
) -> Result<()> {
    let Session { model, workspace, sandbox, mode, thinking, permissions, prompt, summarizer } =
        *session;
    use octane_core::{EventSink, ModelStepSource, TurnRunner};
    use octane_protocol::TurnId;
    use octane_tools::ToolRegistry;
    use octane_tui::{Activity, TuiApprover};

    let tracker = std::sync::Arc::new(octane_tools::FileTracker::new());
    let mut registry = ToolRegistry::new();
    octane_tools::register_all(&mut registry, tracker, sandbox.clone());

    let (policy, _errors) = build_policy(workspace, permissions);
    let (approver, mut approvals) = TuiApprover::new();
    let (sink, mut events) = EventSink::new(TurnId::new());

    // Subagent progress rides its own channel, so their tool calls appear in the
    // transcript while their reasoning and prose do not — the latter is the
    // whole point of delegating.
    let (progress, mut subagent_progress) = tokio::sync::mpsc::unbounded_channel();

    // `task` is registered for the primary agent only. The subagent runner
    // builds its own registry without it, which is what stops recursion.
    let (agents, _agent_errors) =
        octane_config::discover_agents(&octane_config::roots(workspace));
    registry.register(std::sync::Arc::new(octane_core::TaskTool::new(
        agents,
        std::sync::Arc::new(SubagentRunner {
            model: model.clone(),
            workspace: workspace.clone(),
            sandbox: sandbox.clone(),
            policy_permissions: permissions.clone(),
            approver: approver.clone(),
            mode,
            progress,
        }),
    )));
    let registry = std::sync::Arc::new(registry);

    let budget = octane_context::Budget::for_model(model.info());
    let agent = octane_core::Agent::build();
    let mut runner = TurnRunner::new(agent, policy, registry.clone(), approver, budget);
    runner.mode = mode;
    runner.events = sink;
    // The runner is handed the assembled prompt, so the preamble is at the
    // front of what it sees and must survive compaction.
    runner.preserved_prefix = prompt.preamble_len();
    runner.summarizer = summarizer
        .as_ref()
        .map(|s| s.clone() as std::sync::Arc<dyn octane_context::compact::Summarizer>);

    let cancel = runner.cancel.clone();
    // Plan mode denies every mutating action, so showing the model `write` and
    // `bash` only buys a tool call that ends the turn. Enforced by omission for
    // the same reason the subagent path is: a tool it can see is one it tries.
    // (`is_mutating` is coarser than the policy — it also hides MCP tools plan
    // mode would permit — which errs toward showing too little, not too much.)
    let tools = registry.schemas_where(|tool| {
        mode != PermissionMode::Plan || !tool.is_mutating()
    });
    let source = ModelStepSource::new(model.clone(), tools).with_thinking(thinking);

    let turn_history = prompt.assemble(history, None);
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
                if let octane_protocol::Event::Usage(reported) = &event {
                    if let Some(activity) = app.status_mut().activity.as_mut() {
                        activity.input_tokens = reported.input_tokens;
                        activity.output_tokens = reported.output_tokens;
                    }
                    app.status_mut().cost_usd += reported.cost;
                    session_usage.add(reported);
                }

                // The activity line names what is running. It was set once to
                // "Thinking" and never reassigned, so a forty-second build read
                // as forty seconds of thinking. `Activity::label` was
                // documented and unit-tested for this and nothing produced it.
                if let octane_protocol::Event::Item(item_event) = &event {
                    let item = match item_event {
                        octane_protocol::ItemEvent::Started { item, .. }
                        | octane_protocol::ItemEvent::Completed { item, .. } => Some(item),
                        _ => None,
                    };
                    if let Some(item) = item {
                        let label = match &item.kind {
                            octane_protocol::ItemKind::ToolExecution { name, input, .. } => Some(
                                format!(
                                    "{name} {}",
                                    octane_tui::render::summarize_input(name, input)
                                ),
                            ),
                            // Back to waiting on the model once a call answers.
                            octane_protocol::ItemKind::ToolResult { .. } => {
                                Some("Thinking".to_string())
                            }
                            _ => None,
                        };
                        if let Some(label) = label {
                            if let Some(activity) = app.status_mut().activity.as_mut() {
                                activity.label = label;
                            }
                        }
                    }
                }

                app.push_event(&event)?;
            }

            Some((prompt, responder)) = approvals.recv() => {
                app.set_approval(prompt, responder);
            }

            Some(event) = subagent_progress.recv() => {
                app.push_event(&event)?;
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

    // A compacted turn hands back the reduced conversation. It arrives with
    // the assembled preamble still on the front, which the caller re-adds
    // every turn, so it is stripped here rather than duplicated forever.
    if let Some(replacement) = outcome.history_replacement {
        let preamble = prompt.preamble_len().min(replacement.len());
        *history = replacement[preamble..].to_vec();
    }
    history.extend(outcome.messages);

    if !outcome.stop_reason.is_success() {
        app.push_event(&completed_static(octane_protocol::ItemKind::Error {
            message: outcome.stop_reason.summary(),
        }))?;
    }

    let used = octane_context::prune::estimate_tokens(history);
    app.status_mut().context_used = budget.utilization(used);

    Ok(())
}

/// Runs subagents by building a real turn for each.
///
/// The subagent gets its own registry filtered to the tools its definition
/// permits, its own history, and its own event sink — the last of which is what
/// keeps its transcript out of the main context. Only the final message crosses
/// back, which is the entire point of delegation.
struct SubagentRunner {
    model: std::sync::Arc<dyn octane_provider::LanguageModel>,
    workspace: Utf8PathBuf,
    sandbox: SandboxPolicy,
    policy_permissions: octane_config::settings::Permissions,
    /// Approvals go to the same UI as the parent's, so a subagent asking for
    /// permission is not a silent hang.
    approver: std::sync::Arc<dyn octane_core::Approver>,
    mode: PermissionMode,
    /// Progress from subagents, so the UI can show what they are doing.
    progress: tokio::sync::mpsc::UnboundedSender<octane_protocol::Event>,
}

#[async_trait::async_trait]
impl octane_core::Delegate for SubagentRunner {
    async fn run(
        &self,
        agent: &octane_config::AgentDefinition,
        prompt: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        use octane_core::{EventSink, ModelStepSource, PromptAssembler, TurnRunner};
        use octane_protocol::{Message, Role, TurnId};
        use octane_tools::ToolRegistry;

        let tracker = std::sync::Arc::new(octane_tools::FileTracker::new());
        let mut registry = ToolRegistry::new();
        octane_tools::register_all(&mut registry, tracker, self.sandbox.clone());
        // Deliberately no `task` tool: subagents do not delegate further.
        let registry = std::sync::Arc::new(registry);

        // Filtered to what this agent's definition permits, and filtered by
        // *omission* — a tool the model can see is one it will try, and the
        // refusal it then reasons around is context spent on nothing.
        let (policy, _) = build_policy(&self.workspace, &self.policy_permissions);

        let mut core_agent = octane_core::Agent::build();
        core_agent.name = agent.name.clone();
        core_agent.allowed_tools =
            agent.frontmatter.tools.iter().map(|tool| tool.to_string()).collect();
        // Inherited unless the definition overrides it. Subagents must inherit
        // `accept-edits`: Antigravity found that background agents which do not
        // silently queue writes for an approval the user never sees.
        core_agent.mode = agent.frontmatter.mode_override.unwrap_or(self.mode);

        // Name filter *and* mode filter. The built-in read-only agents list
        // their tools explicitly, but a user-defined agent with
        // `mode-override: plan` and no `tools:` list would otherwise be handed
        // the mutating set it can never use.
        let permitted = registry.schemas_where(|tool| {
            agent.permits_tool(tool.name())
                && (core_agent.mode != PermissionMode::Plan || !tool.is_mutating())
        });

        let budget = octane_context::Budget::for_model(self.model.info());
        let mut runner = TurnRunner::new(
            core_agent.clone(),
            policy,
            registry,
            self.approver.clone(),
            budget,
        );
        runner.mode = core_agent.mode;
        runner.cancel = cancel;

        // Its own sink, drained here rather than shown: the subagent's tool calls
        // are surfaced as progress, but its transcript never enters the parent's
        // context.
        let (sink, mut events) = EventSink::new(TurnId::new());
        runner.events = sink;

        let progress = self.progress.clone();
        let label = agent.name.clone();
        let pump = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                // Only tool activity is worth surfacing; the subagent's prose is
                // its report, delivered at the end.
                if let octane_protocol::Event::Item(octane_protocol::ItemEvent::Completed {
                    item,
                    ..
                }) = &event
                {
                    if let octane_protocol::ItemKind::ToolExecution { name, input, .. } =
                        &item.kind
                    {
                        let _ = progress.send(octane_protocol::Event::Item(
                            octane_protocol::ItemEvent::Completed {
                                turn_id: octane_protocol::TurnId::new(),
                                item: octane_protocol::Item {
                                    id: octane_protocol::ItemId::new(),
                                    kind: octane_protocol::ItemKind::ToolExecution {
                                        call_id: octane_protocol::ToolCallId::new(),
                                        name: format!("{label}:{name}"),
                                        input: input.clone(),
                                    },
                                    status: octane_protocol::ItemStatus::Completed,
                                },
                            },
                        ));
                    }
                }
            }
        });

        let system = if agent.prompt.is_empty() {
            format!("You are the {} subagent.", agent.name)
        } else {
            agent.prompt.clone()
        };
        let assembler = PromptAssembler::new(system)
            .environment(format!("Working directory: {}", self.workspace));

        let history = assembler.assemble(&[], Some(Message::text(Role::User, prompt)));
        let source = ModelStepSource::new(self.model.clone(), permitted);

        let outcome = runner
            .run(&source, history, self.workspace.clone(), self.workspace.clone())
            .await;
        pump.abort();

        if !outcome.stop_reason.is_success() {
            return Err(outcome.stop_reason.summary());
        }

        // The final assistant message is the report. Everything before it was
        // the process, which is exactly what delegation exists to discard.
        Ok(outcome
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| message.text_content())
            .unwrap_or_default())
    }
}

/// Build the policy from configured rules, layered on the baseline.
fn build_policy(
    workspace: &Utf8PathBuf,
    permissions: &octane_config::settings::Permissions,
) -> (octane_permission::Policy, Vec<octane_permission::PermissionError>) {
    use octane_permission::{Policy, Scope};

    let mut builder =
        Policy::builder().workspace_root(workspace.as_str()).with_baseline_denies();
    for rule in &permissions.deny {
        builder = builder.deny(rule, Scope::Project);
    }
    for rule in &permissions.ask {
        builder = builder.ask(rule, Scope::Project);
    }
    for rule in &permissions.allow {
        builder = builder.allow(rule, Scope::Project);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> octane_tools::ToolRegistry {
        let mut registry = octane_tools::ToolRegistry::new();
        octane_tools::register_all(
            &mut registry,
            std::sync::Arc::new(octane_tools::FileTracker::new()),
            octane_sandbox::SandboxPolicy::DangerFullAccess,
        );
        registry
    }

    /// Spot-checks the exact split rather than comparing two derived sets,
    /// which would hold even if `is_mutating` inverted and both columns moved
    /// together.
    #[test]
    fn plan_mode_hides_exactly_the_mutating_tools() {
        let listing = render_tools(&registry(), PermissionMode::Plan);
        let (offered, hidden) = listing.split_once("Hidden by").expect("a hidden section");

        for read_only in ["read", "glob", "grep", "list"] {
            assert!(offered.contains(read_only), "{read_only} must stay offered in plan mode");
        }
        for mutating in ["write", "edit", "bash"] {
            assert!(hidden.contains(mutating), "{mutating} must be hidden in plan mode");
            assert!(!offered.contains(mutating), "{mutating} must not also be offered");
        }
    }

    #[test]
    fn every_tool_is_offered_outside_plan_mode() {
        let listing = render_tools(&registry(), PermissionMode::Default);
        assert!(!listing.contains("Hidden by"), "nothing is withheld by default");
        for name in ["read", "write", "edit", "bash", "glob", "grep", "list"] {
            assert!(listing.contains(name), "{name} missing");
        }
    }

    /// The transcript renders markdown, so a listing that starts a line with a
    /// marker turns into a list, a heading, or a horizontal rule.
    #[test]
    fn no_listing_line_is_read_as_markdown() {
        let mut listings = vec![
            render_tools(&registry(), PermissionMode::Plan),
            render_tools(&registry(), PermissionMode::Default),
        ];
        listings.push(render_agents(&Utf8PathBuf::from(".")));

        for listing in listings {
            for line in listing.lines() {
                let trimmed = line.trim_start();
                for marker in ["- ", "* ", "+ ", "> ", "# ", "---", "***", "___", "```", "~~~"] {
                    assert!(
                        !trimmed.starts_with(marker),
                        "line would render as markdown: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_cache_hit_rate_cannot_exceed_one() {
        // Cached tokens are counted inside the input total by every codec, so a
        // rate above 1 would mean the accounting drifted rather than the cache
        // performing impossibly well.
        let mut usage = SessionUsage::default();
        usage.add(&octane_protocol::Usage {
            input_tokens: 1_000,
            output_tokens: 10,
            cached_input_tokens: 900,
            reasoning_tokens: 0,
            cost: 0.0,
        });
        assert!((usage.cache_hit_rate() - 0.9).abs() < f64::EPSILON);

        assert_eq!(SessionUsage::default().cache_hit_rate(), 0.0, "no divide by zero");
    }
}
