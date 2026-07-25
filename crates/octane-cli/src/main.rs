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

            AppEvent::Picked { .. } => {}

            AppEvent::Exit => break,
            AppEvent::Interrupt => {}
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
                };
                run_turn(&mut app, &session_ctx, &mut history).await?;
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
                    other if skill_body(workspace, other).is_some() => {
                        // Tier 2: the body is read only now, on activation.
                        skill_body(workspace, other).unwrap_or_default()
                    }
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
    ("/cs", "search the codebase with parallel research agents"),
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
        let permitted = registry.schemas_where(|tool| agent.permits_tool(tool.name()));

        let (policy, _) = build_policy(&self.workspace, &self.policy_permissions);

        let mut core_agent = octane_core::Agent::build();
        core_agent.name = agent.name.clone();
        core_agent.allowed_tools =
            agent.frontmatter.tools.iter().map(|tool| tool.to_string()).collect();
        core_agent.mode = agent.frontmatter.mode_override.unwrap_or(self.mode);

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
