# How AI Coding CLIs Actually Work

Research notes for building `octane-agent`. Covers **Claude Code**, **Antigravity CLI**, **Codex CLI**, **opencode**, and **Crush** — what each one is, how it's built, and the subsystem-by-subsystem design space you have to make decisions in.

Researched 2026-07-25 via Firecrawl (search + scrape) plus direct repo-structure inspection. Sources at the bottom. Provenance caveats are flagged inline where a claim comes from third-party reverse engineering rather than official docs.

---

## 0. The one-paragraph summary

All five are the same shape: **a harness wrapped around a tool-calling LLM**. The model is the brain; the harness is everything else — a loop that calls the model, executes the tools it asks for, feeds results back, and repeats until the model stops asking. The loop itself is trivial (~50 lines). *Everything* that makes these products good is the surrounding machinery: prompt assembly and cache discipline, the tool suite and its descriptions, the permission engine, context compaction, subagent isolation, session persistence, and the TUI. Competitive advantage lives in the harness, not the loop.

---

## 1. The canonical agent loop

Every one of these tools implements the same cycle. Codex calls it the agent loop, Claude Code's is described as **TAOR** (Think → Act → Observe → Repeat), opencode's is `SessionPrompt.loop()`, Crush's is `fantasy.Agent.Stream`.

```
user input
   │
   ▼
build prompt  ──►  system instructions + tool defs + history + new message
   │
   ▼
call model (streaming)
   │
   ├── text deltas ────────────────► render to UI, persist
   ├── reasoning deltas ───────────► render (collapsed), persist
   └── tool call
         │
         ▼
      permission check (allow / ask / deny)
         │
         ▼
      execute tool (sandboxed)
         │
         ▼
      append result to history ──────► loop back to "call model"
   │
   ▼
finish_reason != tool_calls  ──►  turn ends
```

Three things worth internalizing:

**The exit condition is the model, not your code.** You don't decide when the task is done. You loop while `finish_reason == "tool_calls"` and stop when the model emits a plain assistant message. Your only hard stops are safety rails: a max-step cap (opencode: `steps.length >= 1000`), a loop detector, or a permission rejection.

**The prompt is append-only and grows every iteration.** Full history must be resent each call, so a conversation is *O(n²) in bytes transmitted*. This single fact drives the design of prompt caching and compaction. It is not an optimization — it's a correctness and cost constraint you design around from day one.

**Cost and latency are dominated by turn count, not token count.** Every tool call is a full round trip through inference. This is why parallel tool calls, batched reads, and subagent delegation matter so much.

### Loop safety rails

Crush ships an explicit loop detector worth copying (`internal/agent/loop_detection.go`): it hashes `(tool_name, input, output)` for each of the last 10 steps and aborts if any signature repeats more than 5 times. Cheap, and it catches the classic "agent reads the same file forever" failure.

```go
const (
    loopDetectionWindowSize = 10
    loopDetectionMaxRepeats = 5
)
// signature = sha256(toolName \0 input \0 output \0 ...) over the trailing window
```

---

## 2. The five tools

### Claude Code — Anthropic

| | |
|---|---|
| Language | TypeScript on Bun, shipped as a bundled npm package |
| Source | Closed (a March 2026 sourcemap leak produced extensive third-party analysis) |
| UI | Custom React Fiber reconciler rendering to the terminal |
| Extension model | Declarative: `CLAUDE.md`, Skills, Subagents, Hooks, MCP, Plugins — all markdown/JSON, no code |

**The defining idea: declarative extensibility.** Almost every capability is added by dropping a markdown file somewhere. Skills are prompt macros, subagents are markdown files with YAML frontmatter, hooks are shell commands bound to lifecycle events, memory is `CLAUDE.md` files layered org → project → user. Non-engineers can extend it. This also doubles as demand-sensing: what people build declaratively tells you what to ship natively.

**Permission engine (7 stages).** Per third-party analysis, every tool call runs a rule cascade — deny rules, ask rules, per-tool `checkPermissions()`, content-specific rules, then a safety check that *no* bypass mode can skip — before falling through to bypass mode, an auto-mode classifier, and finally manual prompting. Six permission modes: `default`, `plan`, `acceptEdits`, `bypassPermissions`, `dontAsk`, `auto`.

**The auto-mode classifier is a second LLM judging every command.** Two stages: a 64-token binary fast path at temperature 0 using stop sequences to force `<block>yes|no</block>`, escalating to 4096-token full reasoning only when the fast path says "escalate". Results cached ~1h with a claimed 60–80% hit rate. It gives up after 3 consecutive or 20 total denials. Notably, entering auto mode *strips* your previously granted allow-rules for process spawners (python, node, bash, ssh, sudo, eval, exec) — it doesn't trust your past approvals.

**The bash parser is the most copyable piece of security engineering here.** ~4,400 lines of hand-rolled recursive descent, fail-closed and allowlist-based: any unrecognized AST node → "too complex" → ask the user. It detects 15 categories of dangerous AST nodes (command substitution, process substitution, subshells, loops, function defs), blocks 35+ dangerous builtins (`eval`, `source`, `exec`, `trap`, all 18 dangerous zsh builtins), and runs pre-AST checks for control characters, Unicode whitespace attacks, zsh `=curl` equals-expansion, and array-subscript arithmetic RCE (`test -v 'a[$(id)]'`). Resource-bounded: 50ms timeout, 50K node budget.

**Six-tier compaction** (see §4). **File-based multi-agent IPC**: parallel agents coordinate through JSON mailboxes at `~/.claude/work/ipc/` with 500ms polling, spawned into tmux panes, iTerm2 splits, or in-process. Deliberately dumb, works everywhere, and the cited analysis counts 13 race conditions as the price.

**Terminal rendering:** React → custom reconciler → Yoga flexbox (reimplemented in TS to avoid native deps) → Int32Array screen buffer (21 bits codepoint, 4+4 bits color, 3 bits style) → frame diffing → ANSI. 10 FPS cap; idle scroll drops from ~10KB to ~50 bytes of output.

---

### Codex CLI — OpenAI

| | |
|---|---|
| Language | Rust (`codex-rs`, ~150 crates) |
| Source | Open, Apache-2.0 |
| UI | Ratatui TUI (`codex-rs/tui`) |
| Architecture | Shared **core** library + **App Server** JSON-RPC protocol |

**The defining idea: one harness, many surfaces.** CLI, web, VS Code, macOS app, JetBrains, and Xcode all run the same Rust `core` — agent loop, thread lifecycle, config/auth, sandboxed tool execution. Features ship once and appear everywhere.

**The App Server is the piece to steal if you ever want more than a TUI.** A long-lived process hosting core threads, exposed over bidirectional JSON-RPC as JSONL over stdio. Four internal parts: stdio reader (transport), message processor (translates RPC → core ops, and low-level internal events → stable UI-ready notifications), thread manager (one core session per thread), and the threads themselves.

They **tried MCP for this first and it didn't work** — MCP semantics couldn't represent rich session state like streaming diffs and progress. So they designed a custom protocol with an explicit backward-compatibility guarantee: old clients talk to new servers safely, which is how Xcode pins a stable client while pointing at a newer server binary.

**Three protocol primitives, each with an explicit lifecycle:**

- **Item** — atomic I/O unit (user message, agent message, tool execution, approval request, diff). Lifecycle: `item/started` → optional `item/*/delta` stream → `item/completed`. Clients render on `started` without waiting for content.
- **Turn** — one unit of agent work; begins on client submit, ends when all outputs for that input are done. Contains many Items and many inference cycles.
- **Thread** — durable session container. Holds turns, persists event history to disk, supports create/resume/fork/archive. The server can pause a turn mid-execution to request approval and blocks until the client answers.

**Prompt structure is explicitly ordered for cache stability.** `instructions` (model-specific, e.g. `gpt-5.2-codex_prompt.md`) and `tools` first, then `input` in this order: developer message describing sandbox permissions and writable folders → optional user config from `~/.codex/config.toml` → `AGENTS.md` files aggregated from git root down to cwd → environment context (cwd, shell) → the actual user message. Role hierarchy `system > developer > user > assistant`.

**Cache discipline is treated as a correctness constraint.** Since the prompt is append-only, each turn shares a prefix with the last, so server-side prefix caching turns quadratic cost into near-linear. But an exact-prefix mismatch poisons everything after it, so: sandbox config changes are *appended* as a new developer message rather than editing the earlier one; cwd changes append a new `environment_context`. They shipped a real bug where MCP tool definitions were emitted in nondeterministic order and blew the cache on every single turn.

**Model-native compaction.** When tokens exceed `auto_compact_limit`, Codex calls a dedicated `/responses/compact` endpoint that returns a smaller input list including a `type=compaction` item carrying an opaque `encrypted_content` blob — the model's latent understanding of the conversation, richer than any text summary. For zero-data-retention customers OpenAI holds the decryption key, not the data. Codex deliberately does *not* use `previous_response_id`, keeping requests stateless for ZDR.

**Sandboxing is per-OS and native** (visible as crates): `linux-sandbox` (Landlock + seccomp), `windows-sandbox-rs` (AppContainer), plus a general `sandboxing` crate and `execpolicy` for command policy. Sessions persist as "rollouts" (`rollout.rs`, `thread-store`). Also present: `skills`, `hooks`, `core-plugins`, `memories`, `agent-graph-store`, `collaboration-mode`, `network-proxy`, `code-mode`.

---

### opencode — anomalyco (formerly sst)

| | |
|---|---|
| Language | TypeScript on Bun; server is Hono |
| Source | Open, ~190K stars — the most-starred of the group |
| UI | **OpenTUI** — custom framework, Zig core + SolidJS |
| Architecture | HTTP server + SSE, multi-client |

> Note: `sst/opencode` now redirects to `anomalyco/opencode`. Also: **this is a different project from Crush**, despite the shared history of the name. Different language, different lineage.

**The defining idea: client/server from day one.** Running `opencode` starts a Hono HTTP server *and* a client. The server holds all agent logic; the TUI is just one client. A REST API (OpenAPI 3.1, published at `/doc`) plus an SSE event stream at `/global/event` means a web UI, mobile app, VS Code extension, or shell script are all first-class. `opencode serve` runs headless and you attach from anywhere. Client SDKs are generated from the OpenAPI spec via Stainless, so the TUI talks to the server through the same typed SDK third parties get.

**Provider-agnostic via the Vercel AI SDK.** 75+ providers behind one interface. Resolution is a hierarchy: model = CLI `--model` → config file → last-used from KV store → default ranking. Credentials = env vars → `~/.local/share/opencode/auth.json` → config → models.dev defaults. Four auth styles: API keys, OAuth (Copilot, GitLab Duo, Claude Pro/Max), cloud creds (Bedrock, Vertex), and unauthenticated local endpoints (Ollama, LM Studio, vLLM). Pricing data from **models.dev** lets it compute per-run cost from streamed usage stats.

**`ProviderTransform` is the pattern to copy.** All provider quirks are isolated in one namespace, outside the loop: Anthropic needs empty content filtered, Mistral needs tool IDs normalized to 9 alphanumeric chars, Anthropic/Bedrock/OpenRouter need cache-control headers injected. The loop body only knows "send messages, get response".

**Messages are arrays of polymorphic `Part`s** — `text`, `tool-invocation`, `reasoning`, `file`, `image`, `agent`. Each has its own schema and rendering logic, so adding a content type touches nothing existing. Tool parts carry state (`pending` → `executing` → `completed`/`error`) streamed live over SSE.

**`Database.effect` solves a race you will otherwise hit.** Per-project SQLite (`~/.local/share/opencode/project/<hash>/data.db`, Drizzle ORM, tables: sessions, messages, parts, permissions, mcp_servers). All mutations run inside a transaction; event emission is *scheduled* via `Database.effect()` and fires **only after the transaction commits**. Without this, clients receive an event and then can't find the row.

**Git-based snapshots for undo.** At every `start-step` it captures a tree without touching history:

```
git --git-dir <private> add .          → git write-tree   # snapshot
git read-tree <hash> && checkout-index -a -f              # restore
```

**Two-tier agents.** Primary (`build` full access, `plan` read-only — Tab to switch) and subagents (`general`, `explore`) invoked via the `task` tool, plus hidden agents for `compaction` and session `title` generation. Switching plan → build injects a synthetic `<system-reminder>` telling the model its mode changed and write tools are now live.

**~14 built-in tools** across five categories: files (`read`/`write`/`edit`/`patch`), search (`grep` via ripgrep, `glob` sorted by mtime, `list`), execution (`bash` in a pty, `task`), knowledge (`skill`, `webfetch`, `websearch` via Tavily, `lsp`), interaction (`question`, which blocks for user input). Custom tools = drop a `.ts` file in `.opencode/tools/`; filename becomes tool name and can override a builtin.

**Plugin system with 20+ hooks** chained in a pipeline (each plugin transforms the previous one's output): `chat.params`, `chat.messages.transform`, `llm.stream.before/after`, `tool.execute.before/after`, `message.updated`, `session.compaction`, `shell.env`. Plugins get project info, an SDK client, and a `$` shell API.

**OpenTUI** exists because they wanted 60 FPS. TypeScript layer (SolidJS reconciler via `solid-js/universal`, no virtual DOM; Yoga flexbox; Renderables for Box/Text/EditBuffer/Code-with-Tree-sitter/Diff/ScrollBox) over a Zig core loaded via Bun `dlopen()` FFI doing frame diffing, RLE-encoded ANSI generation, and rope text buffers. Sub-millisecond frames. The transferable lesson isn't "write Zig" — it's "write only the hot path natively."

---

### Crush — Charm

| | |
|---|---|
| Language | Go |
| Source | Open, ~27K stars |
| UI | Bubble Tea / Lip Gloss / Glamour, moving to Ultraviolet |
| LLM layer | **`charm.land/fantasy`** — Charm's own Go agent SDK |

Started as Kujtim Hoxha's Go coding agent, adopted by Charm in July 2025. Charm's five years of TUI tooling is the whole point: this is the best-looking one, and "glamorous" is a real product thesis, not a joke.

**`fantasy` is the AI-SDK-equivalent** ("Build AI agents with Go. Multiple providers, multiple models, one API"). It provides `Agent.Stream` with the callback surface you'd design yourself: `PrepareStep`, `OnToolCall`, `OnToolResult`, `OnReasoningStart/End`, `OnStepFinish`, `OnRetry`, a `ModelProvider` thunk for hot model swapping, and a `StopWhen []StopCondition` list.

**Mid-session model switching while preserving context** is a headline feature, and it's why `ModelProvider` is a function rather than a value. If you're building provider-agnostic, design for this early — it's much harder to retrofit.

**Auto-summarize is wired into `StopWhen`**, not a separate pass: a stop condition checks remaining context against a threshold and sets `shouldSummarize`, letting the loop exit cleanly and compact before continuing.

**Structure** (`internal/`): `agent` (with `coordinator.go` for multi-agent, `loop_detection.go`, `hooked_tool.go`, `hyper`), `backend`, `server` + `proto` + `client` + `swagger` (it has also grown a client/server split with a generated API), `lsp`, `permission`, `session`, `skills`, `hooks`, `projects`, `workspace`, `shell`, `diff`, `diffdetect`, `history`, `pubsub`, `oauth`, `db`.

**Deep LSP tooling — the most of any tool here.** Beyond diagnostics: `lsp_definition`, `references`, `lsp_symbols`, `lsp_call_hierarchy`, `lsp_rename`, `lsp_replace_symbol`, `lsp_restart`. `lsp_replace_symbol` is a genuinely good idea: edit by semantic identity rather than by text match. Also ships `sourcegraph.go` for cross-repo search and `job_kill`/`job_output` for background process management.

---

### Antigravity CLI — Google

| | |
|---|---|
| Binary | `agy` |
| UI | Keyboard-driven TUI |
| Config | `~/.gemini/antigravity-cli/settings.json` |
| Architecture | Shared agent harness with Antigravity 2.0 (IDE) |

Same structural bet as Codex: **one agent core, multiple surfaces** (CLI, Antigravity 2.0 desktop, IDE, SDK). Settings, permissions, and security config sync automatically across surfaces, and you can **export a conversation from the terminal into the visual editor** mid-task. Migrates Gemini CLI extensions/skills/settings on first run.

**Three execution modes, cycled with Shift+Tab:** `default` (interactive syntax-highlighted diff review before any write), `accept-edits` (auto-approve file writes), `plan` (prepends a `/plan` prefix; agent investigates with read-only tools `code_search`, `grep_search`, `view_file` and presents an outline for approval). Persisted as `agentMode` in settings, overridable with `--mode`.

The diff-review UX is worth stealing: on a pending write you get `y` accept / `n` reject / `f` full-screen scrollable diff with 3 context lines / `Ctrl+G` open in `$EDITOR` — **or just type instructions**, which rejects the edit and tells the agent what to do differently. That last option turns a permission prompt into a steering opportunity.

**Permissions are `action(target)` resources** across `allow`/`deny`/`ask` lists, precedence strictly **Deny > Ask > Allow**:

| Action | Target | Matching | Default |
|---|---|---|---|
| `read_file` | path / `*` | absolute or workspace-relative, recursive | Ask (auto-allow in workspace) |
| `write_file` | path / `*` | same; implicitly grants `read_file` on that path | Ask (auto-allow in workspace) |
| `read_url` | domain / `*` | hostname + subdomains, ignores path | Ask |
| `execute_url` | domain / `*` | clicking/typing in browser flows | Ask |
| `command` | prefix / regex / `*` | per-token anchored regex `^(?:pattern)$` | Ask |
| `unsandboxed` | prefix / `*` | grants execution *outside* the sandbox | Ask |
| `mcp` | `server/tool` / `*` | exact tool or whole server | Ask |

Two implicit rules that are obviously right and easy to forget: **write implies read**, and **denying read implies denying write**. Windows paths are normalized (drive letters stripped, backslashes converted) before rule evaluation. And on an Ask prompt you can **edit the target string inline** to widen scope — broadening `/project/file.txt` to `/project` for the rest of the turn — with validation that the edit still covers the request. That kills prompt fatigue without a blanket allow.

**Native OS sandboxing, no containers** (`enableTerminalSandbox`): `nsjail` on Linux (namespaces + cgroups), `sandbox-exec` on macOS, `AppContainer` on Windows. The approval prompt adapts to state — sandbox on gives you "Yes, and run without sandbox restrictions"; sandbox off gives you "Yes, and run in sandbox." Per-invocation escape hatches in both directions.

**Async-first subagents.** Long operations are delegated to parallel background subagents so the terminal never blocks; you keep prompting while they run. A `/agents` Agent Manager panel shows a live list (ID, role, state: running/done/killed/error, current step), and `Enter` opens a **Subagent Detail View** exposing that agent's full reasoning log and tool calls. `/tasks` tracks non-agentic background processes with stdout logs and safe termination.

The two keybindings solving the real problem with background agents — *they need approvals while you're busy*:
- **`Alt+J`** — "teleport" from your conversation straight into the detail view of the next subagent awaiting approval; `Esc` teleports back.
- **`Ctrl+K`** — approve the pending action from an inline status notification (`Subagent 12 asks to run "npm test"`) without leaving your prompt.

Custom agents are markdown + YAML frontmatter in `.agents/agents/<name>.md` or `~/.gemini/config/agents/`; `subagent: true` makes one invocable via `invoke_subagent`. Subagents inherit the parent's `accept-edits` setting so background writes don't silently queue for approval.

Antigravity's other distinctive concept is **Artifacts** — the agent produces reviewable work products (implementation plans, walkthroughs, screenshots, browser recordings) as first-class objects, not just chat messages.

---

## 3. The standard tool suite

Remarkably convergent across all five. If you ship these, you have a coding agent:

| Tool | Notes on what makes it non-trivial |
|---|---|
| `read` | Absolute paths only; line-numbered output (`cat -n` style); default ~2000-line limit; offset/limit params; reject binary and image files with a *useful* error; truncate long lines; tell the model when more lines remain. opencode and Crush also attach **LSP diagnostics** to the output. |
| `write` | Usually requires having read the file first. |
| `edit` | Search/replace on exact strings — the highest-value and highest-failure tool. Variants: `multiedit`, `patch` (batch), and Crush's semantic `lsp_replace_symbol`. |
| `bash` | Persistent shell session, timeout, output cap, streaming stdout/stderr to the UI, and a required `description` argument (5–10 words) purely so the UI and permission prompt can show intent. The security surface. |
| `glob` | Fast pattern match, results sorted by mtime (recency is a relevance signal). |
| `grep` | ripgrep under the hood, respects `.gitignore`, file-type filters. |
| `list` | Tree listing with ignore patterns. |
| `todowrite` / `todoread` | Per-session todo list. Trivially implemented (a map keyed by session ID) and disproportionately effective: it's the model's own scratchpad against context rot, and the UI checkboxes are most of the perceived "intelligence". |
| `task` | Spawn a subagent. Its *description* is the dynamically generated list of available agents. |
| `webfetch` / `websearch` | Fetch + HTML→markdown, plus a search provider. Claude Code restricts fetches to hosts the user mentioned or that appear in project files. |
| `lsp` | Definitions, references, hover, symbols, call hierarchy. |
| `question` | Blocks and asks the user. Underrated — gives the model a way to stop guessing. |
| MCP tools | Registered into the same registry; indistinguishable from builtins to the model. |

**The tool description is the actual interface.** These descriptions are hundreds of words and contain the behavioral contract: "always use absolute paths", "speculatively read multiple files in a batch", "execute independent calls in parallel", "quote paths with spaces", "verify the parent directory exists before mkdir". Tuning descriptions is prompt engineering with a much better feedback loop than tuning the system prompt.

**Tool definition pattern**, identical in shape everywhere — name, description, schema (Zod/JSON Schema), execute function, plus a context object carrying session ID, message ID, agent name, abort signal, and a permission callback:

```ts
Tool.define("read", {
  description: DESCRIPTION,                       // long, prescriptive
  parameters: z.object({ filePath: z.string(), offset: z.number().optional() }),
  async execute(params, ctx) { /* ctx: sessionID, messageID, agent, abort, ask */ },
})
```

**Execution pipeline** (opencode's, representative):

```
LLM emits tool call → registry lookup → permission check (allow/deny/ask)
  → plugin pre-hook → execute() → plugin post-hook → DB commit → SSE event
```

---

## 4. Context management

The hardest constraint, and where the tools differ most. Four strategies, roughly in order of quality:

**1. Pruning (free, no API call).** Drop old *tool outputs* while protecting recent context. opencode: protect the last **40K tokens** (`PRUNE_PROTECT`), only prune tool outputs older than that exceeding **20K tokens** (`PRUNE_MINIMUM`). Both knobs matter — too small a protection window and the agent repeats work it already did; too small a minimum and you thrash. `skill` tool output is exempt, because losing project rules mid-conversation visibly degrades quality. Claude Code's Tier 1 equivalent clears tool results older than 60 minutes of cache expiry, keeping the last 5.

**2. Cache-preserving surgical removal.** Claude Code Tier 2 uses an Anthropic `cache_edits` API parameter to remove tool results *without invalidating the cached prompt prefix*. The distinction matters enormously: naive pruning saves tokens but destroys your cache, which can cost more than it saves.

**3. Summarization.** opencode summarizes when `tokens > (context_limit - output_limit) * 0.9`, with a hidden `compaction` agent producing a structured summary (goal, user instructions, discovered facts, completed work, relevant files) that replaces the old history. Claude Code's Tier 5 forks a subprocess that *shares the parent's prompt cache* (~76% cheaper) and emits a 9-section narrative: primary request/intent, key technical concepts, files and code sections with snippets, errors and fixes, problem solving, **all user messages**, pending tasks, current work, optional next step. After compaction it restores up to 5 recently-read files (50K budget, 5K/file) and re-injects active skills (25K budget). Retries up to 3× truncating from the head if the summary itself overflows.

**4. Model-native compaction.** Codex's `/responses/compact` returning an encrypted latent-state blob. Best quality, but only available if you control the inference endpoint. Not an option for a third-party harness.

Claude Code's reported thresholds are a good starting point:

```js
effectiveContextWindow = contextWindow - min(modelMaxTokens, 20_000)
autoCompactThreshold   = effectiveContextWindow - 13_000   // ~167K on a 200K model
warningThreshold       = effectiveContextWindow - 20_000    // ~160K
```

**Have a circuit breaker.** Claude Code stops attempting compaction after 3 consecutive failures and accepts being over-limit rather than looping forever. Also keep a reactive path: catch `prompt_too_long` from the API and truncate oldest-first while **preserving tool_use/tool_result pairing** — orphaning either half produces API errors.

### Prompt caching (the thing that makes it affordable)

Layer the prompt by **stability, most static first**:

```
[ system instructions      ]  ← never changes         ┐
[ tool definitions         ]  ← stable, ORDER-STABLE  │ cached prefix
[ sandbox/permission ctx   ]  ← rarely changes        │
[ project memory (AGENTS/  ]  ← per session           ┘
[   CLAUDE.md)             ]
[ conversation history     ]  ← append-only
[ new user message         ]  ← the only new bytes
```

Rules that fall out of this:
- **Never insert or edit earlier content.** Config changed mid-session? *Append* a new developer message.
- **Sort your tool definitions deterministically.** MCP servers returning tools in map order will silently cost you every cache hit.
- Claude Code reportedly places a **deliberate cache-busting boundary after MCP instructions** — 7 static sections share one cache block, 13 dynamic sections go after — so adding an MCP server invalidates only the tail.
- Anthropic/Bedrock/OpenRouter need explicit `cache_control` breakpoints injected; OpenAI caches automatically. This belongs in your provider transform layer.

---

## 5. Permissions and sandboxing

Two independent layers. Don't conflate them.

**Layer 1 — policy: allow / ask / deny.** Universal across all five. Glob or prefix matching over `action(target)` resources.

```json
{ "permission": {
    "read":  { ".env": "deny", "**": "allow" },
    "bash":  { "rm -rf *": "deny", "git *": "allow", "**": "ask" },
    "edit":  { "**": "allow" }
}}
```

Decisions you have to make explicitly:
- **Precedence.** Antigravity: strict `Deny > Ask > Allow`, so `ask: command(*)` overrides `allow: command(git)`. opencode: **last matching glob wins**, evaluated session-level → agent-level → global. These give opposite answers on the same config. Pick one and document it.
- **Scoping.** opencode persists grants to the DB so you're not re-asked. Antigravity grants for the remainder of the turn, and lets the user *edit the target* on the prompt to widen scope. Both beat re-prompting.
- **Modes as presets.** `plan` (read-only), `default` (ask), `accept-edits`, `bypass`. Shift+Tab to cycle is now the de facto standard. Every tool ships this.
- **Implicit rules.** Write implies read; deny-read implies deny-write.
- Agents constrain tools too: `plan` simply doesn't get `edit`/`write`.

**Layer 2 — containment: OS sandboxing.** Everyone converged on native kernel facilities over containers:

| OS | Mechanism | Used by |
|---|---|---|
| Linux | `nsjail` (namespaces + cgroups) / Landlock + seccomp | Antigravity / Codex |
| macOS | `sandbox-exec` (Seatbelt profiles) | Antigravity, Codex |
| Windows | `AppContainer` | Antigravity, Codex (`windows-sandbox-rs`) |

Near-zero overhead, no daemon. Give yourself a per-command escape hatch in both directions (Antigravity's "yes, but unsandboxed" / "yes, but sandboxed") and a permission action (`unsandboxed(prefix)`) for the commands that genuinely need out, like `git push`.

**Layer 3 — command analysis, if you want to be serious.** Claude Code's fail-closed bash AST parser (§2). The principle generalizes: *parse, don't pattern-match*. Regex-based command blocklists are trivially bypassed; an AST that rejects anything it doesn't recognize is not.

---

## 6. Subagents and parallelism

Three levels of isolation, each with a different cost/benefit:

| Level | Isolation | Cost | Used for |
|---|---|---|---|
| **Skill / prompt macro** | none — same context | ~free | injecting procedures, project rules |
| **Subagent** | own context window, own tools, own model, own session | one extra inference chain | research, search sweeps, review — anything whose *process* you don't want in your main context |
| **Parallel peer agents** | separate processes | high | multi-module implementation (Claude Code "agent teams", Codex `collaboration-mode`, Crush `coordinator.go`) |

**Subagents are the primary answer to context collapse.** A search sweep that would burn 40K tokens of your main window returns as a 500-token summary. The mechanics are simple: `task` is a normal tool whose description is the generated list of available agents, and whose `execute` creates a fresh session with that agent's prompt/tools/model, runs the loop, and returns the final text.

Design constraints all five share:
- **Stateless and one-shot.** No back-and-forth. The prompt must be fully self-contained and must state exactly what to return. Say explicitly whether you want research or code.
- **Subagents don't get `task`** (no unbounded recursion) and typically don't get todo tools.
- **The result is invisible to the user** — the parent must summarize it.
- Defined declaratively as markdown + YAML frontmatter (`description`, `mode`, `model`, `temperature`, `tools: {write: false}`) in a conventional directory. Same format in Claude Code, opencode, and Antigravity.

**The unsolved-ish problem is approvals for background agents.** If a subagent needs permission while you're typing, naive designs either block your terminal or silently queue. Antigravity's `Alt+J` teleport and `Ctrl+K` inline fast-path approval are the best answers I found. Claude Code routes them through a UI bridge with a mailbox fallback.

---

## 7. Session persistence

| | opencode | Codex | Claude Code |
|---|---|---|---|
| Store | per-project SQLite (Drizzle) | rollout files + thread-store | JSONL transcripts |
| Granularity | sessions / messages / **parts** | Items within Turns within Threads | messages |
| Branching | `Session.fork()` copies history to a message, keeps `parentID` | thread fork | checkpoint / rollback / fork |
| Undo | git `write-tree` snapshot per step | `turn_diff_tracker` | checkpoints |

Two things everyone does that you should copy:

**Persist at part granularity, and stream state transitions.** A tool call is a persisted record moving `pending → executing → completed|error`, each transition emitted as an event. That's what makes the UI feel alive, and it's what lets a client reconnect and rebuild the exact timeline.

**Snapshot the working tree per step.** opencode's `git write-tree` trick gives you free undo without polluting history and without a custom VCS.

---

## 8. Extensibility surfaces

Five distinct mechanisms, all present in at least three of the tools. They're not redundant — they sit at different points on the isolation/determinism spectrum:

| Surface | Form | Runs | Purpose |
|---|---|---|---|
| **Memory files** | `AGENTS.md` / `CLAUDE.md` | injected in prompt | project ground truth, layered org → project → user |
| **Skills** | markdown, loaded on demand | in-context | procedures, prompt macros. Protect their output from pruning. |
| **Slash commands** | markdown with args | expands to a prompt | canned workflows |
| **Subagents** | markdown + YAML frontmatter | own context | delegation |
| **Hooks** | shell commands on lifecycle events | **deterministically, no LLM** | lint on save, audit on shell, gate on deploy |
| **MCP** | stdio / HTTP / SSE servers | external process | third-party tools + resources |
| **Plugins** | code with hook registrations | in-process | everything else |

**Hooks are the most underrated.** Anything you want *guaranteed* should not go through the model. "Run the formatter after every edit" as a prompt instruction is ~90% reliable; as a post-edit hook it's 100%.

**MCP integration details worth knowing:** local servers are spawned as subprocesses with a command array + env; remote servers connect over HTTP/SSE and, on a 401, opencode automatically initiates OAuth Dynamic Client Registration (RFC 7591). MCP tools land in the same registry as builtins and go through the same allow/deny/ask engine — that's what makes them safe to add. Two operational gotchas: sort the tool list deterministically (cache), and MCP could not express rich session state well enough to serve as Codex's client protocol, so don't plan on MCP as your *own* UI transport.

---

## 9. Architecture decisions for `octane-agent`

The repo is empty, so these are all still open. Here's the decision space with what I'd pick and why.

**1. In-process TUI, or client/server?**
Codex, opencode, and Antigravity all converged on a **core/server + protocol + thin clients** split, and Codex's CLI is explicitly the *laggard* they plan to refactor onto the App Server. opencode's takeaway is blunt: separate UI from agent logic early, because retrofitting is expensive. Two viable transports — HTTP + SSE (opencode: browser-friendly, OpenAPI-generatable SDKs) or JSON-RPC over stdio (Codex: bidirectional, trivial to embed as a child process, language-agnostic). **Recommendation: build the core as a library with an event stream from commit one, even if you only ship a TUI for months.** Don't put loop logic in UI code.

**2. Language.** The field: Rust (Codex), Go (Crush), TypeScript/Bun (Claude Code, opencode). Go buys you Bubble Tea/Lip Gloss/Glamour and `fantasy` for free plus single-binary distribution; TS buys you the AI SDK and the largest MCP/tooling ecosystem; Rust buys you the sandboxing crates and startup time. All three are proven. Pick on your own fluency — the harness design transfers.

**3. Provider abstraction.** Use an existing agent SDK (Vercel AI SDK, `charm.land/fantasy`) rather than writing streaming clients per provider. Then add your own transform layer for quirks. Design for **mid-session model switching** (a model *provider function*, not a model value) and pull pricing from models.dev so you can show cost.

**4. Tools.** Ship the ~12 primitives in §3 and stop. Every source independently concludes that a small set of capability primitives plus `bash` beats many specialized tools — `bash` is the universal adapter that gives you git, npm, docker, and everything else for free. Budget real time for tool *descriptions*; they're where behavior actually lives.

**5. Permissions.** Non-negotiable, and it's the difference between demo and product. Minimum viable: `allow/ask/deny` with glob matching, the four mode presets with Shift+Tab cycling, persisted grants, and inline diff review before writes. Add native OS sandboxing next. Add bash AST analysis when you have users who aren't you.

**6. Context.** Build pruning-with-protection-window *and* summarization from the start, with a circuit breaker. Structure the prompt static-first and make the tool list order-stable on day one — cache discipline is nearly free if you design for it and painful to retrofit.

**7. Persistence.** Per-project SQLite, parts-level records, events emitted post-commit (`Database.effect`), git `write-tree` snapshots per step for undo, session fork.

**8. Extensibility.** Markdown-and-YAML declarative surfaces (memory, skills, subagents, commands) plus hooks plus MCP. Note that Claude Code's extension model is *entirely declarative* and that's cited as a major adoption driver — no plugin API required to get started.

**9. What not to build early.** A custom TUI framework (opencode's Zig core and Claude Code's React reconciler are both late-stage optimizations for 60 FPS streaming — Bubble Tea or Ink is fine); model-native compaction (needs your own inference endpoint); parallel peer agents (subagents cover most of the value at a fraction of the complexity, and Claude Code's swarm IPC reportedly carries 13 race conditions).

### The one-line thesis, from the Claude Code analysis

> *The harness matters more than the model.* The permission pipeline, the compaction algorithm, the streaming tool executor, the bash parser — the model is interchangeable, the harness is not.

Corollary that shows up in multiple sources: **delete scaffolding as models improve.** If your harness gets more complex with every model release, the architecture is wrong.

---

## Sources

**Codex**
- OpenAI, *Unrolling the Codex agent loop* — via analysis at https://swequiz.com/articles/openai-codex-architecture (original: https://openai.com/index/unrolling-the-codex-agent-loop/)
- https://blog.bytebytego.com/p/how-openai-codex-works
- `openai/codex` repo structure (`codex-rs/*`, `codex-rs/core/src/*`), GitHub API

**opencode**
- https://cefboud.com/posts/coding-agents-internals-opencode-deepdive/ — best source in this batch; walks real source for tools, loop, TUI, snapshots
- https://gist.github.com/shibuiwilliam/1d1466b24cb5c8f0d9367f2c75c9c064 — "10 Design Elements Every Coding Agent Developer Should Know"
- https://opencode.ai/docs/agents/

**Crush**
- `charmbracelet/crush` repo structure + `internal/agent/agent.go`, `internal/agent/loop_detection.go`
- `charmbracelet/fantasy` (`charm.land/fantasy`)
- https://charm.land/blog/crush-comes-home/

**Antigravity CLI**
- https://antigravity.google/docs/cli/overview, `/modes`, `/subagents`, `/permissions`, `/sandbox`

**Claude Code**
- https://karanprasad.com/blog/how-claude-code-actually-works-reverse-engineering-512k-lines + archive at https://github.com/thtskaran/claude-code-analysis — third-party analysis following the March 2026 sourcemap incident. Detailed and internally consistent, but unofficial: treat specific numbers as indicative.
- https://vrungta.substack.com/p/claude-code-architecture-reverse — TAOR loop, layered memory, isolation spectrum
- https://kirshatrov.com/posts/claude-code-internals — mitmproxy prompt capture (2025, older version, but the only source showing actual wire format)
- https://southbridge-research.notion.site/claude-code-an-agentic-cleanroom-analysis — cleanroom analysis, not yet read

---

# Addendum: subsystem specifications

Implementation-level detail for the seven subsystems `octane-agent` is being built around. Researched separately from the survey above; these are the specs I'm coding against.

## A. Permissions — the policy engine

**Resource grammar.** Every sensitive operation is a string `action(target)`. Antigravity's action set, which generalizes well:

| Action | Target | Match |
|---|---|---|
| `read_file` | path / `*` | absolute or workspace-relative, recursive over the subtree |
| `write_file` | path / `*` | same; **implicitly grants `read_file`** on that path |
| `read_url` | domain / `*` | hostname + subdomains; path ignored |
| `command` | prefix / regex / `*` | per-token anchored regex `^(?:pattern)$` |
| `unsandboxed` | prefix / `*` | run outside containment (needed for `git push` etc.) |
| `mcp` | `server/tool` / `*` | exact tool or whole server |

Per-token anchored matching is the important trick: `command(npm run (build|lint|test))` splits on whitespace and anchors each token, so it matches `npm run build` but not `npm run build; rm -rf /`.

**Decision order.** Two incompatible conventions exist. Antigravity: strict `Deny > Ask > Allow`, so `ask: command(*)` beats `allow: command(git)`. opencode: last matching glob wins, evaluated session → agent → global. **Choosing Antigravity's**: a user who writes a broad `ask` rule means it as a floor, and surprising *upward* is much worse than surprising downward.

**Implicit rules** worth encoding, both from Antigravity: write implies read; deny-read implies deny-write.

**Scope grants, don't re-ask.** Persist grants (opencode → DB) and let the user widen the target on the prompt (Antigravity: broaden `/proj/f.txt` to `/proj` for the rest of the turn, validated to still cover the request). Prompt fatigue is what drives people to `--dangerously-skip-permissions`.

**Modes are presets over the same engine**, not a parallel code path: `plan` (read-only), `default` (ask), `accept-edits`, `bypass`. Shift+Tab cycles.

## B. Sandboxing — OS containment

Independent of policy. Policy decides *whether to try*; the sandbox decides *what the process can reach if it lies*.

**macOS — Seatbelt via `/usr/bin/sandbox-exec`** (hardcode the path; never resolve via `PATH`). Deny-by-default SBPL, then selectively allow. Codex's base profile:

```scheme
(version 1)
(deny default)
(allow process-exec)                      ; children inherit the policy
(allow process-fork)
(allow signal (target same-sandbox))
(allow user-preference-read)               ; cf prefs
(allow process-info* (target same-sandbox))
(allow file-write-data                     ; /dev/null only
  (require-all (path "/dev/null") (vnode-type CHARACTER-DEVICE)))
(allow sysctl-read (sysctl-name "hw.ncpu") (sysctl-name "hw.memsize") ...)
(allow ipc-posix-sem)                      ; python multiprocessing
(allow pseudo-tty)                         ; openpty()
```

Writable roots are passed as **parameters**, not interpolated into the policy text — no quoting/injection surface:

```
/usr/bin/sandbox-exec -p <POLICY> \
  -DWRITABLE_ROOT_0=/path/to/project \
  -DWRITABLE_ROOT_0_RO_0=/path/to/project/.git \
  -DWRITABLE_ROOT_0_RO_1=/path/to/project/.octane \
  -DWRITABLE_ROOT_1=/private/tmp \
  -- bash -c "…"
```

```scheme
(allow file-write*
  (require-all (subpath (param "WRITABLE_ROOT_0"))
               (require-not (subpath (param "WRITABLE_ROOT_0_RO_0")))))
```

**The single most important detail I found:** `.git/` and the agent's own config dir are carved out **read-only inside writable roots**. Otherwise the agent can write `.git/hooks/pre-commit` (arbitrary code on the user's next commit) or rewrite its own sandbox config. That's privilege escalation through a tool that only ever asked to "write a file in the project."

Network is off by default. Enabling it needs extra mach-lookup grants (`com.apple.networkd`, `com.apple.SystemConfiguration.DNSConfiguration`, `com.apple.trustd.agent`, `SecurityServer`) plus write access to the Darwin user cache dir for TLS. With a proxy, restrict to the loopback port: `(allow network-outbound (remote ip "localhost:43128"))`.

`sandbox-exec` is formally deprecated by Apple but is what every harness uses; there is no supported replacement for command-line process sandboxing.

**Linux** — bubblewrap (namespace isolation) + Landlock 5.13+ (path-based FS access) + seccomp-bpf (syscall filtering, which is how network is denied). Codex ships this as a separate helper binary invoked with `--sandbox-policy <json>`, which is the right shape: the policy crosses a process boundary as data.

**Windows** — restricted process token / AppContainer.

**Policy levels** (Codex's `SandboxPolicy`, adopting directly): `ReadOnly`, `WorkspaceWrite { writable_roots, network }`, `DangerFullAccess`, `ExternalSandbox` (already inside a container — don't double-wrap).

**Detect denial and offer escalation.** Codex has `is_likely_sandbox_denied()`; a sandbox `EPERM` looks like a broken command to the model and it will waste turns "fixing" it. Catch it and ask the user instead.

## C. MCP client

JSON-RPC 2.0. Three lifecycle phases:

1. **Initialize** — client sends `initialize` with `protocolVersion`, client capabilities (`roots`, `sampling`), `clientInfo`. **Must not be batched.** Server replies with its capabilities (`tools`, `resources`, `prompts`, `logging`, `completions`), `serverInfo`, and an optional `instructions` string. If the server answers with a version we don't support, disconnect.
2. **Operation** — only use negotiated capabilities. `listChanged` sub-capability means the server will notify on tool-list changes; `subscribe` (resources only) means per-item change notifications.
3. **Shutdown** — no shutdown message. For stdio: close the child's stdin, wait, then `SIGTERM`.

Client sends nothing but `ping` before the `initialize` response; then `notifications/initialized`.

Transports: **stdio** (spawn a subprocess, newline-delimited JSON over its pipes) and **streamable HTTP** (remote, with SSE for server→client). On a 401 from a remote server, opencode kicks off OAuth Dynamic Client Registration (RFC 7591) automatically.

Two integration rules from the survey: MCP tools go into the **same registry** and through the **same permission engine** as builtins, and the tool list must be **sorted** before it enters the prompt or it destroys the prefix cache.

`instructions` from the server is untrusted third-party text entering the system prompt. Treat as data, fence it, and never let it outrank harness rules.

## D. Skills

Following the [Agent Skills spec](https://agentskills.io/specification) rather than inventing a format — it's what Claude Code, Antigravity, and opencode all consume.

```
skill-name/
├── SKILL.md          # required: YAML frontmatter + markdown body
├── scripts/          # optional: executables the agent may run
├── references/       # optional: docs loaded on demand
└── assets/           # optional: templates, data
```

Frontmatter: `name` (required, ≤64 chars, `[a-z0-9-]`, no leading/trailing/double hyphen, **must equal the directory name**), `description` (required, ≤1024, must say *what* and *when*), optional `license`, `compatibility` (≤500), `metadata` (string map), `allowed-tools` (space-separated, e.g. `Bash(git:*) Bash(jq:*) Read`, experimental).

**Progressive disclosure is the whole point** — three tiers:

| Tier | Loaded | Budget |
|---|---|---|
| metadata (`name` + `description`) | at startup, for every skill | ~100 tokens each |
| `SKILL.md` body | when the skill activates | <5k tokens, ≤500 lines |
| `references/`, `scripts/`, `assets/` | only when the body points at them | unbounded |

So the startup cost of N installed skills is ~100N tokens, and depth is free until used. References should stay one level deep from `SKILL.md`.

**Protect skill output from pruning** (opencode does): skills carry project rules, and silently dropping them mid-session degrades quality in a way that looks like the model got dumber.

## E. Memory

Layered files loaded at session start, most general first so specific overrides general:

```
enterprise/managed policy   →  org-wide, not user-editable
~/.octane/OCTANE.md         →  user, all projects
<repo root>/OCTANE.md       →  project, committed
./OCTANE.md (subdirs)       →  aggregated from git root down to cwd
OCTANE.local.md             →  personal, gitignored
```

Codex aggregates `AGENTS.md` from the git root down to cwd — every level on the path contributes, so a monorepo subpackage adds context without repeating the root's.

Read **both** `OCTANE.md` and `AGENTS.md`; `AGENTS.md` is the emerging cross-tool convention and users should not have to duplicate.

Support `@path/to/file` imports so memory can reference docs without inlining them.

Memory sits in the cached prefix, so it must be **stable within a session**. Re-reading a changed file mid-session poisons the cache — snapshot at session start (as Claude Code does with its directory tree and git status) and note in the prompt that it is a snapshot.

**Auto-memory**: the agent proposing durable notes for future sessions is cited as a genuine retention feature, not over-engineering. Needs a write path and user review.

## F. Slash commands

Markdown files, discovered by filename:

```
.octane/commands/review.md      →  /review          (project)
~/.octane/commands/review.md    →  /review          (user)
.octane/commands/db/migrate.md  →  /db:migrate      (namespaced by directory)
```

Frontmatter: `description` (shown in the picker), `argument-hint`, `allowed-tools`, `model`. Body is a prompt template with `$ARGUMENTS` / `$1`, `$2` positional substitution, and — the useful part — `!`command`` inline shell substitution so a command can inject live state (`!`git diff --staged``) before the model ever sees it.

A command expands to a *prompt*, not a code path. That is why the whole extension model stays declarative.

## G. The ReAct loop — what actually goes in it

Composing the above, the loop body per step:

```
1. assemble prompt      static-first: system → tools(sorted) → sandbox ctx
                        → memory → history → new input        [octane-context]
2. stream inference     normalized StreamEvents               [octane-provider]
3. on tool call:
     a. registry lookup                                       [octane-tools]
     b. policy decision  allow / ask / deny                    [octane-permission]
     c. pre-hook                                               [octane-hooks]
     d. wrap in sandbox, execute                               [octane-sandbox]
     e. post-hook
     f. persist, publish event
4. check stop conditions
     - finish_reason != tool_calls          → turn done
     - step cap exceeded                    → abort
     - repeated (tool, input, output) x5/10 → loop detected, abort
     - context budget crossed               → compact, continue
     - permission denied                     → end turn, do not retry
```

Everything in brackets is a separate crate with one responsibility. The loop itself coordinates and holds no domain logic — that's the SRP line, and it's also what makes each piece testable without a model.

---

# Addendum: terminal UI

Studied opencode's TUI, [pi](https://github.com/badlogic/pi-mono) (Mario Zechner's minimal agent), and Cline. The single most consequential finding is the first one.

## H. Two kinds of TUI, and why the choice is not cosmetic

pi's write-up frames this better than anything else I found. There are exactly two ways to build a terminal UI, and picking one determines what the product can do.

**Full-screen.** Take over the viewport, treat it as a grid of cells, draw everything yourself. Used by **opencode** and Amp.

- You lose the scrollback buffer, so you must implement your own scrolling.
- You lose terminal search (`cmd+F`), so you must implement your own.
- Copy/paste stops selecting what the user expects.
- Mouse scrolling "always feels kind of off", because you are re-implementing something the terminal already does well.

**Scrollback.** Write to the terminal like a normal CLI, appending to scrollback, and only move the cursor back up within the visible region to redraw the live parts — the input box and the spinner. Used by **Claude Code, Codex, Droid, and pi**.

- Native scrolling, native search, native copy/paste, native everything.
- Works over SSH and in tmux without special handling.
- Constrains what the UI can be — which is a feature. A coding agent is a linear chat: prompt, replies, tool calls, results. That maps onto the terminal's native model exactly.

**Taking the scrollback approach.** A coding agent gains nothing from owning the viewport, and loses the three things users actually rely on. Ratatui supports this directly: `Viewport::Inline(n)` keeps a small live region at the bottom while `Terminal::insert_before()` pushes finished content up into real scrollback.

### Flicker and redraw

Two techniques, both from pi, both cheap:

**Synchronized output.** Wrap every frame in `CSI ?2026h` … `CSI ?2026l` so the terminal buffers and presents atomically. Supported by most modern terminals; the difference between "flickers noticeably" and "does not" in Ghostty/iTerm2.

**Differential rendering.** Keep the previously rendered lines, find the first line that differs, move the cursor there, redraw to the end. Three cases:

1. First render — write everything.
2. Width changed — full clear and re-render, because soft wrapping moved.
3. Otherwise — redraw from the first differing line.

With one catch: if the first change is *above* the viewport (the user scrolled), a full re-render is required, because the terminal will not let you write into scrollback above the visible region.

**Cache completed components.** A fully-streamed assistant message never needs its markdown re-parsed. Render once, keep the lines. This is what makes "compare every line every frame" affordable.

## I. Interaction patterns worth adopting

**From opencode:**

| Affordance | Why |
|---|---|
| `@path` fuzzy file reference | Pulls file content into context without a tool call round trip |
| `!command` prefix | Runs a shell command directly, output attached as a tool result — no inference spent to run `ls` |
| `/command` picker | Discoverable slash commands |
| Leader key (`ctrl+x`) | Chords instead of colliding with terminal/readline bindings |
| `/details` | Toggle tool-execution detail — collapsed by default, expandable |
| `/thinking` | Toggle reasoning block visibility |
| `/undo`, `/redo` | Git-backed, removes the message *and* the file changes |
| `/editor` | Compose in `$EDITOR` for long prompts |
| Attention notifications | Bell/notification when the agent needs input after going quiet |

**From Cline:** Plan/Act as the primary interaction axis, with conversation carried across the switch — the planning context is the point, so discarding it on transition would defeat the purpose. Cline also allows **different models per mode**: a stronger reasoning model for Plan, a faster one for Act.

**From Antigravity** (§2 earlier): the edit-approval prompt offers `y` / `n` / `f` full-screen diff / `Ctrl+G` open in `$EDITOR` — **or type instructions**, which rejects the edit *and tells the agent what to do differently*. That turns a permission prompt into a steering opportunity, and it is the single best idea in any of these UIs.

## J. What the status line has to answer

Three questions a user has constantly, none of which should require a command:

- **What will happen if I hit enter?** — the active mode, since `plan` and `bypass` behave completely differently.
- **How much room is left?** — context utilization, before compaction surprises them.
- **What is this costing?** — cumulative session spend.

Plus which model is answering, since mid-session switching is a feature.

## K. Layout

```
 ← native scrollback: finished messages, tool calls, diffs.
   Terminal owns scrolling, search, and selection.

  ⠋ Editing src/main.rs · 12s · ↑1.2k ↓340          ← transient, only while working
 ╭──────────────────────────────────────────────╮
 │ > _                                          │   ← inline viewport
 ╰──────────────────────────────────────────────╯
   build · sonnet-5 · ctx 12% · $0.04 · ⇧⭾ mode      ← status
```

---

# Addendum: providers and model configuration

## L. Four APIs cover almost everything

From pi's write-up, confirmed by Junie shipping exactly the same set: there are four wire formats worth speaking.

| Format | Endpoint | Reaches |
|---|---|---|
| OpenAI Chat Completions | `/v1/chat/completions` | OpenAI, and nearly every self-hosted and third-party endpoint — Ollama, vLLM, LM Studio, llama.cpp, Groq, Cerebras, xAI, Mistral, OpenRouter, DeepSeek, LiteLLM proxies |
| OpenAI Responses | `/v1/responses` | newer OpenAI, and endpoints that implement it |
| Anthropic Messages | `/v1/messages` | Anthropic, Bedrock, Vertex |
| Google Generative AI | `:generateContent` | AI Studio, Vertex |

Completions is the one with the long tail of divergence, because every provider has its own reading of it. Real cases from pi:

- Cerebras, xAI, Mistral, Chutes reject the `store` field
- Mistral and Chutes want `max_tokens`, not `max_completion_tokens`
- Cerebras, xAI, Mistral, Chutes do not support the `developer` role
- Grok rejects `reasoning_effort`
- Reasoning comes back as `reasoning_content` on some, `reasoning` on others
- Google still does not stream tool calls

None of that belongs in the agent loop. It belongs in a per-provider transform (`RESEARCH.md` §2, opencode's `ProviderTransform`).

## M. Junie's custom-model profiles

The best configuration design I found, and worth adopting nearly wholesale.

**Discovery.** JSON files in `$JUNIE_HOME/models/*.json` (user) and `.junie/models/*.json` (project). The filename minus `.json` is the profile id — no name field to keep in sync with anything.

**Shape.** Top-level fields are defaults; two optional roles override them.

```json
{
  "baseUrl": "https://openrouter.ai/api/v1/chat/completions",
  "id": "qwen/qwen3-coder",
  "apiType": "OpenAICompletion",
  "apiKey": "${OPENROUTER_API_KEY}",
  "extraHeaders": { "X-Title": "octane" },
  "extraBody":    { "tags": ["team:platform"] },
  "temperature": 0,
  "maxContextLength": 262144,
  "primaryModel": { "id": "anthropic/claude-sonnet-4.5" },
  "fasterModel":  { "id": "qwen/qwen3-coder", "temperature": 0 }
}
```

**`primaryModel` / `fasterModel`** is a good abstraction. The faster role is for summarization and classification — exactly the hidden compaction and title agents — and letting it be a cheaper model is a real cost lever that costs nothing to expose.

**Merge rules**, and the distinction matters:

- scalars (`id`, `baseUrl`, `apiKey`, `apiType`, `temperature`, `maxContextLength`) — replaced
- `extraHeaders` — merged, role wins on conflict
- `extraBody` — merged **recursively**, so a role can override one nested key without replacing the whole subtree

**`${VAR}` environment references** in `apiKey` and `extraHeaders`, because profiles get committed to share a `baseUrl` or routing setup with a team. A missing variable is a **load error naming the variable**, not a silent empty header — the alternative is a 401 that looks like a bad key.

**`extraBody`** merges into the request root, for proxies that want routing metadata or tags (LiteLLM). Junie notes it takes precedence over fields it sets itself, which is a sharp edge worth documenting rather than preventing.

**No temperature by default.** Junie sends none unless configured, letting the provider use its own. Right call: a hardcoded `0.7` silently overrides a provider's tuned default.

Junie also says plainly that small or heavily quantized models fail at agentic work — malformed tool calls, drift, loops — and that this is the model's limitation, not the harness's. Worth repeating in our own docs, because it is the first thing people blame the tool for.

## N. Crush / catwalk: provider with a models array

[catwalk](https://github.com/charmbracelet/catwalk) is Crush's provider database, and it fixes Junie's main weakness — one file per model.

```go
type Provider struct {
    Name, ID, APIKey, APIEndpoint string
    Type                Type              // openai | openai-compat | anthropic | google | azure | bedrock | google-vertex | ...
    DefaultLargeModelID string            // == Junie's primaryModel
    DefaultSmallModelID string            // == Junie's fasterModel
    Models              []Model
    DefaultHeaders      map[string]string
}
```

Crush also distinguishes `openai` from `openai-compat` — same wire format, but the former means "actually OpenAI behind a gateway" and the latter "something else wearing its shape". That distinction drives model detection, not encoding.

What catwalk gets wrong for our purposes: `Type` and `APIEndpoint` sit on the *provider*, so a provider speaks exactly one format at one URL.

## O. What octane adopts

Junie's `${VAR}` handling and merge semantics, catwalk's provider-with-models shape, and one thing neither has.

**`api` and `baseUrl` are per-model.** A single gateway commonly fronts several shapes at once — `/v1/chat/completions`, `/v1/responses`, and `/v1/messages` behind one host — and neither format can express that. Both fields become provider *defaults* any model may override, which also makes Google's two flavours expressible: same `api: "google"`, different `baseUrl` and different `auth`.

**Auth is typed, not a key string.** `apiKey` carries a configurable header and prefix, because the three major formats disagree — `Authorization: Bearer`, `x-api-key` with no prefix, `x-goog-api-key`. Plus `none` for local endpoints, `googleVertex` (project + location + ADC), `awsSigV4` (region + credential chain), and `tokenFile` for OAuth flows minted out of band.

**Unusable providers are listed with a reason.** Filtering a provider out because its variable is unset, silently, is how someone loses an hour. Junie fails the whole load; we report and continue, so one bad file does not stop a session.

Discovery from `~/.octane/providers/*.json` and `.octane/providers/*.json`, filename as the provider key, project winning. A file replaces a built-in of the same name wholesale rather than merging — a half-overridden connection is harder to reason about, and merging makes it impossible to *remove* a model.

---

# Addendum: subscription auth

## P. Anthropic: third-party subscription auth is off the table

Not a technical limitation. A policy one, enforced.

- **19 March 2026** — opencode merged [PR #18186](https://github.com/anomalyco/opencode/pull/18186), commit message "anthropic legal requests", removing the Anthropic OAuth plugin, the Claude system prompt, and every reference to Claude Pro/Max authentication.
- **4 April 2026** — Anthropic enforced a policy that Claude Pro, Max, and Team subscriptions no longer cover usage through third-party harnesses authenticating by OAuth.

So octane **will not implement Claude Pro/Max OAuth**. It would breach Anthropic's terms, it is the exact thing a comparable project received legal demands over, and it is actively being blocked, so it would break regardless. Anthropic access is by API key, which is supported and is what the API is for.

## Q. OpenAI: documented for first-party clients, unclear for others

OpenAI documents two sign-in paths for Codex — ChatGPT subscription and API key — and `codex login` opens a browser flow. That is documented for *OpenAI's own* surfaces: the ChatGPT desktop app, Codex CLI, and the IDE extension.

Whether a third-party client may use the same flow is not stated anywhere OpenAI publishes, and the practical route people ask about is reusing Codex's own client ID — which is impersonating a first-party client, not an integration. Developers are asking on OpenAI's forum how to do it correctly, which is itself a sign there is no sanctioned answer.

octane will not ship a hardcoded first-party client ID.

## R. What octane does instead

Build the mechanism, not the credentials.

1. **API keys everywhere.** Supported by every provider, and the only path that is unambiguously permitted.
2. **A generic OAuth 2.0 flow** — authorization code with PKCE, and device code — driven entirely from the provider JSON: `clientId`, endpoints, scopes. If a provider sanctions third-party subscription access, it becomes a config file rather than a code change. Enterprise gateways behind an IdP need this regardless, and that is a case nobody objects to.
3. **`tokenFile`**, already present: mint a token however you like out of band, octane reads it. This is the escape hatch for anything octane should not be doing itself.

The line: octane ships the protocol, and the user supplies the client identity. A harness that hardcodes someone else's client ID is not integrating with a provider, it is pretending to be one.
