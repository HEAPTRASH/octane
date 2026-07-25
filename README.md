# octane-agent

An AI coding agent for the terminal, in Rust.

Design notes and the survey the architecture is derived from are in [`RESEARCH.md`](RESEARCH.md) — how Claude Code, Codex CLI, opencode, Crush, and Antigravity CLI actually work, and which of their decisions this project adopts and why.

## Status

Early. The subsystems below are implemented and tested. Seven tools work — `read`, `write`, `edit`, `bash`, `glob`, `grep`, `list` — with `bash` under real OS containment. The TUI runs; model inference is not wired up yet, so `!shell` and `/commands` work end to end but prompts do not.

```
$ octane doctor                                        # resolved config: sandbox, writable roots, mode
$ octane tool read '{"path":"src/main.rs","limit":40}' # run one tool, no model involved
$ octane tool bash '{"command":"cargo test","description":"runs the tests"}'
$ octane tool grep '{"pattern":"TODO","mode":"files","glob":"**/*.rs"}'
$ octane                                               # interactive session
```

`octane tool` applies the same policy and containment a real turn would, which makes "is the sandbox actually on?" answerable without spending a token.

## Getting started

The Nix flake owns the toolchain — no `rustup` needed.

```bash
direnv allow          # or: nix develop
cargo test --workspace
cargo run -p octane-cli -- doctor
```

Without `nix-direnv`, `use flake` re-evaluates on every `cd`. To cache it:

```bash
nix profile install nixpkgs#nix-direnv
echo 'source $HOME/.nix-profile/share/nix-direnv/direnvrc' >> ~/.config/direnv/direnvrc
```

## Workspace layout

One responsibility per crate. The dependency direction is strictly one-way — nothing below depends on anything above it in this table.

| Crate | Owns | Does **not** own |
|---|---|---|
| `octane-protocol` | Thread / Turn / Item / Message / Part / Event — the shared vocabulary and wire format | any behaviour |
| `octane-provider` | `LanguageModel` trait, normalized `StreamEvent`, `ProviderTransform`, pricing | prompt assembly, tool execution |
| `octane-tools` | `Tool` trait, `ToolRegistry`, the built-in tools | whether a call is permitted |
| `octane-permission` | `action(target)` policy → allow / ask / deny; modes | OS enforcement |
| `octane-sandbox` | Seatbelt / Landlock / AppContainer containment | consent |
| `octane-context` | token budget, pruning, compaction thresholds | making model calls |
| `octane-memory` | `OCTANE.md` / `AGENTS.md` layering and `@imports` | skills |
| `octane-skills` | Agent Skills discovery, progressive disclosure | activation policy |
| `octane-commands` | slash command discovery and template expansion | running the shell |
| `octane-mcp` | MCP JSON-RPC lifecycle, stdio transport, tool adapter | permission decisions |
| `octane-core` | the ReAct loop, agents, prompt assembly, stop conditions | **any domain logic** |
| `octane-tui` | terminal client: composer, keymap, event rendering, approvals | agent logic |
| `octane-cli` | argument parsing, config resolution | everything else |

`octane-core` coordinates and holds no domain logic. Every collaborator is a trait, which is why the loop is testable without a model, a shell, or a network — see the scripted-provider tests in `crates/octane-core/src/turn.rs`.

## The TUI is scrollback, not full screen

The most consequential UI decision (`RESEARCH.md` §H). There are two ways to build a terminal UI: own the viewport as a cell grid (opencode, Amp), or append to scrollback and redraw only the live rows (Claude Code, Codex, pi).

Owning the viewport costs the scrollback buffer, terminal search, and sane copy/paste — all of which then have to be reimplemented, and mouse scrolling never quite feels right afterwards. A coding agent is a linear chat, which maps onto the terminal's native model exactly, so it pays that price for nothing.

So: finished content goes into real scrollback via `Terminal::insert_before`, and only a small `Viewport::Inline` region — composer, activity line, status — is redrawn. Frames are wrapped in synchronized-output escapes (`CSI ?2026h`/`l`) so the terminal presents them atomically instead of tearing.

Everything in `octane-tui` except `app` is pure — state in, lines out — because rendering and keybinding logic is where the bugs are and it should be testable by calling a function.

## The two layers that are easy to conflate

**Policy** (`octane-permission`) asks *should this be allowed?* and runs before a command does.
**Containment** (`octane-sandbox`) asks *what can the process reach if it does something other than what it said?* and is enforced by the kernel.

Both are needed. Policy alone trusts that `make test` does what its name suggests. Containment alone blocks things silently and produces failures the model cannot diagnose.

## Decisions worth knowing about

- **Precedence is `Deny > Ask > Allow`.** A broad `ask(command(*))` beats a narrow `allow(command(git))`. Surprising a user *upward* into less oversight than they configured is worse than an extra prompt. The one exception, documented in `Policy::evaluate`: an interactive grant made this session outranks a configured `ask`, or "remember my answer" would be impossible.
- **`.git/` and `.octane/` are read-only inside writable roots.** Otherwise "write files in the project" transitively grants `.git/hooks/pre-commit` — arbitrary code on the user's next commit — and lets the agent rewrite its own policy.
- **Sandbox paths are passed as `-D` parameters, never interpolated** into the Seatbelt profile. A directory name containing `") (allow default) ("` would otherwise rewrite the policy. There is a test for exactly that.
- **An unsupported platform is a sandbox *error*, not a pass.** Running unconfined because the OS is unfamiliar is the failure a sandbox exists to prevent.
- **Tool schemas live in a `BTreeMap`.** They sit in the cached prompt prefix; hash-map iteration order would reorder them between turns and silently void every cache hit. Codex shipped this bug with MCP tools.
- **The prompt is append-only.** Config changed mid-session? Append a developer message; never edit what was already sent.
- **A denial ends the turn.** Reporting a refusal to the model invites it to find another route to the same action, which turns a clear "no" into a negotiation.
- **The model decides when the task is done.** The step cap, loop detector, and context thresholds are safety rails, not completion criteria.
- **`.gitignore` is honoured even outside a git repo** (`require_git(false)`). `ignore` defaults the other way to match ripgrep's CLI, but an agent gets pointed at extracted archives and vendored subtrees that have a `.gitignore` and no `.git`. Honouring it only sometimes reads as the tool being bad, not as a subtlety.
- **`glob` sorts by mtime, `list` sorts alphabetically.** Recency is the cheapest relevance signal when hunting for a file, but a tree whose branches move between calls is harder to reason about than a stable one.
- **An approval prompt accepts instructions, not just yes/no.** Antigravity's idea, and the best one in any UI surveyed: typing at a prompt rejects the action *and tells the agent what to do differently*. The common case is not "no", it is "no, do it this other way", and a prompt that cannot express that forces the user to reject, wait, and retype their intent.
- **There is no "always allow" key.** A grant that broad should be a deliberate config edit, not one keystroke away from a prompt someone is trying to dismiss.
- **Esc interrupts; it does not exit.** Those are different intentions, and conflating them loses sessions to a reflex. `ctrl+d` exits only on an empty composer for the same reason.
- **Loop detection hashes `(tool, input, output)`.** Including the output is what makes it correct: a repeated call whose output *changed* is progress — polling a build, tailing a log.

## Testing

```bash
cargo test --workspace       # 341 tests
cargo clippy --workspace --all-targets
```

Tests are behavioural and named for the property they protect, e.g. `a_path_with_shell_metacharacters_cannot_reach_the_profile`, `a_session_grant_cannot_survive_a_switch_to_plan`, `changing_output_is_progress_not_a_loop`.

`crates/octane-tools/tests/sandbox_execution.rs` goes further and asserts the security claim itself against the real kernel sandbox — that a write outside the writable roots, to `.git/hooks/`, or to the network does not land. `danger_full_access_is_genuinely_unconfined` is the negative control: the same write succeeds with containment off, which is what makes the other results evidence rather than coincidence. macOS-only, since Seatbelt is the only backend implemented.
