# Architecture

Orientation for anyone working in this repository: the
toolchain's sharp edges, the crate layering, and the invariants whose violation
is silent.

octane is an AI coding agent for the terminal, in Rust. It is early, but it runs end to end: the harness, tools, TUI, and provider layer all work, and a turn goes prompt → streamed tool call → sandboxed execution → answer against a real model.

## Toolchain

The Nix flake owns the toolchain. There is no rustup, and `cargo` is not on `PATH` outside the shell.

```bash
nix develop -c cargo test --workspace     # or `direnv allow` once, then plain cargo
```

**The flake only sees git-tracked files.** A new file that has not been `git add`ed makes `nix develop` fail with "not tracked by Git", which reads as a flake error rather than a staging problem. `git add -A` before building after adding files.

## Commands

```bash
nix develop -c cargo test --workspace
nix develop -c cargo test -p octane-permission                    # one crate
nix develop -c cargo test -p octane-permission a_session_grant    # one test, by substring
nix develop -c cargo clippy --workspace --all-targets             # must be silent
nix develop -c cargo run -p octane-cli                            # the TUI
```

`octane tool <name> '<json>'` runs a single tool under the same policy and containment a real turn applies. It is the fastest way to check the sandbox without spending a token. `octane doctor` prints the resolved config; `octane models` prints configured providers and, importantly, unusable ones with the reason.

## Testing the TUI

Unit tests cover everything except `octane-tui::app`, which is the only module touching a terminal. For the rest:

```bash
nix develop -c python3 scripts/tui-smoke.py    # drives the real binary through a pty
```

It fails if idle output exceeds a byte budget. That check exists because the two flicker bugs found so far both rendered correctly; the symptom was only traffic on the wire (9.6 KB/s while idle). Any change to `app.rs` draw/poll logic should be re-measured with it.

For visual inspection, drive the binary through a pty with a terminal emulator. A python with `pyte` can be built ad hoc:

```bash
nix build --impure --no-link --print-out-paths --expr \
  'let pkgs = import <nixpkgs> {}; in pkgs.python312.withPackages (ps: [ ps.pyte ])'
```

The harness **must answer `ESC[6n`** (device status report) with something like `\x1b[20;1R`, or the TUI exits with "cursor position could not be read". Every TUI bug found so far was found this way and was invisible to unit tests.

## Architecture

Fifteen crates, one responsibility each, strictly one-way dependencies:

```
protocol ← provider ← context
         ← sandbox  ← tools ← mcp
         ← permission
                        ↖ core ← tui ← cli
config, memory, skills, commands, session ─↗
```

`octane-cli` owns the composition: it discovers commands, skills, agents,
memory and prior sessions and hands them to `core`, which is why those crates
hang off the side rather than sitting in the chain.

**A session is recorded as it happens, never rewritten.** `octane-session`
appends one JSONL line per message under `~/.octane/sessions/`, so a session
killed by a panic or a closed terminal is resumable up to its last complete
line — and a truncated final line is expected rather than corruption, because
the process was killed mid-write. Rewriting one document per turn would put the
data exactly where the crash is. There is deliberately no index: listing walks
the directory, which is fine for thousands of sessions and would need
rethinking at a million.

**Slash commands go through one registry.** `octane_commands::Registry` holds the
client's built-ins alongside every discovered `.octane/commands/*.md`, and `/`
completion reads from it rather than from a list in the CLI. Built-ins are
registered *last* and win: command files come from whatever repository was
cloned, so without that ordering, shipping a `commands/clear.md` would be enough
to redefine what "start over" does. A file that tries is reported, not dropped.
A command expands to a prompt and never to a code path (`RESEARCH.md` §F); its
``!`shell` `` substitutions run through the same policy engine a model-issued
command does, before the model sees anything.

**MCP tools are not a second path into tool execution.** Servers are declared in
`mcp.json`, spawned once per session, and their tools registered into the same
`ToolRegistry` as the built-ins — so they inherit the sorted, cache-stable schema
list and are resolved by the same policy engine as `mcp(server/tool)`. There is
no weaker route: an MCP tool the policy denies is denied exactly as `bash` would
be. `McpTool::is_mutating` returns true unconditionally, because a third-party
schema cannot be trusted to describe its own effects, which is what keeps a
`plan` agent from calling one. Server `instructions` are untrusted third-party
text: fenced, attributed, and placed behind project memory in the preamble.

`octane-protocol`, `octane-permission`, `octane-sandbox`, `octane-memory`, `octane-skills` and `octane-commands` have **no internal dependencies at all**. Keep it that way; the layering is the design — it is why the command registry takes the client's built-ins as data instead of knowing what a terminal or a session is.

**`octane-core` coordinates and holds no domain logic.** Policy lives in `octane-permission`, containment in `octane-sandbox`, token accounting in `octane-context`, tool behaviour in `octane-tools`. Every collaborator is a trait, which is why `turn.rs` is testable against a scripted provider with no model, shell, or network. If you find yourself adding a decision to `core`, it probably belongs in the crate that owns that question.

### Two layers that are easy to conflate

**Policy** (`octane-permission`) asks *should this be allowed?* and runs before a command does. **Containment** (`octane-sandbox`) asks *what can the process reach if it does something other than what it said?* and is enforced by the kernel. Both are needed and they are not interchangeable.

### Non-obvious invariants

These are load-bearing and their violation is silent:

- **Tool schemas live in a `BTreeMap`.** They sit in the cached prompt prefix, so iteration order is part of the cache key. Verified against Codex at the time of writing: its `McpConnectionSet.servers` is a `HashMap` and `list_all_tools` returns tools in its iteration order unsorted, so a multi-server setup gets a different tool order per process. Note the precise cost — Rust seeds `RandomState` once per process, so the order is stable *within* a run and differs *across* restarts. Codex's built-in tools are not affected; they are pushed onto a `Vec` in fixed code order.
- **The prompt is append-only.** Configuration changed mid-session? Append a developer message. Editing what was already sent costs a full cache miss on every subsequent turn.
- `.git/` and the agent's own config dir are read-only inside writable roots. Otherwise "write files in the project" transitively grants `.git/hooks/pre-commit`, which is arbitrary code on the next commit.
- **Sandbox paths are `-D` parameters, never interpolated** into the Seatbelt profile. Codex does the same, independently, and hardcodes `/usr/bin/sandbox-exec` for the same PATH-impersonation reason — the two backends agreeing is the strongest evidence either is right.
- **On Linux, the bubblewrap carve-outs are bound *after* the writable root** that contains them, because the later mount wins. Reverse them and the sandbox reports success while granting `.git/hooks`.
- **An unsupported platform is a sandbox *error*, not a pass.** macOS uses Seatbelt, Linux uses bubblewrap. Windows has no backend and refuses rather than running unconfined — except when it detects it is already inside WSL or a container, which is the `ExternalSandbox` case arriving by detection. The blocker is specific and worth knowing: a Windows restricted token lowers what the *process* is and cannot express `read_only_subpaths`, so such a backend would honour "write only in the workspace" while silently ignoring "except `.git/`" — passing its own tests while failing the one invariant that matters. Doing it properly needs per-path deny ACEs, which needs `unsafe`, which needs a crate that opts out of the workspace lint and a Windows CI job to be trustworthy.
- **A denial ends the turn** rather than being reported to the model, which would invite it to route around the refusal.
- **Permission precedence is `Deny > Ask > Allow`**, with one documented exception in `Policy::evaluate`: an interactive grant made this session outranks a configured `ask`, or "remember my answer" is impossible.
- **`.gitignore` is honoured outside git repos** (`require_git(false)`), unlike `ignore`'s default.

### TUI

Full screen on the alternate screen, as a vertical stack of **panes**: header, transcript, approval, activity, composer, status.

A pane implements `octane_tui::component::Pane` — `constraint(width)` says how much room it wants, `render(area, buf)` draws it — and lives in the module owning its state, so the composer's pane is in `composer.rs` and the status line's in `status.rs`. `app` collects the constraints, splits once, and renders each into what it got; it holds no idea what any pane looks like.

**Measuring and drawing stay on the same type.** Split into a `foo_height` beside a `foo_widget`, they drift, and the drift is silent because each half is individually correct: a pane measured one row short simply loses its last line. That is why `Pane` has exactly those two methods and why panes render from `&self` — anything needing `&mut` (wrapping the transcript mutates its cache) is computed before the frame and passed in, which is what `TranscriptView` is for.

Everything except `app` is pure: state in, cells out. A pane is testable by rendering it into a `ratatui::buffer::Buffer` and asserting on cells, with no terminal involved.

Two rules or the region visibly flickers, and neither is catchable by a unit test because both render correctly: redraw only when `dirty`, and call `Terminal::resize` only when the size actually changed (it resets ratatui's buffers, discarding the cell diff).

Glyphs are chosen for **width safety**. A character that renders double-width in one terminal and single in another destroys every box and aligned column. `octane-tui::glyphs` documents the ranges that are safe; `⚡` (U+26A1) and `✔` (U+2714) are not, `✓` (U+2713) is. There is an ASCII fallback set and it must reach the transcript and status line, not just the banner.

### Providers

One JSON file per provider declaring a connection and many models, discovered from `~/.octane/providers/*.json` and `.octane/providers/*.json`. `api` and `baseUrl` are provider defaults that any model may override, since a single gateway commonly fronts `/chat/completions`, `/responses`, and `/messages` at once. Auth is typed rather than a key string. Worked examples in `examples/providers/`.

## Conventions

**`unsafe_code = "forbid"`** workspace-wide. This blocks `std::env::set_var`, so anything reading the environment takes an injected lookup instead. See `octane_provider::config::resolve_env_with`.

**Tests are named for the property they protect**, not the function they call: `a_path_with_shell_metacharacters_cannot_reach_the_profile`, `a_session_grant_cannot_survive_a_switch_to_plan`, `changing_output_is_progress_not_a_loop`. Security properties get a negative control where one is possible. `danger_full_access_is_genuinely_unconfined` is what makes the sandbox tests evidence rather than coincidence.

**Comments explain why, not what**, and especially why an obvious alternative was rejected. Much of this codebase is a decision with a cost attached; the reasoning is the part worth keeping.

## RESEARCH.md

The design is derived from a survey of Claude Code, Codex CLI, opencode, Crush, Antigravity CLI, pi, Cline, and Junie. `RESEARCH.md` is the rationale for most non-obvious choices here and is cited by section from the code (`RESEARCH.md` §H, §L, …). Read the relevant section before changing something that looks arbitrary. It usually is not.
