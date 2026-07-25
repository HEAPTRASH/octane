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

## The TUI

Full screen, on the alternate screen: brand header at top, transcript filling the middle, composer and status pinned to the bottom. An empty session shows the hints and a lot of deliberate negative space.

That trades away the terminal's own scrollback, search, and selection (`RESEARCH.md` §H covers the tradeoff — opencode and Amp make the same choice; Claude Code and Codex do not). `octane-tui::transcript` provides scrolling in their place, with the property that matters: **follow-tail**. New content keeps the view pinned to the bottom until you scroll up, then stops, because a pane that moves while you read it is the worst thing a log can do. Scrolling back down resumes following.

Everything except `app` is pure — state in, lines out — because rendering, keybinding, and completion logic is where the bugs are and it should be testable by calling a function.

Frames are wrapped in synchronized-output escapes (`CSI ?2026h`/`l`) so terminals present them atomically instead of tearing, and the region is only redrawn when something changed. That last one is not catchable by a unit test — it renders *correctly* either way, it just repaints ~12 times a second forever. `scripts/tui-smoke.py` measures it on the wire: idle output must be ~0 bytes, not 9.6 KB/s.

### Input

`@path` attaches a file, `!command` runs a shell command, `/` opens commands. Both `@` and `/` complete as you type, with **subsequence matching** rather than prefix — `@octcli` finds `crates/octane-cli/src/main.rs`, because prefix matching would require knowing where a file lives before you can ask for it.

Newlines have three bindings because one is not portable. `shift+enter` is what people reach for, but most terminals send a bare CR for it, indistinguishable from Enter, unless the keyboard enhancement protocol is negotiated — octane requests it at startup, so it works on Kitty, Ghostty, WezTerm, foot, and recent iTerm2. `alt+enter` works essentially everywhere. A trailing `\` before Enter needs no terminal support at all.

## Look and feel

Monster-inspired: black base, acid green (`#95D600`, the brand's own value), high contrast. The startup wordmark animates a charge pulse across itself — two passes, ~600ms — before settling into scrollback, because content committed to scrollback is never redrawn and anything that moves has to move before it lands.

Three colour tiers, detected rather than assumed: exact hexes on truecolor, nearest xterm-256 indices otherwise, and bold/dim only under `NO_COLOR` or `TERM=dumb`. `NO_COLOR` is honoured because people set it for a reason.

Glyphs are chosen for **width safety**, not just availability. A character that renders double-width in one terminal and single in another silently destroys every box and aligned column, and the breakage is invisible until someone reports it from a terminal you don't have. Everything comes from ranges that are unambiguously narrow — box drawing, block elements, geometric shapes, braille, arrows. Emoji, Nerd Font glyphs, and emoji-presentation symbols like `⚡` and `✔` are excluded; `✓` is text presentation and safe, its neighbour is not. A full ASCII set covers terminals without dependable Unicode, and it reaches the transcript and status line, not just the banner.

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
cargo test --workspace       # 417 tests
cargo clippy --workspace --all-targets
python3 scripts/tui-smoke.py # drives the real TUI through a pty
```

Tests are behavioural and named for the property they protect, e.g. `a_path_with_shell_metacharacters_cannot_reach_the_profile`, `a_session_grant_cannot_survive_a_switch_to_plan`, `changing_output_is_progress_not_a_loop`.

`crates/octane-tools/tests/sandbox_execution.rs` goes further and asserts the security claim itself against the real kernel sandbox — that a write outside the writable roots, to `.git/hooks/`, or to the network does not land. `danger_full_access_is_genuinely_unconfined` is the negative control: the same write succeeds with containment off, which is what makes the other results evidence rather than coincidence. macOS-only, since Seatbelt is the only backend implemented.
