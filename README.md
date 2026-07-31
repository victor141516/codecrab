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

## CodeCrab vs. OpenAI Codex

The table below compares the user-facing capabilities documented in this README with OpenAI Codex at [`b545c94`](https://github.com/openai/codex/tree/b545c94041017d000e2c8b2f6272705d21b85dfb). It is a product comparison, not a benchmark: some Codex capabilities are experimental, account-dependent, or available only through particular clients. CodeCrab has also evolved since the gap analysis used for this comparison—most notably, it now includes persistent PTYs and agent-to-agent delegation.

| Capability | CodeCrab | OpenAI Codex | Practical difference |
| --- | --- | --- | --- |
| Autonomous agent loop | Runs without approval prompts, sandboxing, or a tool-round limit. | Runs autonomously, with configurable approvals, sandbox profiles, permissions, and Guardian checks. | Both perform multi-step repository work; CodeCrab favors unconditional autonomy while Codex offers more security gates. |
| ChatGPT authentication | Browser OAuth PKCE with refresh, plus API-key profiles. | Browser or device-code ChatGPT login, API keys, and additional credential paths. | Core ChatGPT subscription access is shared; Codex has more login and secure-storage options. |
| User interfaces | Full-screen TUI, one-shot CLI, embedded web app, and HTTP/NDJSON API from one binary. | TUI, advanced `exec`, app-server, IDE/Desktop continuity, and official SDKs. | CodeCrab ships its own browser client; Codex exposes a broader platform for external clients and IDEs. |
| Providers and local models | OpenAI plus configurable OpenAI-compatible profiles, including manually configured local endpoints. | OpenAI plus integrated Ollama/LM Studio onboarding and native Amazon Bedrock support. | CodeCrab emphasizes generic compatible endpoints; Codex has richer first-class onboarding and provider-specific integrations. |
| Models, reasoning, and speed | Uses the provider catalog and persists model, reasoning, and service tier per session. | Exposes model and reasoning selection across its clients and protocol. | Broadly comparable; CodeCrab deliberately avoids inventing provider capabilities absent from the catalog. |
| Streaming and tool activity | Persists and streams assistant deltas, retries, progress, and typed tool lifecycle events in exact order to TUI and web. | Streams typed thread, turn, item, plan, diff, approval, and tool events through app-server and clients. | Both are live and structured; Codex exposes a broader event taxonomy for integrations. |
| File editing and diffs | Reads, writes, and performs exact replacements; changes can be inspected with Git commands. | Adds `apply_patch`, aggregated turn diffs, `/diff`, and `codex apply`. | Codex has a first-class patch/diff workflow; CodeCrab currently exposes lower-level edits. |
| Shell and persistent PTYs | Supports non-interactive commands and conversation-scoped PTYs with text, paste, key, mouse, resize, observe, list, and close operations. | Supports PTYs, background processes, stdin, resize, termination, `/ps`, and `/stop`. | Both support persistent interactive processes; Codex has more direct user-facing process management. |
| Persistent sessions | Saves cross-project sessions, live workers, activities, model state, terminals, branches, goals, and URL-addressable web navigation. | Adds rename, archive, search, filters, sections, manual ordering, pagination, and session forks. | CodeCrab covers persistence and concurrent work; Codex provides richer session organization. |
| Conversation branches | Keeps a non-destructive tree inside one session, with message editing and reversible preview in TUI and web. | Provides `/side`, `/btw`, `/fork`, in-memory forks, and forks at a selected turn. | CodeCrab emphasizes visible in-session history; Codex offers more ways to promote or isolate forks. |
| Long-running goals and planning | Persistent goals continue automatically until the model verifies completion or reports a blocker. | Provides structured Plan mode, plan events, and multiple-choice `request_user_input`. | CodeCrab has first-class autonomous goals; Codex has richer interactive planning and decision checkpoints. |
| Agent delegation | Creates persistent child sessions with isolated context, live observation, follow-ups, waiting, and exact-turn cancellation. | Stable subagent collaboration plus richer collaboration controls and thread navigation. | The core capability exists in both; Codex currently exposes a broader collaboration surface. |
| Agent Skills | Discovers project/user skills, supports explicit or automatic selection, and progressively loads `SKILL.md` resources. | Adds skill search, remote previews, dependency installation, and distribution through plugins. | Skill execution is comparable at the core; Codex has a larger discovery and distribution ecosystem. |
| Project instructions | Loads global personal instructions and one root-level project `AGENTS.md` per agent. | Resolves hierarchical instructions from project root to cwd, with fallbacks and limits. | Codex handles nested monorepo instructions; CodeCrab currently snapshots only global plus selected-root guidance. |
| Composer completion | Shares slash-command, skill, and fuzzy `@path` completion between Rust TUI and web clients. | Provides slash-command UX and can generate shell completion scripts. | CodeCrab prioritizes identical in-composer behavior across its two clients; Codex also integrates with the user's shell. |
| Follow-ups and steering | Both clients queue editable follow-ups; Steer cancels the active turn and sends one selected item next. | Supports steering and rich turn/subagent interaction through its clients and protocol. | Both support mid-work direction; CodeCrab documents queue ordering as a first-class client behavior. |
| Context compaction | Automatically compacts at safe boundaries while retaining the complete canonical transcript and persisted checkpoints. | Automatically compacts and also exposes `/compact` and an API operation to force it. | The automatic behavior is shared; Codex gives users and integrations manual control. |
| Voice | Dictation in TUI and web, with insertion or immediate send and a live web waveform. | Accepts structured audio and offers experimental bidirectional realtime voice with incremental transcription and spoken output. | Prompt transcription is shared; Codex's realtime conversation is broader but experimental. |
| Images, web, and computer use | No structured image input, image generation, hosted web search, browser use, or computer-use tools. | Supports image input and generation, web search, browser integrations, and computer use when client/account capabilities allow. | This is one of Codex's largest tool and modality advantages. |
| Extensibility | Agent Skills plus an HTTP/NDJSON API tailored to the embedded web client. | MCP client and server, Apps, plugins, marketplaces, hooks, app-server, and TypeScript/Python SDKs. | CodeCrab stays compact; Codex is a substantially broader integration platform. |
| Security model | Deliberately uses the operating-system account as its only boundary and never asks for execution approval. | Offers sandbox profiles, approvals, execution rules, managed network controls, and enterprise policy. | This is an intentional product-policy difference, not simply a missing CodeCrab feature. |
| Diagnostics | Effective config output, auth status, health API, lazy TUI error logs, and explicitly unredacted OpenAI traffic debugging. | Adds `doctor`, installation/update checks, redacted diagnostic reports, telemetry/audit options, and feedback workflows. | CodeCrab provides focused local diagnostics; Codex has broader operational tooling. |
| Distribution | One native binary embeds the complete Vue web application. | Native CLI/app-server plus SDKs and integrations with richer clients. | CodeCrab is simpler to self-host as a complete UI; Codex spans more separately integrated surfaces. |

In short, both cover the core local coding-agent workflow: autonomous turns, repository tools, streaming, sessions, models, skills, compaction, and ChatGPT authentication. CodeCrab differentiates itself through a compact self-contained TUI/web product, persistent goals, explicit cross-client parity, and an always-autonomous policy. Codex goes further in multimodality, patch/review workflows, planning, extensibility, IDE/platform integration, remote execution, and configurable security controls.

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
| `Ctrl+V`, `Alt+V`, `Command+V` when reported | Paste clipboard files or image pixels; terminals may consume `Command+V` before CodeCrab sees it |
| `Enter` | Complete the selected item or send |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | Insert a newline (`Alt+Enter` or `Ctrl+J` on macOS) |
| `Ctrl+Shift+S` | Start or stop dictation (`Ctrl+S` on macOS) |
| `Up` / `Down` | Navigate menus or move between visual editor rows |
| `PgUp` / `PgDn`, mouse wheel | Scroll the conversation |
| `Esc` | Discard an active recording; press twice within one second to stop an active turn |
| `Ctrl+D` or `Ctrl+C` | Save and quit while idle |

Run `/help` or press `F1` for the complete keyboard reference. The terminal handles Unicode-aware soft wrapping, international keyboard layouts including AltGr, Markdown and code highlighting, mouse selection, native clipboard copy, and explicit image/file clipboard paste.

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

### File and image attachments

The web composer accepts multiple files through its paperclip button, paste, or drag and drop. Browser files are hashed before upload, checked against the selected session, and streamed to the agent host only when that SHA-256 is not already present. Uploads are limited to 25 MiB per file. Generic files remain unchanged and are exposed to the agent as stable host-side paths; CodeCrab does not automatically parse, OCR, or upload them to the model provider.

The terminal keeps ordinary local files lightweight: `@path` and pasted non-image files remain direct absolute or project-relative host paths. Images selected through `@`, copied as files, or copied as raw screenshot/browser pixels are imported into session storage. `Ctrl+V` is the reliable in-app clipboard action, with `Alt+V` as a fallback; `Command+V` works only when the terminal forwards it.

Attachment metadata is stored in the session JSON while bytes live under `.codecrab/session-data/<session-id>/attachments/<sha256>/`. Deduplication is deliberately per session, and deleting a session deletes its attachment data. Composer bindings preserve the order of text and images across drafts, queued prompts, edits, branches, persistence, and resume.

Images retain their original bytes and receive a metadata-stripped, aspect-preserving model rendition no larger than 1024 px on either edge. CodeCrab never upscales. PNG is used when alpha matters and JPEG otherwise; the agent can call `view_attachment_image` for a bounded higher-detail rendition. Structured image tool results use the ChatGPT Responses path; compatible Chat Completions backends receive an explicit unsupported-output error. Image submission is rejected without clearing the draft unless the provider catalog explicitly declares image input support for the selected model.

Because browser uploads write files on the CodeCrab host, the existing server warning is especially important: remotely exposed servers must be placed behind an authenticated gateway. Attachment contents are not logged except when the explicitly unredacted `--debug-openai` mode includes a multimodal provider request.

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
| `POST` | `/api/attachments/preflight` | Look up a session attachment by browser-computed SHA-256 |
| `POST` | `/api/attachments/upload` | Stream and verify one bounded browser file upload for an explicit project/session |
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
- `src/attachments.rs` — session attachment storage, hashing, image processing, and model renditions.
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
