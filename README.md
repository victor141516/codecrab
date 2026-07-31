# CodeCrab 🦀

**A small, auditable coding agent for your terminal and browser, written in Rust.**

CodeCrab turns a plain-language request into real repository work: it can inspect code, edit files, run commands, verify the result, and keep going until the task is finished. The same autonomous agent powers a compact terminal UI, one-shot CLI commands, and an embedded web app.

> [!IMPORTANT]
> CodeCrab is intentionally autonomous. It runs file changes and commands without approval prompts, has the same filesystem access as your operating-system account, and is not confined to the selected project. Use it only where you are comfortable granting that access.

## Features

- 🤖 **Autonomous execution** — no approval prompts, artificial tool-round limits, or hidden iteration cap.
- 💬 **ChatGPT Plus/Pro login** — use your ChatGPT subscription through browser-based OAuth; no API key or separate API billing is required.
- 🖥️ **Three ways to work** — full-screen terminal UI, pipe-friendly one-shot CLI, and a responsive browser interface.
- 🔌 **Flexible providers** — use OpenAI or OpenAI-compatible Chat Completions APIs, including local providers.
- ⚡ **Live, transparent progress** — stream assistant text and structured reads, searches, edits, commands, retries, and other tool activity as they happen.
- 💾 **Persistent sessions** — resume conversations with their messages, tool history, model settings, terminals, goals, and project context intact.
- 🌿 **Conversation branches** — edit an earlier prompt, preview alternate paths, and switch branches without losing history.
- 🎯 **Long-running goals** — give CodeCrab an objective and let it continue across turns until it verifies completion or reports a blocker.
- 🧩 **Agent Skills** — discover reusable `SKILL.md` workflows and invoke them explicitly with `/skill-name` or let the model select one.
- 👥 **Agent delegation** — create isolated child sessions for parallel work or independent validation, then inspect and steer them live.
- 📎 **Project-aware input** — shared `/command`, skill, and fuzzy `@path` autocomplete in both terminal and web clients.
- 🎙️ **Voice dictation** — record, transcribe, review, or send prompts from either interface.
- 🦀 **One native binary** — the Vue web application and syntax highlighting are embedded; Node.js is not needed at runtime.

## Quick start

### 1. Install

Download a binary for Windows, macOS, or Linux from the [latest GitHub Release](https://github.com/victor141516/codecrab/releases/latest), rename it to `codecrab` (`codecrab.exe` on Windows), and put it on your `PATH`.

On macOS or Linux, make the downloaded file executable:

```bash
chmod +x codecrab
```

Or build from source:

```bash
git clone https://github.com/victor141516/codecrab.git
cd codecrab
cargo install --path .
```

A source build requires a recent Rust toolchain and Node.js 20.19+ or 22.12+. Linux also needs ALSA development headers and `pkg-config` (for example, `libasound2-dev` and `pkg-config` on Debian/Ubuntu).

### 2. Sign in

Use the OpenAI account that owns your ChatGPT Plus or Pro subscription:

```bash
codecrab auth login
```

A browser opens for the OAuth flow. An OpenAI API key or another OpenAI-compatible provider can be configured instead.

### 3. Open a project

```bash
cd path/to/your/project
codecrab
```

Then describe the outcome you want—for example, `Find and fix the failing tests`. CodeCrab will inspect the project, make changes, run checks, and summarize the result.

To open another directory without changing your shell's working directory:

```bash
codecrab -C path/to/project
```

## Choose your interface

All three modes use the same agent, providers, tools, skills, instructions, sessions, and model-selection behavior. Session lists are grouped by project and ordered by creation time, newest first; activity updates do not move an existing session. The terminal and web pickers show both creation (`C`) and last-update (`U`) times, and `codecrab sessions` prints `CREATED` and `UPDATED` columns. `codecrab resume` without an ID still resumes the most recently updated session.

### Interactive terminal

```bash
codecrab
codecrab resume
codecrab resume <session-id-or-prefix>
```

Useful composer controls:

| Input | Action |
| --- | --- |
| `/` at the beginning | Complete built-in commands and skills |
| `/` after existing text | Complete skills only |
| `@path` | Find and reference a file or directory |
| `Enter` | Complete the selected item or send |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | Insert a newline (`Alt+Enter` or `Ctrl+J` on macOS) |
| `Ctrl+Shift+S` | Start or stop dictation (`Ctrl+S` on macOS) |
| `Up` / `Down` | Navigate menus or move between visual editor rows |
| `PgUp` / `PgDn`, mouse wheel | Scroll the conversation |
| `Esc` | Discard an active recording; press twice within one second to stop an active turn |
| `Ctrl+D` or `Ctrl+C` | Save and quit while idle |

Run `/help` or press `F1` for the complete keyboard reference. The terminal handles Unicode-aware soft wrapping, international keyboard layouts including AltGr, Markdown and code highlighting, mouse selection, and native clipboard copy.

You can keep typing while an agent turn runs. Sending adds an editable follow-up to a queue; **Steer** cancels the current turn and sends one selected follow-up next without reordering the rest.

### One-shot CLI

Run one task, print the final answer, and exit:

```bash
codecrab run "Find and fix the failing test"
```

Read the prompt from standard input:

```bash
cat task.md | codecrab run
```

Or combine a prompt with piped context:

```bash
git diff | codecrab run "Review this diff for correctness"
```

### Web app and API

```bash
codecrab serve
```

The command prints all bound addresses. By default it serves:

- Web UI: `http://127.0.0.1:4096/`
- JSON/NDJSON API: `http://127.0.0.1:4096/api`
- Web UI and API over self-signed HTTPS: `https://127.0.0.1:4097/`

Use different addresses or automatically selected ports when needed:

```bash
codecrab serve --host 0.0.0.0 --port 8080 --https-port 8443
codecrab serve --port 0 --https-port 0
```

The frontend is embedded in the Rust executable and calls relative `/api` URLs, so it works from the same origin and behind a reverse proxy. HTTP and HTTPS stream assistant deltas, activity, cancellation, and session updates rather than waiting for a final response.

> [!WARNING]
> The server has **no built-in HTTP authentication**. Keep it on localhost or place it behind an authenticated gateway before exposing it to a network. HTTPS encrypts traffic but does not authenticate users; its fresh in-memory certificate is self-signed and changes on every startup.

Press `Ctrl+C` once for graceful shutdown. If requests or connections keep the process alive, CodeCrab explains why; press `Ctrl+C` again to force an immediate exit.

## Everyday workflows

### Sessions and projects

CodeCrab saves project-local JSON sessions under `.codecrab/sessions/` and maintains a global registry so both clients can browse work across projects.

```bash
codecrab sessions
codecrab resume
codecrab resume <session-id-or-prefix>
```

The terminal `/sessions` view and web sidebar both expose the complete project/session hierarchy. Switching sessions also switches the agent's working directory, tools, file completion, skills, and `AGENTS.md` context. A turn can keep running in one session while you open or start another; returning restores its live stream.

Web session URLs use `/sessions/<id>`, so a clean reload or shared URL resolves the correct registered project without relying on browser storage. Unsent composer drafts remain browser-local and isolated by project and session.

### Commands

| Command | Purpose |
| --- | --- |
| `/help` | Open keyboard and command help |
| `/model` | Choose a provider-returned model, reasoning level, and speed |
| `/skills` | Browse installed Agent Skills |
| `/sessions` | Browse, resume, or delete sessions across projects |
| `/branches` | Preview and select conversation branches |
| `/providers` | List configured provider profiles |
| `/provider` | Add, select, or remove a provider profile |
| `/goal <objective>` | Start a persistent goal |
| `/goals` | Browse and manage goals |
| `/clear` | Clear the current conversation context |
| `/quit` | Save and exit |

A built-in command runs only when it is the entire input. Text such as `Explain /help` is sent to the agent unchanged.

### Branches

Conversation history is stored as a tree. Editing a previous visible user message creates a new sibling branch rather than overwriting the original continuation. `/branches` opens a reversible preview and lets you select any saved path; terminal and web clients use the same underlying branch semantics.

### Persistent goals

Start one with:

```text
/goal Upgrade the dependency and verify the complete test suite
```

CodeCrab stores the objective, pauses any previous active goal, and continues automatically across hidden follow-up turns. The model must explicitly mark the goal complete after verification or blocked when external state prevents progress. A normal final answer does not silently complete it.

Use `/goals` to pause, resume, edit, inspect, or delete historical goals. Normal Stop and double-`Esc` pause the active goal; **Steer** preserves it and sends additional direction.

### Agent delegation

When you explicitly request another agent, parallel work, delegation, or independent validation, CodeCrab can start persistent child sessions. Each child has isolated model context, tools, skills, goals, activity, and project instructions; it does not inherit the parent's transcript.

Children appear immediately in the terminal session picker and web sidebar. You can open a child while it runs to see streamed messages and tool activity while the parent continues updating independently. The parent can inspect status or messages, send follow-ups, wait efficiently, or stop exactly one child turn.

Delegation is process-local and uses the same operating-system account and filesystem. There is no automatic worktree, sandbox, write lock, or conflict prevention, so parallel agents should edit disjoint areas or coordinate explicitly.

### Automatic context compaction

CodeCrab uses provider-reported context metadata to compact long conversations automatically. It keeps recent complete turns verbatim and rolls older history into a structured summary, while preserving the full canonical transcript and tool activity in session JSON. Both clients display compaction activity, and a failed or cancelled compaction never replaces the previous projection.

## Authentication and providers

### ChatGPT Plus/Pro

```bash
codecrab auth login
codecrab auth status
codecrab auth logout
```

CodeCrab uses OAuth PKCE, refreshes tokens automatically, and uses the subscription's Codex allowance. The default OpenAI profile has `auth = "auto"`: it selects ChatGPT OAuth when a login exists, otherwise its configured API key. Use `oauth` or `api_key` to require one path explicitly.

### API keys and compatible providers

Add and activate a local provider without authentication:

```bash
codecrab provider add local \
  --base-url http://localhost:11434/v1 \
  --auth none \
  --model local-model \
  --activate
```

Manage profiles with:

```bash
codecrab provider list
codecrab provider show local
codecrab provider use local
codecrab provider remove local
```

Provider authentication modes are `auto`, `oauth`, `api_key`, and `none`. `provider add` prompts for an omitted API key without echo; automation can use `--api-key-stdin`. Avoid `--api-key` when shell history or process listings are a concern.

Profiles keep their base URL, current key, model, discovered or manually declared catalog, reasoning options, service tiers, modalities, and context limits together. Sessions store the profile name and model selection but never copy provider secrets. See [`codecrab.example.toml`](codecrab.example.toml) for complete OpenAI, remote-compatible, and local-provider examples.

New sessions with `model = "auto"` prefer GPT-5.6 Sol with high reasoning and Fast speed when the live provider catalog offers that combination. Otherwise CodeCrab uses the provider's first model and declared defaults; it does not invent unsupported model variants.

## Agent Skills and project instructions

CodeCrab implements the `SKILL.md` Agent Skills format. A project skill can look like this:

```text
.agents/skills/
└── review-rust/
    ├── SKILL.md
    ├── references/
    ├── scripts/
    └── assets/
```

```markdown
---
name: review-rust
description: Review Rust changes for correctness and safety.
---

Inspect the affected code and tests before reporting actionable findings.
```

List available skills:

```bash
codecrab skills
```

Invoke one in any prompt:

```text
Use /review-rust to review my changes
```

Discovery precedence is:

1. `.agents/skills` from the selected directory up to its Git root.
2. `~/.agents/skills`.
3. `$CODEX_HOME/skills`, or `~/.codex/skills`.
4. Extra path-list entries from `CODECRAB_SKILLS_DIR`.

Project skills shadow later skills with the same name. CodeCrab initially exposes compact skill metadata, loads the full `SKILL.md` only when selected, and reads referenced resources progressively.

For general personal guidance, create `~/.config/codecrab/AGENTS.md`. For repository-specific guidance, create `AGENTS.md` at the selected project root. CodeCrab snapshots these instructions when constructing an agent, so changes apply to new conversations, cold resumes, or project switches rather than an already-running agent.

## Configuration

Inspect the platform-global file path and effective non-secret configuration:

```bash
codecrab config
```

The complete format is documented in [`codecrab.example.toml`](codecrab.example.toml). It covers provider profiles, manual model catalogs and allowlists, reasoning and service-tier options, context limits, request inactivity timeout, project registry, and deterministic shell selection.

Environment variables override the file; CLI `--model` and `--base-url` flags have the highest priority.

| Variable | Purpose |
| --- | --- |
| `CODECRAB_PROVIDER` | Active provider for new sessions |
| `CODECRAB_MODEL` | Active provider's model |
| `CODECRAB_BASE_URL` | Active provider's compatible `/v1` base URL |
| `CODECRAB_AUTH` | `auto`, `oauth`, `api_key`, or `none` |
| `CODECRAB_API_KEY` | Temporary API key for this process |
| `CODECRAB_SHELL` | Shell executable used by managed PTYs |
| `CODECRAB_SKILLS_DIR` | Additional skill directories, separated like `PATH` |

`request_timeout_seconds` measures model-response **inactivity**, not total request duration. Every streamed chunk resets it. CodeCrab retries model timeouts and request failures up to five times and persists every retry and terminal error.

### Secret storage

ChatGPT OAuth tokens and provider API keys are stored as plain text in the global `config.toml`, protected only by your operating-system account permissions. They are not written into project sessions, and normal CLI, TUI, and web configuration views redact them.

## Filesystem, shell, and terminal access

Relative tool paths start at the selected working directory, but parent paths, absolute paths, other drives, and symbolic links are valid. CodeCrab deliberately has no sandbox or project boundary.

The agent can:

- list, read, and recursively search files;
- create files and overwrite UTF-8 text;
- perform exact, unique text replacements;
- run non-interactive commands with separate stdout and stderr;
- launch persistent interactive PTYs and send text, paste, key, mouse, and resize actions;
- observe, list, and close managed terminals.

A PTY command still running after five seconds returns a conversation-scoped terminal ID so the agent can continue interacting with it. Live terminals survive session navigation within one process; after a process restart they are marked interrupted because a PTY cannot be reattached.

## Voice dictation

Terminal and web capture use `Ctrl+Shift+S` (`Ctrl+S` on macOS); the web client also provides a microphone button and displays a live waveform. Stopping normally transcribes and inserts text at the current composer cursor, or at the end of an unfocused web draft. Pressing Enter in an active terminal recording, or Send in an active web recording, transcribes and submits immediately. Pressing `Esc` in either client, or using the web discard button, stops recording and deletes the captured audio without transcribing it.

With ChatGPT OAuth, dictation uses ChatGPT's private subscription transcription endpoint. This is not a public compatibility contract and may change without notice. API-key transcription uses the official provider's `/audio/transcriptions` endpoint. Microphone permission is required.

## HTTP API

`codecrab serve` exposes the same core behavior used by the web application. Chat and recursive completion responses are NDJSON streams.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/health` | Process health and version |
| `GET` | `/api/state` | Current project, session, models, skills, providers, and workers |
| `POST` | `/api/completions` | Shared command, skill, and filesystem completion |
| `POST` | `/api/completions/recursive` | Progressive fuzzy filesystem completion batches |
| `POST` | `/api/chat` | Run or edit a prompt as an ordered NDJSON event stream |
| `POST` | `/api/chat/cancel` | Cancel one session's active provider or tool operation |
| `POST` | `/api/transcribe` | Transcribe uploaded audio |
| `PUT` | `/api/model` | Change model, reasoning, and service tier |
| `POST` | `/api/providers` | Add or replace a provider profile |
| `POST` | `/api/providers/use` | Select the provider for new sessions |
| `POST` | `/api/providers/delete` | Delete an inactive provider profile |
| `POST` | `/api/session/clear` | Clear one session |
| `POST` | `/api/branches/preview` | Preview a conversation path without persistence |
| `POST` | `/api/branches/select` | Select and persist a conversation path |
| `POST` | `/api/sessions` | Create and select a session in a project |
| `GET` | `/api/sessions/stream` | Stream the live session catalog, worker lifecycle, transcripts, and activities |
| `POST` | `/api/sessions/delete` | Delete a session |
| `POST` | `/api/sessions/resume` | Resolve and resume a session across registered projects |
| `GET` | `/api/directories` | Browse directories on the backend host |
| `POST` | `/api/directories` | Create a directory on the backend host |
| `POST` | `/api/projects/open` | Open and register an existing directory |
| `POST` | `/api/goals/create` | Create and activate a goal |
| `PUT` | `/api/goals/edit` | Replace a goal objective |
| `POST` | `/api/goals/activate` | Activate a saved goal |
| `POST` | `/api/goals/pause` | Pause a goal |
| `POST` | `/api/goals/delete` | Delete a goal |

## Diagnostics

Interactive TUI errors are written to a lazily created execution log so stderr cannot corrupt the terminal. Select a fixed location with:

```bash
codecrab --error-log ./codecrab-errors.log
```

Non-TUI modes report diagnostics to stderr.

For protocol troubleshooting, print complete OpenAI/OAuth requests and responses to stderr or a file:

```bash
codecrab --debug-openai run "Say hello"
codecrab --debug-openai=./openai-debug.log serve
```

> [!CAUTION]
> `--debug-openai` is intentionally **unredacted**. Its output can contain API keys, access and refresh tokens, authorization codes, prompts, repository contents, tool arguments, and model responses. Never share it without reviewing the complete log.

The `=` before a debug path is required. Explicit debug files are appended to lazily, and a file error is surfaced rather than silently falling back to stderr.

## Architecture

```text
TUI / run / web API / session tools
                 │
                 ▼
        SessionCoordinator
                 │
                 ▼
       ConversationManager
   ├── session A ──▶ Tokio worker ──▶ Agent A
   ├── session B ──▶ Tokio worker ──▶ Agent B
   └── session C ──▶ Tokio worker ──▶ Agent C
```

Each conversation worker exclusively owns one `Agent`, serializes its commands and persistence, and emits authoritative snapshots and ordered events. Presentation layers never retain or lock an agent. This keeps terminal, CLI, web, background sessions, and delegated sessions on one behavior path.

The main implementation areas are:

- `src/agent.rs` — model/tool loop, progress policy, goals, and instruction loading.
- `src/compaction/` — context projection and automatic structured compaction.
- `src/conversation.rs` — persistent Tokio workers, commands, snapshots, and cancellation.
- `src/coordination.rs` — shared agent construction and session delegation.
- `src/provider.rs` — ChatGPT Responses and compatible Chat Completions protocols.
- `src/tools.rs` and `src/terminal/` — filesystem, commands, PTYs, and terminal emulation.
- `src/completion.rs` — shared slash, skill, and filesystem completion.
- `src/session.rs` — conversation trees, goals, persistence, and project catalogs.
- `src/skills.rs` — Agent Skills discovery, activation, and resources.
- `src/ui.rs` — Ratatui client.
- `src/server.rs` — Axum API and embedded application server.
- `web/` — Vue 3 and Tailwind frontend.

## Development

Prerequisites:

- A recent Rust toolchain with Rust 2024 edition support
- Node.js 20.19+ or 22.12+
- Linux: ALSA development headers and `pkg-config`

Run the project checks:

```bash
npm --prefix web ci
npm --prefix web test
npm --prefix web run build
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Rust builds invoke `build.rs`. On a fresh checkout it runs `npm ci`, builds the frontend, verifies that production output contains exactly `index.html`, `app.js`, and `app.css`, and embeds those assets in the executable.

## License

CodeCrab is available under the [MIT License](LICENSE).
