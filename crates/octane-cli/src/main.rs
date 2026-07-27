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

    /// Resume a recorded session.
    ///
    /// A session is recorded as it happens, so one that ended in a crash or a
    /// closed terminal is still resumable up to its last complete turn.
    Resume {
        /// Session id, or enough of its start to be unambiguous. Omit to pick
        /// one from a list.
        id: Option<String>,
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
        Some(Command::Models) => {
            print!("{}", render_models(&workspace));
            Ok(())
        }
        Some(Command::Settings) => {
            print!("{}", render_settings(&workspace));
            Ok(())
        }
        Some(Command::Connect { provider, user }) => connect(&workspace, provider.as_deref(), user),
        Some(Command::Resume { id }) => {
            let start = match id {
                Some(id) => {
                    let found = octane_session::find(&sessions_dir(&workspace), &id)?;
                    Start::With(octane_session::read(&found.path)?.1)
                }
                None => {
                    let sessions = octane_session::list(&sessions_dir(&workspace));
                    // A sentence, not an empty overlay: there is nothing to
                    // filter, scroll, or choose, and taking the screen to say so
                    // is worse than saying it here.
                    if sessions.is_empty() {
                        println!("No recorded sessions yet.");
                        return Ok(());
                    }
                    Start::Pick(sessions)
                }
            };
            let model = cli.model.clone().or_else(|| settings.model.clone());
            tokio::runtime::Runtime::new()?.block_on(interactive(
                &workspace,
                sandbox,
                mode,
                model.as_deref(),
                settings,
                settings_errors,
                start,
            ))
        }
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
                Start::With(Vec::new()),
            ))
        }
    }
}

/// What the conversation starts from.
///
/// `Pick` is not a `Vec<Message>` yet because it cannot be: choosing needs the
/// screen, and the screen does not exist until `interactive` takes it. Carrying
/// the already-read listing rather than a flag keeps the directory walked once
/// and keeps the "nothing to resume" sentence on the normal terminal, where it
/// belongs.
enum Start {
    /// Prior messages, empty for a fresh session.
    With(Vec<octane_protocol::Message>),
    Pick(Vec<octane_session::SessionSummary>),
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
    // Printed so the setting is verifiable before a session needs it: this is
    // the model compaction summarises with, and a typo here surfaces as a turn
    // that fails at the context threshold rather than as a bad config line.
    if let Some(reference) = faster {
        match registry.resolve(reference) {
            Ok(resolved) => {
                println!("faster        {} (compaction)", resolved.reference)
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
    println!("mcp servers");
    let (servers, errors) =
        octane_mcp::config::discover(&octane_config::roots(workspace), |name| {
            std::env::var(name).ok()
        });
    if servers.is_empty() && errors.is_empty() {
        println!("  <none configured>          .octane/mcp.json");
    }
    for (name, server) in &servers {
        // Not started here: doctor reports configuration, and spawning a child
        // process to answer "what is configured" would make a diagnostic command
        // the thing that needs diagnosing.
        let state = if server.enabled { "" } else { "  (disabled)" };
        // A declaration names a command or a url, never both, so whichever is
        // set is what the server actually is.
        let target = server.command.as_deref().or(server.url.as_deref()).unwrap_or("");
        println!("  {name:<26} {target} {}{state}", server.args.join(" "));
    }
    for error in &errors {
        println!("  ! {error}");
    }

    println!();
    println!("memory files searched, in load order");
    for name in octane_memory::MEMORY_FILENAMES {
        println!("  {name}");
    }
    println!("  {}", octane_memory::LOCAL_MEMORY_FILENAME);

    Ok(())
}

/// A one-off tool invocation outside any turn: `octane tool`, an `@path`
/// attachment, a `!command`. The turn runner builds its own context per call,
/// so this is only the standalone path — spelled once because three identical
/// copies of it drift one field at a time.
fn tool_context(workspace: &Utf8PathBuf) -> octane_tools::ToolContext {
    octane_tools::ToolContext {
        session_id: octane_protocol::SessionId::new(),
        call_id: octane_protocol::ToolCallId::new(),
        agent: "build".into(),
        workspace: workspace.clone(),
        cwd: workspace.clone(),
        cancel: Default::default(),
    }
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
    use octane_tools::ToolRegistry;

    let tracker = std::sync::Arc::new(octane_tools::FileTracker::new());
    let mut registry = ToolRegistry::new();
    octane_tools::register_all(&mut registry, tracker, sandbox);

    let tool = registry.get(name).ok_or_else(|| {
        anyhow::anyhow!(
            "no tool named {name:?}. Available: {}",
            registry.names().collect::<Vec<_>>().join(", ")
        )
    })?;

    let ctx = tool_context(workspace);

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
/// Owns everything a turn needs but cannot rebuild: the model connection, the
/// assembled prompt, and the mutable session state a `/settings` change may
/// touch. Each submission hands those to [`run_turn`]; a session without a
/// resolved model still runs, so `/connect` is reachable from inside it.
async fn interactive(
    workspace: &Utf8PathBuf,
    sandbox: SandboxPolicy,
    mut mode: PermissionMode,
    model: Option<&str>,
    mut settings: octane_config::Settings,
    settings_errors: Vec<octane_config::SettingsError>,
    start: Start,
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
        StatusLine { mode, model: model_label.clone(), ..Default::default() },
        workspace.to_string(),
        contained,
    )?;

    // Built once. A failure here is reported and the session continues without
    // a model, so `/connect` still works.
    let session: Option<std::sync::Arc<dyn octane_provider::LanguageModel>> = match &selected {
        Ok(resolved) => match octane_provider::connect(resolved.clone()) {
            Ok(model) => Some(model),
            Err(error) => {
                app.report_error(format!("{}: {error}", resolved.reference))?;
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

    let mut recording = Recording {
        dir: sessions_dir(workspace),
        meta: octane_session::SessionMeta {
            id: octane_protocol::SessionId::new(),
            timestamp: jiff::Timestamp::now().to_string(),
            cwd: workspace.clone(),
            model: Some(model_label.clone()),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        recorder: None,
    };

    // Connected before the prompt is assembled, because a server's instructions
    // belong in the cached prefix. Discovering them later would mean either a
    // stale prompt or a rewritten one, and rewriting the prefix costs a full
    // cache miss on every subsequent turn.
    let (mcp, mcp_errors) = Mcp::connect(workspace).await;

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
        assembler.mcp_instructions(&mcp.instructions)
    };

    // Snapshotted once: the registry is read at startup, so this is exactly the
    // set of models a `model` setting could name and have work.
    let model_names: Vec<String> =
        registry.models().iter().map(|model| model.reference.clone()).collect();
    // `Pick` carries no messages yet: they arrive when a row is chosen, several
    // hundred lines below in the loop. The listing is held rather than turned
    // into a picker here because the glyph set is not settled until the settings
    // below have been applied, and a picker built with the wrong one would show
    // an ellipsis the terminal cannot draw.
    let (mut history, choices) = match start {
        Start::With(messages) => (messages, Vec::new()),
        Start::Pick(sessions) => (Vec::new(), sessions),
    };
    let resuming = !history.is_empty();
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
    // Discovered here rather than in `octane-config`: a palette is a UI type, and
    // config does not depend on the UI.
    let (custom_themes, mut theme_errors) = discover_themes(workspace);
    let mut theme_names = theme_choices(&custom_themes);
    if let Some(name) = settings.theme.as_deref() {
        match resolve_theme(name, &custom_themes, octane_tui::ColorDepth::detect()) {
            Some(theme) => app.options_mut().theme = theme,
            // Said rather than silently defaulted: a typo that quietly does
            // nothing is the reason `Theme::named` refuses to guess.
            None => theme_errors.push(format!(
                "`theme = \"{name}\"` names no built-in or discovered theme; using the default."
            )),
        }
    }
    // Opened here rather than resolved before the call, because a picker is a
    // screen and this is the first moment there is one. Esc closes it and leaves
    // a fresh session, which is the only way out: a cancelled picker reports
    // nothing back, so there is no cancellation for this to act on.
    // Configuration problems are said once, at the top, rather than surfacing as
    // a confusing failure on the first prompt.
    for error in &settings_errors {
        app.report_error(error.to_string())?;
    }
    for error in registry.errors() {
        app.report_error(error.to_string())?;
    }
    for error in &mcp_errors {
        app.report_error(error)?;
    }
    for error in &theme_errors {
        app.report_error(error)?;
    }

    // After the errors above, not before: a picker is modal and drawn over the
    // transcript, so opening it first hides the very problems the user needs to
    // see before choosing.
    if !choices.is_empty() {
        let ellipsis = app.options().glyphs.ellipsis;
        app.set_picker(session_picker(choices, ellipsis));
    }
    if resuming {
        app.say(resumed_notice(history.len()))?;
    }
    if let Err(error) = &selected {
        app.report_error(format!("{error}. Run `octane models` to see what is configured."))?;
    }

    // Everything `/` offers, from one registry: this client's built-ins and
    // every discovered command file.
    let (commands, command_errors) = command_registry(workspace);
    for error in &command_errors {
        app.report_error(error.to_string())?;
    }
    let mut candidates: Vec<Candidate> = commands
        .iter()
        .map(|entry| Candidate::new(format!("/{}", entry.name), entry.detail()))
        .collect();
    // Skills are offered as commands rather than behind a separate trigger: a
    // user reaching for a capability does not care whether it was built in,
    // dropped in `commands/`, or dropped in `skills/`. A skill whose name the
    // registry already claims is left out rather than listed twice.
    candidates.extend(
        skill_candidates(workspace)
            .into_iter()
            .filter(|candidate| !commands.contains(candidate.value.trim_start_matches('/'))),
    );
    app.set_commands(candidates);
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
                        app.say(report)?;
                        // The registry is read at startup, so a provider added
                        // now is not usable until restart. Better said than
                        // discovered by a prompt that still reports no model.
                        app.say("Restart octane to use it.")?;
                    }
                    Err(error) => {
                        app.report_error(error.to_string())?;
                    }
                }
            }

            // Chosen from the overlay `octane resume` opened at startup, so the
            // history is still empty and nothing is being discarded here.
            AppEvent::Picked { kind: octane_tui::PickerKind::Session, key } => {
                app.close_pickers();
                // Through `find` rather than by carrying the path in the row:
                // one lookup, so a resume by id and a resume by pick cannot
                // disagree about which file an id names.
                match octane_session::find(&sessions_dir(workspace), &key)
                    .and_then(|found| octane_session::read(&found.path))
                {
                    Ok((_, messages)) => {
                        history = messages;
                        app.say(resumed_notice(history.len()))?;
                    }
                    Err(error) => app.report_error(error.to_string())?,
                }
            }

            // Choosing a setting opens the values it can take, rather than
            // cycling it in place: a four-value setting cycled blind takes
            // three keystrokes to inspect and one to overshoot.
            AppEvent::Picked { kind: octane_tui::PickerKind::Setting, key } => {
                // Pushed, not replaced: Esc from the values goes back to the
                // setting list with its filter and selection intact.
                let ellipsis = app.options().glyphs.ellipsis;
                match setting_value_picker(&settings, &model_names, &theme_names, ellipsis, &key) {
                    Some(picker) => app.set_picker(picker),
                    None => {
                        app.close_pickers();
                        app.report_error(format!("{key} is not an editable setting."))?
                    }
                }
            }

            AppEvent::Picked { kind: octane_tui::PickerKind::SettingValue(setting), key: value } => {
                // The builder's row is a door, not a value: it opens the list of
                // themes to copy rather than setting anything.
                if setting == "theme" && value == NEW_THEME {
                    app.set_picker(theme_base_picker());
                    continue;
                }
                app.close_pickers();

                // Once the copy exists on disk it is an ordinary named theme, so
                // the builder ends in exactly the same place picking a built-in
                // does — one write, one live application, one report.
                let (setting, value) = if setting == THEME_BASE {
                    match create_custom_theme(workspace, &value) {
                        Ok((name, note)) => {
                            app.say(note)?;
                            // Errors are not re-reported: they were said at
                            // startup and nothing here can have introduced one.
                            theme_names = theme_choices(&discover_themes(workspace).0);
                            ("theme".to_string(), name)
                        }
                        Err(error) => {
                            app.report_error(error.to_string())?;
                            continue;
                        }
                    }
                } else {
                    (setting, value)
                };

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
                    &theme_names,
                    &setting,
                    &value,
                );
                match outcome {
                    Ok(report) => app.say(report)?,
                    Err(error) => app.report_error(error.to_string())?,
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
                app.note(ItemKind::UserMessage {
                    text: format!("/cs {args}"),
                })?;

                let Some(model) = session.as_ref() else {
                    app.report_error("No model is configured. Run `/connect` to set one up.")?;
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
                    mcp_tools: &mcp.tools,
                };
                run_turn(&mut app, &session_ctx, &mut history, &mut session_usage).await?;
                    recording.record(&mut app, &history)?;
            }

            AppEvent::Submit(Submission::Command { name, args }) => {
                let echoed =
                    if args.is_empty() { format!("/{name}") } else { format!("/{name} {args}") };
                app.note(ItemKind::UserMessage { text: echoed })?;
                // A command file expands to a prompt and runs a turn; a
                // built-in runs code. The registry decides which, and a
                // built-in always wins.
                if let Some(octane_commands::Kind::Prompt(command)) =
                    commands.get(&name).map(|entry| &entry.kind)
                {
                    let expanded = expand_command(
                        &mut app,
                        command,
                        &args,
                        workspace,
                        &sandbox,
                        &settings.permissions,
                        mode,
                    )
                    .await;
                    let prompt = match expanded {
                        Ok(prompt) => prompt,
                        Err(error) => {
                            app.report_error(error.to_string())?;
                            continue;
                        }
                    };
                    let Some(model) = session.as_ref() else {
                        app.report_error("No model is configured. Run `/connect` to set one up.")?;
                        continue;
                    };
                    history.push(octane_protocol::Message::text(
                        octane_protocol::Role::User,
                        prompt,
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
                        mcp_tools: &mcp.tools,
                    };
                    run_turn(&mut app, &session_ctx, &mut history, &mut session_usage).await?;
                    recording.record(&mut app, &history)?;
                    continue;
                }

                let body = match name.as_str() {
                    "thinking" => set_thinking(&mut app, &mut thinking, &args),
                    "help" => HELP.to_string(),
                    "models" => render_models(workspace),
                    "connect" => {
                        app.set_picker(connect_picker());
                        continue;
                    }
                    "agents" => render_agents(workspace),
                    "mcp" => render_mcp(workspace, &mcp),
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
                        // MCP tools too, for the same reason: this listing
                        // claims to be what the model can call this turn, and a
                        // connected server's tools are exactly that.
                        for tool in &mcp.tools {
                            registry.register(tool.clone());
                        }
                        render_tools(&registry, mode)
                    }
                    "stats" => match session.as_ref() {
                        Some(model) => render_stats(&session_usage, model.as_ref(), &history, &assembler),
                        None => "No model is configured. Run `/connect` to set one up.".into(),
                    },
                    "settings" => {
                        app.set_picker(settings_picker(&settings, &model_names, &theme_names));
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
                    // Tier 2 of progressive disclosure: a skill's body is read
                    // only now, on activation. Last, so a skill dropped in a
                    // cloned repository cannot take `/clear` or `/exit`.
                    other => skill_body(workspace, other)
                        .unwrap_or_else(|| format!("Unknown command /{other}. Try /help.")),
                };
                app.say(body)?;
            }

            AppEvent::Submit(Submission::Shell { command }) => {
                // No separate echo of the typed text: the call line below names
                // the command and carries a status marker, so echoing it first
                // put the same string on two adjacent rows. The `!` was an
                // input affordance, not a fact worth a row of transcript.
                //
                // One id for both halves, so the result groups under the call
                // that produced it.
                let call_id = ToolCallId::new();
                app.note(ItemKind::ToolExecution {
                    call_id: call_id.clone(),
                    name: "bash".into(),
                    input: shell_input(&command),
                })?;
                let (output, metadata, failed) = run_shell(&command, workspace, &sandbox).await;
                // A tool result, not an agent message. It was the latter, which
                // ran the shell's output through the markdown renderer and
                // attributed it to the model — so a `!ls` listing containing a
                // `*` came back styled, and output the agent never saw looked
                // like something it had said.
                app.note(ItemKind::ToolResult {
                    call_id,
                    name: "bash".into(),
                    title: command.clone(),
                    metadata,
                    body: output,
                    is_error: failed,
                })?;
            }

            AppEvent::Submit(Submission::Prompt { text, file_references }) => {
                app.note(ItemKind::UserMessage { text: text.clone() })?;

                // `@path` attaches the file's contents through the `read` tool,
                // so a reference gets the same limits a model-issued read does.
                let mut prompt = text.clone();
                for reference in &file_references {
                    match attach(reference, workspace).await {
                        Ok(attached) => {
                            app.note(ItemKind::ToolExecution {
                                call_id: ToolCallId::new(),
                                name: "read".into(),
                                input: serde_json::json!({ "path": reference }).to_string(),
                            })?;
                            prompt.push_str("\n\n");
                            prompt.push_str(&attached);
                        }
                        Err(error) => {
                            app.report_error(format!("@{reference}: {error}"))?;
                        }
                    }
                }

                let Some(model) = session.as_ref() else {
                    app.report_error("No model is configured. Run `/connect` to set one up.")?;
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
                    mcp_tools: &mcp.tools,
                };
                run_turn(&mut app, &session, &mut history, &mut session_usage).await?;
                recording.record(&mut app, &history)?;
            }
        }
    }

    mcp.shutdown().await;
    app.restore()?;
    Ok(())
}

/// Slash commands offered by completion.
/// The commands this client implements in code.
///
/// Handed to [`octane_commands::Registry`] rather than listed straight into the
/// completion menu: the menu is the registry's job now, and registering these
/// there is also what stops a `commands/clear.md` in a cloned repository from
/// taking the name.
const BUILTINS: &[octane_commands::Builtin] = &[
    octane_commands::Builtin { name: "help", description: "show the key reference" },
    octane_commands::Builtin { name: "models", description: "list configured models" },
    octane_commands::Builtin { name: "connect", description: "set up a provider" },
    octane_commands::Builtin {
        name: "thinking",
        description: "show or set reasoning: off, low, medium, high",
    },
    octane_commands::Builtin {
        name: "cs",
        description: "search the codebase with parallel research agents",
    },
    octane_commands::Builtin { name: "agents", description: "list available agents" },
    octane_commands::Builtin {
        name: "mcp",
        description: "list MCP servers, their status, and their tools",
    },
    octane_commands::Builtin {
        name: "tools",
        description: "list the tools the model can call this turn",
    },
    octane_commands::Builtin { name: "stats", description: "token usage, cache hit rate, and cost" },
    octane_commands::Builtin { name: "settings", description: "show resolved settings" },
    octane_commands::Builtin {
        name: "clear",
        description: "start over: clear the conversation and the screen",
    },
    octane_commands::Builtin { name: "exit", description: "quit octane" },
];

/// Config subdirectories, least specific first, so the project's wins.
fn config_dirs(workspace: &Utf8PathBuf, sub: &str) -> Vec<camino::Utf8PathBuf> {
    octane_config::roots(workspace).into_iter().map(|root| root.join(sub)).collect()
}

/// The slash-command namespace: this client's built-ins plus every discovered
/// `.octane/commands/*.md`.
fn command_registry(
    workspace: &Utf8PathBuf,
) -> (octane_commands::Registry, Vec<octane_commands::CommandError>) {
    let roots = config_dirs(workspace, octane_commands::COMMANDS_DIR);
    let last = roots.len();
    let dirs: Vec<(camino::Utf8PathBuf, bool)> =
        roots.into_iter().enumerate().map(|(i, dir)| (dir, i + 1 == last)).collect();
    octane_commands::Registry::load(BUILTINS, &dirs)
}

/// Connected MCP servers, and what they contribute to a session.
///
/// Built once at startup, not per turn: each server is a child process, and
/// respawning one for every prompt would pay initialization on every turn and
/// discard whatever state the server holds.
struct Mcp {
    /// Ready to register into a turn's [`octane_tools::ToolRegistry`], where they
    /// are indistinguishable from built-ins — same registry, same policy.
    tools: Vec<std::sync::Arc<dyn octane_tools::Tool>>,
    clients: Vec<std::sync::Arc<octane_mcp::McpClient>>,
    /// Server guidance, already fenced and attributed as third-party.
    instructions: String,
}

impl Mcp {
    /// Spawn every enabled server, negotiate, and adopt its tools.
    ///
    /// A server that fails to start or refuses the handshake is reported and
    /// skipped. One broken entry in a cloned repository's `mcp.json` must not be
    /// the reason a session will not open.
    async fn connect(workspace: &Utf8PathBuf) -> (Self, Vec<String>) {
        let roots = octane_config::roots(workspace);
        let (servers, config_errors) =
            octane_mcp::config::discover(&roots, |name| std::env::var(name).ok());

        let mut mcp =
            Self { tools: Vec::new(), clients: Vec::new(), instructions: String::new() };
        let mut errors: Vec<String> = config_errors.iter().map(ToString::to_string).collect();

        for (name, server) in servers {
            if !server.enabled {
                continue;
            }

            // `connect` owns the stdio/HTTP choice, so this site cannot forget
            // `url` and silently drop every remote server.
            let transport = match server.connect(&name).await {
                Ok(transport) => transport,
                Err(error) => {
                    errors.push(format!("mcp {name}: {error}"));
                    continue;
                }
            };

            let client = std::sync::Arc::new(octane_mcp::McpClient::new(&name, transport));
            if let Err(error) = client.initialize().await {
                errors.push(format!("mcp {name}: {error}"));
                // Started but unusable, so the child is reaped rather than left
                // holding a pipe nobody reads.
                let _ = client.shutdown().await;
                continue;
            }

            match client.list_tools().await {
                Ok(descriptors) => {
                    for descriptor in descriptors {
                        mcp.tools.push(std::sync::Arc::new(octane_mcp::McpTool::new(
                            client.clone(),
                            descriptor,
                        )));
                    }
                }
                Err(error) => errors.push(format!("mcp {name}: {error}")),
            }

            if let Some(rendered) = client.rendered_instructions().await {
                if !mcp.instructions.is_empty() {
                    mcp.instructions.push_str("\n\n");
                }
                mcp.instructions.push_str(&rendered);
            }
            mcp.clients.push(client);
        }

        (mcp, errors)
    }

    /// Close every server's stdin and reap it.
    async fn shutdown(&self) {
        for client in &self.clients {
            let _ = client.shutdown().await;
        }
    }
}

/// Append the turn to the session file, reporting a failure without ending the
/// session — the conversation is worth more than its record.
/// A session file, created on the first turn rather than at startup.
///
/// Lazily, because `octane resume` with no id opens a picker *before* any
/// session exists. Creating the file eagerly wrote an empty one every time
/// someone merely browsed — and those strays then sorted to the top of the very
/// list the picker shows, so looking at the list corrupted it.
///
/// It still opens before the first turn completes, so a session that dies to a
/// panic keeps whatever it managed.
struct Recording {
    dir: camino::Utf8PathBuf,
    meta: octane_session::SessionMeta,
    recorder: Option<octane_session::Recorder>,
}

impl Recording {
    /// Append the turn. A failure is reported and the session continues: losing
    /// the transcript is bad, refusing to work because it cannot be written is
    /// worse.
    fn record(
        &mut self,
        app: &mut octane_tui::App,
        history: &[octane_protocol::Message],
    ) -> Result<()> {
        if self.recorder.is_none() {
            match octane_session::Recorder::create(&self.dir, &self.meta) {
                Ok(recorder) => self.recorder = Some(recorder),
                Err(error) => {
                    app.report_error(format!("not recording this session: {error}"))?;
                    // Left as `None`; the next turn tries again, which is what
                    // recovers a session started before its directory existed.
                    return Ok(());
                }
            }
        }
        if let Some(recorder) = self.recorder.as_mut()
            && let Err(error) = recorder.record(history)
        {
            app.report_error(format!("could not record the session: {error}"))?;
        }
        Ok(())
    }
}

/// Where recorded sessions live.
///
/// The user root, not the project's: a session is a conversation the user had,
/// and scoping it to a directory means losing it the moment they work somewhere
/// else. The recorded `cwd` says which tree it belonged to.
fn sessions_dir(workspace: &Utf8PathBuf) -> camino::Utf8PathBuf {
    octane_config::roots(workspace)
        .first()
        .cloned()
        .unwrap_or_else(|| workspace.join(".octane"))
        .join(octane_session::SESSIONS_DIR)
}

/// Said on resuming, from either route.
///
/// One sentence, spelled once: the transcript starts empty even though the model
/// has the prior conversation, and that mismatch is confusing unsaid.
fn resumed_notice(count: usize) -> String {
    format!(
        "Resumed with {count} earlier message(s). The transcript starts fresh; \
         the model still has them."
    )
}

/// `octane resume` with no id: which conversation to continue.
///
/// The rows come from `octane_session::list`, which is already newest first, and
/// that order is the whole point — the session someone means is almost always
/// the last one. The preview rides in the label rather than in the detail, so
/// typing filters on what was actually asked; a timestamp is not something
/// anyone can search for from memory. The full id is the key, because that is
/// what `octane resume <id>` and `find` take.
///
/// Both fields are cut to a column budget rather than left whole. The overlay is
/// a fixed 66 columns and the picker pads every label to the longest one, so a
/// single long preview does not wrap — it pushes the message count off the right
/// edge of the box, where it is simply gone. The numbers below leave room for
/// the marker, the padding, and a four-figure count; widening the overlay would
/// only make them conservative, never wrong.
fn session_picker(
    sessions: Vec<octane_session::SessionSummary>,
    ellipsis: &str,
) -> octane_tui::Picker {
    use octane_tui::{PickerItem, PickerKind};

    let items = sessions
        .into_iter()
        .map(|session| {
            // To the minute. The seconds and nanoseconds of an RFC 3339 stamp
            // distinguish nothing a human is choosing between.
            let stamp: String =
                session.meta.timestamp.chars().take(16).collect::<String>().replace('T', " ");
            // Through `truncate` rather than `chars().take`, which counts
            // characters: a CJK preview is one char per two cells, so cutting by
            // character is how a row overruns the box it was measured into.
            let preview = match &session.preview {
                Some(text) => octane_tui::render::truncate(text, 34, ellipsis),
                None => "(nothing was asked)".to_string(),
            };
            // The id is here rather than in the label because nobody scans for
            // one, and the cwd because resuming into a different tree is usually
            // a mistake and this is the only thing that says so.
            let short: String = session.meta.id.as_str().chars().take(12).collect();
            PickerItem::new(session.meta.id.as_str(), format!("{stamp}  {preview}"))
                .state(format!("{} msg", session.messages))
                .detail(format!(
                    "{short}  {}",
                    octane_tui::render::truncate(session.meta.cwd.as_str(), 44, ellipsis)
                ))
        })
        .collect();

    octane_tui::Picker::new(PickerKind::Session, "Resume a session", items)
}

/// Providers discovered under the same config roots everything else uses.
///
/// The roots come from `octane_config` rather than being spelled out again here:
/// a second copy of "where config lives" is a rule that can disagree with itself.
fn registry(workspace: &Utf8PathBuf) -> octane_provider::Registry {
    let roots: Vec<std::path::PathBuf> =
        octane_config::roots(workspace).into_iter().map(Utf8PathBuf::into_std_path_buf).collect();
    octane_provider::Registry::load(&roots)
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

/// How a provider's credential stands, and whether it is satisfied.
///
/// Said by both the `/connect` overlay and the plain-text listing, and the
/// overlay sorts on it — which it used to do by parsing the sentence back out
/// of the string it had just built. The flag is the fact; the string is display.
fn credential_state(recipe: &octane_provider::connect::Recipe) -> (String, bool) {
    use octane_provider::Credential;

    let ready =
        octane_provider::connect::is_satisfied(recipe, |name| std::env::var(name).ok());
    let state = match &recipe.credential {
        Credential::None => "ready".to_string(),
        Credential::TokenFile => "needs a token file".to_string(),
        Credential::ApiKey { env_var } if ready => format!("{env_var} is set"),
        Credential::ApiKey { env_var } => format!("needs {env_var}"),
    };
    (state, ready)
}

/// The `/connect` overlay.
///
/// Providers whose credential is already present are listed first: they are the
/// ones that will work, and burying them under six that need a key first is the
/// wrong order to read in.
fn connect_picker() -> octane_tui::Picker {
    use octane_provider::connect as setup;
    use octane_tui::{PickerItem, PickerKind};

    let mut items: Vec<(bool, PickerItem)> = setup::recipes()
        .into_iter()
        .map(|recipe| {
            let (state, ready) = credential_state(&recipe);
            // Still selectable without the key: writing the file first and
            // exporting second is a perfectly reasonable order to work in.
            (ready, PickerItem::new(recipe.key, recipe.name).detail(recipe.help_url).state(state))
        })
        .collect();

    items.sort_by_key(|(ready, item)| (!ready, item.label.clone()));
    let items: Vec<PickerItem> = items.into_iter().map(|(_, item)| item).collect();

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

/// Where a custom colour scheme lives under a config root, one palette per file.
const THEMES_DIR: &str = "themes";

/// The theme picker's one row that is not a theme.
///
/// A sentinel rather than a name like `custom`, which a discovered file could
/// legitimately be called; `*` is not a plausible file stem, so the builder's
/// row and a real theme can never be confused for one another.
const NEW_THEME: &str = "*new";

/// The picker that asks which theme the new one starts from. Not a setting key —
/// nothing writes `theme-base` to a file — so it is intercepted before
/// [`apply_setting`], which only knows about real settings.
const THEME_BASE: &str = "theme-base";

/// The semantic roles a palette file can set, spelled as the deserializer wants
/// them. Written out because [`octane_tui::theme::Palette`] deserializes but does
/// not serialize, and the builder has to emit the file.
const ROLES: [&str; 15] = [
    "surface", "onAccent", "user", "assistant", "reasoning", "tool", "success", "error",
    "warning", "dim", "accent", "code", "rail", "added", "removed",
];

/// Custom colour schemes: `<root>/themes/*.json`, one [`Palette`] per file,
/// named by its stem.
///
/// Same layering and the same shape as `providers/*.json`, for the same reason —
/// a second idea of where config lives is a rule that can disagree with itself.
/// A malformed file is reported and skipped, never fatal.
///
/// [`Palette`]: octane_tui::theme::Palette
fn discover_themes(
    workspace: &Utf8PathBuf,
) -> (std::collections::BTreeMap<String, octane_tui::theme::Palette>, Vec<String>) {
    let mut themes = std::collections::BTreeMap::new();
    let mut errors = Vec::new();

    for root in octane_config::roots(workspace) {
        // A missing directory is the normal case, not a problem.
        let Ok(entries) = std::fs::read_dir(root.join(THEMES_DIR)) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else { continue };

            // Built-ins win, as the command registry's built-ins do: a file
            // dropped in by a cloned repository must not be able to redefine
            // what `nord` means. Reported rather than dropped silently, so the
            // author of the file learns why it did nothing.
            if octane_tui::themes::BUILT_IN.iter().any(|built_in| built_in == &name) {
                errors.push(format!(
                    "{}: `{name}` is a built-in theme and cannot be redefined; rename the file.",
                    path.display()
                ));
                continue;
            }

            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str(&text) {
                    // The project root comes last, so it replaces a user theme
                    // of the same name.
                    Ok(palette) => {
                        themes.insert(name.to_string(), palette);
                    }
                    Err(source) => errors.push(format!("{}: {source}", path.display())),
                },
                Err(source) => errors.push(format!("{}: {source}", path.display())),
            }
        }
    }
    (themes, errors)
}

/// Every name the `theme` setting can take: the built-ins, then what was found.
fn theme_choices(
    customs: &std::collections::BTreeMap<String, octane_tui::theme::Palette>,
) -> Vec<String> {
    octane_tui::themes::BUILT_IN
        .iter()
        .map(|name| (*name).to_string())
        .chain(customs.keys().cloned())
        .collect()
}

/// The theme a configured name resolves to, or `None` if it names nothing.
fn resolve_theme(
    name: &str,
    customs: &std::collections::BTreeMap<String, octane_tui::theme::Palette>,
    depth: octane_tui::ColorDepth,
) -> Option<octane_tui::Theme> {
    octane_tui::Theme::named(name, depth).or_else(|| {
        customs.get(name).map(|palette| octane_tui::Theme::from_palette(palette, depth))
    })
}

/// A palette file seeded from a built-in, ready to be edited.
///
/// Every role is written even when it is `null`, because a file listing the
/// fifteen roles is the documentation of what can be changed — and `null` is not
/// a placeholder, it is the real "inherit the Octane default" that an omitted
/// role already means. Starting from `octane` therefore writes fifteen nulls,
/// which is both an honest description of that theme and drift-proof: it cannot
/// go stale when the brand palette is retuned.
///
/// Emitted by hand rather than through `serde_json`, which sorts keys
/// alphabetically and would scatter the roles; the values are hex digits and the
/// keys are literals, so there is nothing here to escape.
fn palette_file(base: &str) -> String {
    let palette = octane_tui::themes::PALETTES
        .iter()
        .find(|(name, _)| name == &base)
        .map(|(_, palette)| *palette)
        .unwrap_or_default();
    let hexes = [
        palette.surface,
        palette.on_accent,
        palette.user,
        palette.assistant,
        palette.reasoning,
        palette.tool,
        palette.success,
        palette.error,
        palette.warning,
        palette.dim,
        palette.accent,
        palette.code,
        palette.rail,
        palette.added,
        palette.removed,
    ];

    let body: Vec<String> = ROLES
        .iter()
        .zip(hexes)
        .map(|(role, hex)| match hex {
            Some(hex) => {
                format!("  \"{role}\": \"#{:02x}{:02x}{:02x}\"", hex.0, hex.1, hex.2)
            }
            None => format!("  \"{role}\": null"),
        })
        .collect();
    format!("{{\n{}\n}}\n", body.join(",\n"))
}

/// Copy a built-in into a theme file the user can edit, and say where it landed.
///
/// This is deliberately not a colour picker. Choosing fifteen 24-bit colours in a
/// terminal is a project of its own, and the thing it would produce is a file —
/// so the file is what this writes, seeded with a scheme that already works. The
/// project root rather than the user's, so it lands where every other
/// `/settings` write does and a teammate cloning the repository gets the scheme
/// the `theme` setting names.
fn create_custom_theme(workspace: &Utf8PathBuf, base: &str) -> Result<(String, String)> {
    if octane_tui::Theme::named(base, octane_tui::ColorDepth::TrueColor).is_none() {
        anyhow::bail!("{base} is not a built-in theme");
    }
    let name = format!("{base}-custom");
    let root = octane_config::roots(workspace)
        .last()
        .cloned()
        .unwrap_or_else(|| workspace.join(octane_config::CONFIG_DIR));
    let path = root.join(THEMES_DIR).join(format!("{name}.json"));

    // Never overwritten. The second visit to this row is someone who forgot they
    // had already made the theme, and losing their edits to it is the one
    // outcome that cannot be undone.
    if path.is_file() {
        return Ok((name, format!("`{path}` already exists; leaving it alone.\n")));
    }
    std::fs::create_dir_all(path.parent().unwrap_or(&root))?;
    std::fs::write(&path, palette_file(base))?;

    Ok((
        name,
        format!(
            "Wrote `{path}`, a copy of `{base}`.\n\nEdit any role and pick the theme again; \
             a role left `null` inherits octane's.\n"
        ),
    ))
}

/// The built-ins a custom theme can be copied from.
fn theme_base_picker() -> octane_tui::Picker {
    use octane_tui::{PickerItem, PickerKind};

    let items = octane_tui::Theme::built_in_names()
        .iter()
        .map(|name| PickerItem::new(*name, *name))
        .collect();

    octane_tui::Picker::new(
        PickerKind::SettingValue(THEME_BASE.to_string()),
        "copy from",
        items,
    )
}

/// `/settings` — which setting to change, and what it is worth now.
///
/// The rows carry the current values, so the picker is also the display the
/// old `/settings` printed. Two screens showing the same facts would drift.
fn settings_picker(
    settings: &octane_config::Settings,
    models: &[String],
    themes: &[String],
) -> octane_tui::Picker {
    use octane_tui::{PickerItem, PickerKind};

    let items = octane_config::edit::catalogue(models, themes)
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
    themes: &[String],
    ellipsis: &str,
    key: &str,
) -> Option<octane_tui::Picker> {
    use octane_tui::{PickerItem, PickerKind};

    let editable =
        octane_config::edit::catalogue(models, themes).into_iter().find(|e| e.key == key)?;
    let (current, configured) = editable.effective(settings);

    let mut items: Vec<PickerItem> = editable
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

    // Appended here rather than added to the catalogue, whose promise is that
    // every value it offers parses back as a valid setting. This one is a door,
    // not a value.
    if key == "theme" {
        items.push(
            PickerItem::new(NEW_THEME, format!("custom{ellipsis}"))
                .detail("Copy a built-in into a file you can edit"),
        );
    }

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
    themes: &[String],
    key: &str,
    value: &str,
) -> Result<String> {
    let Live { app, thinking, mode, history } = live;
    use octane_config::edit;

    let editable = edit::catalogue(models, themes)
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
        "theme" => {
            // Re-discovered rather than taking the caller's list, so a theme
            // file written moments ago by the builder resolves on the first try
            // instead of after a restart.
            let (customs, _) = discover_themes(workspace);
            let depth = octane_tui::ColorDepth::detect();
            let resolved = settings
                .theme
                .as_deref()
                .and_then(|name| resolve_theme(name, &customs, depth));
            if let Some(theme) = resolved {
                app.options_mut().theme = theme;
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
            report.push_str(&format!(
                "Set `{variable}` to finish:\n\n\
                 ```\n\
                 export {variable}=...\n\
                 ```\n\n\
                 Get one at {}\n\n\
                 The file references the variable rather than storing the key, \
                 so it is safe to commit.\n",
                recipe.help_url,
            ));
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
        let (state, _ready) = credential_state(&recipe);
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
    use octane_tools::{FileTracker, ReadTool, Tool};

    let tool = ReadTool::new(std::sync::Arc::new(FileTracker::new()));
    let ctx = tool_context(workspace);
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
                    built in, plus .octane/commands/*.md and skills
    @path           attach a file, with fuzzy completion
    !command        run a shell command
    tab             accept a completion
    shift+enter     newline (the box grows)
    alt+enter       newline, on terminals without shift+enter
    \\ then enter    newline, anywhere

  editing
    alt/ctrl+←/→    move by word
    ctrl+w          delete the word before the caret
    alt+d           delete the word after it
    ctrl+u          kill to the start of the line
    ctrl+k          kill to the end of it
    ctrl+y          put the last kill back
    ctrl+z          undo          ctrl+shift+z  redo
    up/down         earlier prompts, from the first line

  transcript
    pageup/pagedn   scroll
    shift+up/down   scroll a little
    ctrl+home/end   jump to top or bottom
    mouse wheel     scroll

  session
    shift+tab       cycle permission mode
    esc             interrupt while working
    ctrl+c          exit";

/// Expand a command file into the prompt it stands for.
///
/// Two-phase on purpose (`octane_commands::expand`): expansion is pure and hands
/// back the shell commands it *would* like run, which then go through the same
/// policy engine a model-issued command does. A command file arrives from
/// whatever repository was cloned, so it is given no more trust than the model —
/// without this split, `.octane/commands/x.md` would be arbitrary code execution
/// on the first `/x`.
async fn expand_command(
    app: &mut octane_tui::App,
    command: &octane_commands::Command,
    args: &str,
    workspace: &Utf8PathBuf,
    sandbox: &SandboxPolicy,
    permissions: &octane_config::settings::Permissions,
    mode: PermissionMode,
) -> Result<String> {
    use octane_permission::{Decision, Resource};

    let expansion = octane_commands::expand(command, args)?;
    if !expansion.needs_shell() {
        return Ok(expansion.prompt);
    }

    let (policy, _errors) = build_policy(workspace, permissions);
    let mut results = Vec::new();

    for request in expansion.shell_requests.clone() {
        let resource = Resource::command(&request.command);
        let decision = policy.evaluate(&resource, mode).decision;
        // `ask` is asked. `deny` is not: a deny the denied text can talk its way
        // out of is not a deny, and this text came from whatever repository was
        // cloned. Nothing runs unless the answer is yes.
        let approved = decision == Decision::Allow
            || (decision == Decision::Ask
                && approve_shell(app, &command.name, &request.command)?);

        if !approved {
            // Said in the transcript as well as marked `[not run: ...]` in the
            // prompt, because that marker is addressed to the model and the
            // person who typed the command is owed the refusal too.
            app.report_error(match decision {
                Decision::Deny => format!(
                    "/{}: `{}` is denied by the permission rules and was not run.",
                    command.name, request.command,
                ),
                _ => format!("/{}: `{}` was not run.", command.name, request.command),
            })?;
            continue;
        }

        // Shown in the transcript: this ran before the model saw anything, and a
        // prompt that silently contains the output of a shell command is the
        // kind of thing a user should be able to see happening.
        app.note(octane_protocol::ItemKind::ToolExecution {
            call_id: octane_protocol::ToolCallId::new(),
            name: "bash".into(),
            input: shell_input(&request.command),
        })?;
        // Only the text is spliced into the prompt; the exit code is for the
        // transcript, and the model reads the output itself.
        let (output, _, _) = run_shell(&request.command, workspace, sandbox).await;
        results.push((request, output));
    }

    Ok(expansion.apply_shell_results(&results))
}

/// The approval prompt for a command file's ``!`shell` `` substitution.
///
/// Names the command file as well as the command, because the question here is
/// not "is `git diff` safe" but "did I mean to let `/review` run something" —
/// and this prompt is the only place that provenance is visible.
fn shell_approval(command: &str, shell: &str) -> octane_tui::ApprovalPrompt {
    octane_tui::ApprovalPrompt {
        resource: octane_permission::Resource::command(shell),
        summary: format!("/{command} wants to run this and use its output"),
        diff: None,
    }
}

/// Put one shell request in front of the user, outside a turn.
///
/// `run_turn` services approvals from a channel because the runner is off in
/// another task; here the prompt and the keystroke that answers it are both on
/// this thread, so this is that `select!` reduced to what remains of it — draw,
/// wait for a key, stop once the question has an answer. Going through
/// `TuiApprover` would buy a `spawn` and a channel to talk to ourselves.
fn approve_shell(app: &mut octane_tui::App, command: &str, shell: &str) -> Result<bool> {
    use tokio::sync::oneshot::error::TryRecvError;

    let (responder, mut answer) = tokio::sync::oneshot::channel();
    app.set_approval(shell_approval(command, shell), responder);

    loop {
        // Terminal errors are swallowed into a refusal rather than propagated.
        // Returning `Err` here would leave the prompt installed with its
        // responder dropped, and a ghost prompt owns every keystroke in the
        // main loop — it would silently eat the next thing the user typed.
        // Declining is also the safe reading: nothing runs.
        if app.draw().is_err() {
            app.dismiss_approval();
            return Ok(false);
        }
        match answer.try_recv() {
            Ok(reply) => return Ok(reply.is_approval()),
            // Answered by nobody: the prompt was dropped without a decision.
            // "No one is there to ask" reads as no, exactly as it does in
            // `TuiApprover`.
            Err(TryRecvError::Closed) => return Ok(false),
            Err(TryRecvError::Empty) => {
                if app.poll().is_err() {
                    app.dismiss_approval();
                    return Ok(false);
                }
            }
        }
    }
}

/// The `bash` input for a `!command`.
///
/// Built once because it is both shown and run: two copies could disagree, and
/// the transcript would then name a command other than the one that executed.
fn shell_input(command: &str) -> String {
    // The description doubles as what the transcript's call line shows, and a
    // constant there said "user-issued shell command" on every row while the
    // command itself — the only part worth reading — was not displayed at all.
    serde_json::json!({ "command": command, "description": command }).to_string()
}

/// Run a shell command through the `bash` tool, so it gets the same containment
/// and bounds an agent-issued command would.
/// Output, the tool's own metadata, and whether it failed.
///
/// The metadata is carried back rather than dropped so the transcript can say
/// `exit 0` under the call instead of repeating the command it already shows.
async fn run_shell(
    command: &str,
    workspace: &Utf8PathBuf,
    sandbox: &SandboxPolicy,
) -> (String, Option<serde_json::Value>, bool) {
    use octane_tools::{BashTool, Tool};

    let tool = BashTool::new(sandbox.clone());
    let ctx = tool_context(workspace);

    match tool.execute(&shell_input(command), &ctx).await {
        Ok(outcome) => {
            let failed = outcome
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("exit_code"))
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|code| code != 0);
            (outcome.output, outcome.metadata, failed)
        }
        Err(error) => (error.to_string(), None, true),
    }
}

/// Load a skill's body, if one by that name exists.
///
/// Tier 2 of progressive disclosure: read on activation, never at startup.
fn skill_body(workspace: &Utf8PathBuf, name: &str) -> Option<String> {
    let (skills, _) = octane_skills::discover(&config_dirs(workspace, "skills"));
    let skill = skills.into_iter().find(|skill| skill.name() == name)?;
    skill.load_body().ok().map(|body| body.render())
}

/// Discovered skills, offered in `/` completion.
///
/// Tier-1 only: name and description. Loading a body here would defeat
/// progressive disclosure, which is the whole reason the format is tiered
/// (`RESEARCH.md` §D).
fn skill_candidates(workspace: &Utf8PathBuf) -> Vec<octane_tui::Candidate> {
    let (skills, _errors) = octane_skills::discover(&config_dirs(workspace, "skills"));
    skills
        .into_iter()
        .map(|skill| {
            // Not truncated here: the popup elides against its own width, and
            // a fixed cut made here was both wrong at other widths and blind to
            // word boundaries — it ended `/rust-review`'s line on "Use".
            octane_tui::Candidate::new(
                format!("/{}", skill.name()),
                skill.frontmatter.description.clone(),
            )
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
        "\
Find everything relevant to this in the codebase, then explain it:

{query}

Use the `task` tool with the `research` agent, and launch several in one message \
so they run concurrently. Give each a different angle — by symbol name, by file \
or directory, by the concept described in prose, by tests that exercise it. A \
single search finds only what you already knew to look for; the useful misses \
are where something is named differently.

Then synthesise. Report:
- where the thing lives, as `path:line`
- how the pieces relate, not a list of what each file contains
- anything that looks like it should be involved but is not

If it is genuinely absent, say so plainly rather than reporting the nearest \
match as though it were the answer."
    )
}

/// `/mcp` — what is connected, and what it brought.
///
/// A server that failed is listed with its reason rather than omitted, the same
/// rule `octane models` follows for an unusable provider: a server that vanishes
/// silently is an hour spent looking for the tool it was supposed to supply.
fn render_mcp(workspace: &Utf8PathBuf, mcp: &Mcp) -> String {
    let (configured, errors) = octane_mcp::config::discover(
        &octane_config::roots(workspace),
        |name| std::env::var(name).ok(),
    );

    let mut out = String::new();
    for error in &errors {
        out.push_str(&format!("  ! {error}\n"));
    }
    if !errors.is_empty() {
        out.push('\n');
    }

    if configured.is_empty() {
        out.push_str("No MCP servers are configured.\n\n");
        out.push_str(&format!("  Declare one in .octane/{}\n", octane_mcp::config::CONFIG_FILE));
        return out;
    }

    for (name, server) in &configured {
        // Which tools came from which server, so a name in `/tools` can be
        // traced back to the thing that supplied it.
        let prefix = format!("mcp__{name}__");
        let tools: Vec<&str> = mcp
            .tools
            .iter()
            .map(|tool| tool.name())
            .filter(|tool| tool.starts_with(&prefix))
            .map(|tool| &tool[prefix.len()..])
            .collect();

        let transport = match (&server.command, &server.url) {
            (Some(command), _) => command.clone(),
            (None, Some(url)) => url.clone(),
            (None, None) => String::new(),
        };
        let state = if !server.enabled {
            "disabled".to_string()
        } else if tools.is_empty() {
            // Enabled, connected or not, but contributing nothing. Said plainly
            // rather than shown as an empty list.
            "no tools".to_string()
        } else if tools.len() == 1 {
            "1 tool".to_string()
        } else {
            format!("{} tools", tools.len())
        };

        out.push_str(&format!("  {name:<16} {state:<10} {transport}\n"));
        for tool in tools {
            out.push_str(&format!("      {tool}\n"));
        }
    }
    out
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

    use octane_tui::render::thousands;

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
    /// Tools adopted from connected MCP servers. Registered alongside the
    /// built-ins so the model cannot tell them apart and the policy engine does
    /// not have to.
    mcp_tools: &'a [std::sync::Arc<dyn octane_tools::Tool>],
}

/// Whether the turn asked for another one immediately.
enum TurnFollowUp {
    Done,
    /// The user rejected an action and said what to do instead.
    Redirected,
}

/// Run a turn, and any turn its rejection asks for.
///
/// The loop is here rather than at the three call sites because a redirect is
/// the same session continuing, not a new submission — and it cannot spin: each
/// pass needs the user to type instructions into an approval prompt.
async fn run_turn(
    app: &mut octane_tui::App,
    session: &Session<'_>,
    history: &mut Vec<octane_protocol::Message>,
    session_usage: &mut SessionUsage,
) -> Result<()> {
    while let TurnFollowUp::Redirected =
        run_one_turn(app, session, history, session_usage).await?
    {}
    Ok(())
}

async fn run_one_turn(
    app: &mut octane_tui::App,
    session: &Session<'_>,
    history: &mut Vec<octane_protocol::Message>,
    session_usage: &mut SessionUsage,
) -> Result<TurnFollowUp> {
    let Session {
        model,
        workspace,
        sandbox,
        mode,
        thinking,
        permissions,
        prompt,
        summarizer,
        mcp_tools,
    } = *session;
    use octane_core::{EventSink, ModelStepSource, TurnRunner};
    use octane_protocol::TurnId;
    use octane_tools::ToolRegistry;
    use octane_tui::{Activity, TuiApprover};

    let tracker = std::sync::Arc::new(octane_tools::FileTracker::new());
    let mut registry = ToolRegistry::new();
    octane_tools::register_all(&mut registry, tracker, sandbox.clone());
    // Same registry as the built-ins, which is what puts them behind the same
    // policy engine and the same sorted, cache-stable schema list.
    for tool in mcp_tools {
        registry.register(tool.clone());
    }

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
            mcp_tools: mcp_tools.to_vec(),
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
                                    octane_tui::render::summarize_input_with(
                                        name,
                                        input,
                                        &app.options().glyphs,
                                    )
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
        app.report_error(outcome.stop_reason.summary())?;
    }

    // The prompt offers "or type instructions", and this is where they land.
    // Pushed as an ordinary user message: the denial still ended the turn — the
    // model is never handed a refusal to argue with — and the redirect opens
    // the next one as a fresh instruction, which is what the user meant by
    // typing it. Before this it was collected by the UI and dropped.
    if let Some(instructions) = outcome.redirect.filter(|text| !text.trim().is_empty()) {
        app.note(octane_protocol::ItemKind::UserMessage { text: instructions.clone() })?;
        history.push(octane_protocol::Message::text(
            octane_protocol::Role::User,
            instructions,
        ));
        return Ok(TurnFollowUp::Redirected);
    }

    let used = octane_context::prune::estimate_tokens(history);
    app.status_mut().context_used = budget.utilization(used);

    Ok(TurnFollowUp::Done)
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
    /// The parent's MCP tools. A subagent sees the same servers, still narrowed
    /// by its own definition — the filter below is what decides, not the source.
    mcp_tools: Vec<std::sync::Arc<dyn octane_tools::Tool>>,
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
        for tool in &self.mcp_tools {
            registry.register(tool.clone());
        }
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

    fn summary(preview: Option<&str>) -> octane_session::SessionSummary {
        octane_session::SessionSummary {
            meta: octane_session::SessionMeta {
                id: octane_protocol::SessionId::new(),
                timestamp: "2026-07-27T09:41:02.123456789Z".into(),
                cwd: Utf8PathBuf::from("/tmp/project"),
                model: None,
                cli_version: "0".into(),
            },
            path: Utf8PathBuf::from("/tmp/sessions/one.jsonl"),
            messages: 7,
            preview: preview.map(Into::into),
        }
    }

    /// The row is chosen by what was asked in it, and resumed by the same id
    /// `octane resume <id>` takes. Keying a row by its path would work until the
    /// day the two lookups disagreed about which file an id names.
    #[test]
    fn a_session_row_is_keyed_by_its_id_and_filterable_by_its_preview() {
        let session = summary(Some("why does the sandbox deny this write"));
        let id = session.meta.id.as_str().to_string();

        let mut picker = session_picker(vec![session], "\u{2026}");
        for ch in "sandbox".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.choose(), Some(id.as_str()), "the key must be the full id");
    }

    /// A session someone opened and closed has no preview, and a row with an
    /// empty label is one nothing can be filtered or aimed at.
    #[test]
    fn a_session_with_nothing_asked_in_it_still_has_a_label() {
        let picker = session_picker(vec![summary(None)], "\u{2026}");
        let label = &picker.matches()[0].label;
        assert!(label.contains("2026-07-27 09:41"), "the timestamp anchors the row: {label}");
        assert!(label.len() > 16, "an empty preview must not leave a bare stamp: {label}");
    }

    /// The builder's whole promise is that what it writes is loadable, and it
    /// writes the file by hand because `Palette` does not serialize. Nothing but
    /// this catches a role misspelled in `ROLES` — `deny_unknown_fields` would
    /// reject the file, but only once a user picked that base.
    #[test]
    fn a_seeded_theme_file_reloads_as_the_scheme_it_copied() {
        for (name, palette) in octane_tui::themes::PALETTES {
            let text = palette_file(name);
            let parsed: octane_tui::theme::Palette = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{name} did not reload: {error}\n{text}"));
            assert_eq!(&parsed, palette, "{name} lost a role on the way through");
        }

        // Octane's copy is fifteen nulls, and null is not "missing a colour" —
        // it is the inheritance that makes an unset role octane's in the first
        // place, so the file reloads as the default palette exactly.
        let octane: octane_tui::theme::Palette =
            serde_json::from_str(&palette_file("octane")).expect("octane reloads");
        assert_eq!(octane, octane_tui::theme::Palette::default());
        assert_eq!(palette_file("octane").matches("null").count(), ROLES.len());
    }

    /// A picker that can offer a name nothing resolves is a picker that can put
    /// the session into the error it exists to prevent.
    #[test]
    fn every_theme_the_picker_offers_resolves_to_a_theme() {
        let mut customs = std::collections::BTreeMap::new();
        customs.insert("mine".to_string(), octane_tui::theme::Palette::default());

        for name in theme_choices(&customs) {
            assert!(
                resolve_theme(&name, &customs, octane_tui::ColorDepth::TrueColor).is_some(),
                "{name} is offered but resolves to nothing"
            );
        }
        // And the builder's row is not one of them: it opens a picker, and a
        // file named after it would be written to `theme` as a name.
        assert!(!theme_choices(&customs).iter().any(|name| name == NEW_THEME));
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

    /// Every built-in must have a code path. A name in `BUILTINS` with no arm in
    /// the dispatch falls through to the skill lookup and then to "Unknown
    /// command", so the menu offers something that does nothing — and nothing
    /// about that fails at compile time.
    ///
    /// The list below is deliberately a second copy: it exists to disagree.
    #[test]
    fn every_offered_builtin_is_one_the_dispatch_handles() {
        const DISPATCHED: &[&str] = &[
            "thinking", "help", "models", "connect", "cs", "agents", "tools", "stats",
            "settings", "clear", "exit", "mcp",
        ];
        for builtin in BUILTINS {
            assert!(
                DISPATCHED.contains(&builtin.name),
                "/{} is offered in the menu but has no arm in the dispatch",
                builtin.name,
            );
        }
        for name in DISPATCHED {
            assert!(
                BUILTINS.iter().any(|builtin| builtin.name == *name),
                "/{name} is dispatched but never offered, so nobody can discover it",
            );
        }
    }

    /// The registry is what `/` reads from, so a built-in missing from it is a
    /// command the user cannot find.
    #[test]
    fn the_registry_offers_every_builtin() {
        let (registry, _) = command_registry(&Utf8PathBuf::from("."));
        for builtin in BUILTINS {
            let entry = registry.get(builtin.name).expect(builtin.name);
            assert_eq!(entry.kind, octane_commands::Kind::Builtin);
        }
    }

    /// Prose built from an indented source literal must not carry the source's
    /// indentation into what is sent or shown. `/cs` did: it had been flattened
    /// into a single-line literal with the indentation baked in as escapes, so
    /// the model received "in one          message" mid-sentence.
    ///
    /// A `\` continuation is the correct tool and eats the leading whitespace,
    /// which a comment here previously claimed it did not.
    #[test]
    fn no_generated_prose_carries_source_indentation() {
        let prompt = codebase_search_prompt("where is the sandbox built?");
        for line in prompt.lines() {
            assert!(
                !line.trim_start().contains("  "),
                "a run of spaces survived into the prompt: {line:?}",
            );
        }
        assert!(prompt.contains("in one message so they run concurrently"));
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

    /// The prompt is the whole of what stands between a command file that
    /// arrived with the cloned repository and a shell, so it must name the
    /// command that would actually run — the same string handed to `run_shell`
    /// — and the file asking for it. A prompt that showed anything else would be
    /// worse than no prompt, since it would collect a genuine "yes" for
    /// something the user never saw.
    #[test]
    fn the_approval_names_the_command_and_the_file_that_wants_it() {
        let prompt = shell_approval("review", "git diff --cached");

        assert_eq!(
            prompt.resource,
            octane_permission::Resource::command("git diff --cached"),
            "the resource asked about must be the one the policy evaluated",
        );
        let title = prompt.title();
        assert!(title.contains("git diff --cached"), "{title}");
        assert!(title.contains("/review"), "{title}");
        assert!(prompt.diff.is_none(), "a command has no diff to offer");
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
