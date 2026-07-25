# Architecture

Orientation for anyone — human or agent — working in this repository: the
toolchain's sharp edges, the crate layering, and the invariants whose violation
is silent.

octane is an AI coding agent for the terminal, in Rust. It is early, but it runs end to end: the harness, tools, TUI, and provider layer all work, and a turn goes prompt → streamed tool call → sandboxed execution → answer against a real model.

## Toolchain

The Nix flake owns the toolchain — there is no rustup and `cargo` is not on `PATH` outside the shell.

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

`octane tool <name> '<json>'` runs a single tool under the same policy and containment a real turn applies — the fastest way to check the sandbox without spending a token. `octane doctor` prints the resolved config; `octane models` prints configured providers and, importantly, unusable ones with the reason.

## Testing the TUI

Unit tests cover everything except `octane-tui::app`, which is the only module touching a terminal. For the rest:

```bash
nix develop -c python3 scripts/tui-smoke.py    # drives the real binary through a pty
```

It fails if idle output exceeds a byte budget. That check exists because the two flicker bugs found so far both **rendered correctly** — the symptom was only traffic on the wire (9.6 KB/s while idle). Any change to `app.rs` draw/poll logic should be re-measured with it.

For visual inspection, drive the binary through a pty with a terminal emulator. A python with `pyte` can be built ad hoc:

```bash
nix build --impure --no-link --print-out-paths --expr \
  'let pkgs = import <nixpkgs> {}; in pkgs.python312.withPackages (ps: [ ps.pyte ])'
```

The harness **must answer `ESC[6n`** (device status report) with something like `\x1b[20;1R`, or the TUI exits with "cursor position could not be read". Every TUI bug found so far was found this way and was invisible to unit tests.

## Architecture

Thirteen crates, one responsibility each, strictly one-way dependencies:

```
protocol ← provider ← context
         ← sandbox  ← tools ← mcp
         ← permission
                        ↖ core ← tui ← cli
```

`octane-protocol` and `octane-permission`, `octane-sandbox`, `octane-memory`, `octane-skills`, `octane-commands` have **no internal dependencies at all**. Keep it that way; the layering is the design.

**`octane-core` coordinates and holds no domain logic.** Policy lives in `octane-permission`, containment in `octane-sandbox`, token accounting in `octane-context`, tool behaviour in `octane-tools`. Every collaborator is a trait, which is why `turn.rs` is testable against a scripted provider with no model, shell, or network. If you find yourself adding a decision to `core`, it probably belongs in the crate that owns that question.

### Two layers that are easy to conflate

**Policy** (`octane-permission`) asks *should this be allowed?* and runs before a command does. **Containment** (`octane-sandbox`) asks *what can the process reach if it does something other than what it said?* and is enforced by the kernel. Both are needed and they are not interchangeable.

### Non-obvious invariants

These are load-bearing and their violation is silent:

- **Tool schemas live in a `BTreeMap`.** They sit in the cached prompt prefix; hash-map iteration order reorders them between turns and voids every cache hit. Codex shipped this bug.
- **The prompt is append-only.** Configuration changed mid-session? Append a developer message. Editing what was already sent costs a full cache miss on every subsequent turn.
- **`.git/` and the agent's own config dir are read-only inside writable roots.** Otherwise "write files in the project" transitively grants `.git/hooks/pre-commit` — arbitrary code on the user's next commit.
- **Sandbox paths are `-D` parameters, never interpolated** into the Seatbelt profile.
- **An unsupported platform is a sandbox *error*, not a pass.** Linux and Windows backends are unimplemented, so `bash` refuses rather than running unconfined.
- **A denial ends the turn** rather than being reported to the model, which would invite it to route around the refusal.
- **Permission precedence is `Deny > Ask > Allow`**, with one documented exception in `Policy::evaluate`: an interactive grant made this session outranks a configured `ask`, or "remember my answer" is impossible.
- **`.gitignore` is honoured outside git repos** (`require_git(false)`), unlike `ignore`'s default.

### TUI

Full screen on the alternate screen: header, transcript, composer, status. Everything except `app` is pure — state in, lines out.

Two rules or the region visibly flickers, and neither is catchable by a unit test because both render correctly: redraw only when `dirty`, and call `Terminal::resize` only when the size actually changed (it resets ratatui's buffers, discarding the cell diff).

Glyphs are chosen for **width safety**. A character that renders double-width in one terminal and single in another destroys every box and aligned column. `octane-tui::glyphs` documents the ranges that are safe; `⚡` (U+26A1) and `✔` (U+2714) are not, `✓` (U+2713) is. There is an ASCII fallback set and it must reach the transcript and status line, not just the banner.

### Providers

One JSON file per provider declaring a connection and many models, discovered from `~/.octane/providers/*.json` and `.octane/providers/*.json`. `api` and `baseUrl` are provider defaults that **any model may override** — a single gateway commonly fronts `/chat/completions`, `/responses`, and `/messages` at once. Auth is typed rather than a key string. Worked examples in `examples/providers/`.

## Conventions

**`unsafe_code = "forbid"`** workspace-wide. This blocks `std::env::set_var`, so anything reading the environment takes an injected lookup instead — see `octane_provider::config::resolve_env_with`.

**Tests are named for the property they protect**, not the function they call: `a_path_with_shell_metacharacters_cannot_reach_the_profile`, `a_session_grant_cannot_survive_a_switch_to_plan`, `changing_output_is_progress_not_a_loop`. Security properties get a negative control where one is possible — `danger_full_access_is_genuinely_unconfined` is what makes the sandbox tests evidence rather than coincidence.

**Comments explain why, not what**, and especially why an obvious alternative was rejected. Much of this codebase is a decision with a cost attached; the reasoning is the part worth keeping.

## RESEARCH.md

The design is derived from a survey of Claude Code, Codex CLI, opencode, Crush, Antigravity CLI, pi, Cline, and Junie. `RESEARCH.md` is the rationale for most non-obvious choices here and is cited by section from the code (`RESEARCH.md` §H, §L, …). Read the relevant section before changing something that looks arbitrary — it usually is not.
