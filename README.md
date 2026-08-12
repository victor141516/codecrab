# CodeCrab 🦀

<p align="center">
  <img src="assets/codecrab-agent.png" alt="CodeCrab secret-agent crab mascot" width="320">
</p>

**A small, auditable coding agent for your terminal and browser, written in Rust.**

CodeCrab turns a plain-language request into real repository work: it can inspect code, edit files, run commands, verify the result, and keep going until the task is finished. The same autonomous agent powers a compact terminal UI, one-shot CLI commands, and an embedded web app.

CodeCrab keeps final responses concise by default while adapting when the user requests more detail.

> [!IMPORTANT]
> CodeCrab is intentionally autonomous. It runs file changes and commands without approval prompts, has the same filesystem access as your operating-system account, and is not confined to the selected project. Use it only where you are comfortable granting that access.

## Features

- 🤖 **Autonomous execution** — no approval prompts, artificial tool-round limits, or hidden iteration cap.
- 💬 **ChatGPT Plus/Pro login** — use your ChatGPT subscription through browser-based OAuth; no API key or separate API billing is required.
- 📊 **OpenAI usage and resets** — see provider-reported quota windows and use available manual reset credits from the terminal or web client.
- 🖥️ **Three ways to work** — full-screen terminal UI, pipe-friendly one-shot CLI, and a responsive browser interface.
- 🔌 **Flexible providers** — use OpenAI or OpenAI-compatible Chat Completions APIs, including local providers.
- ⚡ **Live, transparent progress** — stream assistant text and structured reads, searches, edits, commands, retries, and other tool activity as they happen.
- 🧵 **Managed background processes** — see per-session process counts, inspect styled live output, jump to the originating shell activity, and stop a process tree from either client.
- 🧭 **Web code explorer and diffs** — optionally embed a managed `code-server`, follow edits live, and inspect per-operation or complete turn changes.
- 💾 **Persistent sessions** — resume, rename, pin, and archive conversations with their messages, tool history, model settings, terminals, goals, and project context intact.
- 🌿 **Conversation branches** — edit an earlier prompt, preview alternate paths, and switch branches without losing history.
- 🎯 **Long-running goals** — give CodeCrab an objective and let it continue across turns until it verifies completion or reports a blocker.
- 🗓️ **Scheduled agent tasks** — run recurring or delayed prompts through a persistent, hot-reloaded cron daemon, with execution history in both clients.
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
| File editing and diffs | Reads, writes, and performs exact replacements; the web client can follow edits and open Git-backed operation diffs or accumulated turn changes through a managed `code-server`. | Adds `apply_patch`, aggregated turn diffs, `/diff`, and `codex apply`. | Both expose first-class diffs; CodeCrab uses its optional embedded web IDE while Codex integrates the workflow directly into its clients. |
| Shell and persistent PTYs | Supports non-interactive commands and conversation-scoped PTYs with text, paste, key, mouse, resize, observation, styled live output, origin navigation, per-session counts, and termination through `/processes`. | Supports PTYs, background processes, stdin, resize, termination, `/ps`, and `/stop`. | Both expose persistent process management through their terminal and graphical clients. |
| Persistent sessions | Saves cross-project sessions, live workers, activities, model state, terminals, branches, goals, editable titles, pins, archives, and URL-addressable web navigation. | Adds search, filters, sections, manual ordering, pagination, and session forks. | CodeCrab covers persistent metadata and concurrent work; Codex provides richer large-catalog organization. |
| Conversation branches | Keeps a non-destructive tree inside one session, with message editing and reversible preview in TUI and web. | Provides `/side`, `/btw`, `/fork`, in-memory forks, and forks at a selected turn. | CodeCrab emphasizes visible in-session history; Codex offers more ways to promote or isolate forks. |
| Long-running goals and planning | Persistent goals continue automatically until the model verifies completion or reports a blocker. | Provides structured Plan mode, plan events, and multiple-choice `request_user_input`. | CodeCrab has first-class autonomous goals; Codex has richer interactive planning and decision checkpoints. |
| Agent delegation | Creates persistent child sessions with isolated context, live observation, follow-ups, waiting, and exact-turn cancellation. | Stable subagent collaboration plus richer collaboration controls and thread navigation. | The core capability exists in both; Codex currently exposes a broader collaboration surface. |
| Agent Skills | Discovers project/user skills, supports explicit or automatic selection, and progressively loads `SKILL.md` resources. | Adds skill search, remote previews, dependency installation, and distribution through plugins. | Skill execution is comparable at the core; Codex has a larger discovery and distribution ecosystem. |
| Project instructions | Loads global personal instructions, the Git repository-root `AGENTS.md`, and the selected-directory `AGENTS.md` per agent. | Resolves hierarchical instructions from project root to cwd, with fallbacks and limits. | Both compose repository and working-directory guidance; Codex additionally supports intermediate directories, fallbacks, and size limits. |
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

Download a binary for Windows, macOS, or Linux from the [latest GitHub Release](https://github.com/victor141516/codecrab/releases/latest), rename it to `codecrab` (`codecrab.exe` on Windows), and put it on your `PATH`. The Windows executable embeds the CodeCrab mascot as its application icon.

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

All three modes use the same agent, providers, tools, skills, instructions, sessions, and model-selection behavior. Session lists include a global **No project** group followed by project groups, and each group is arranged as a tree: roots and siblings are ordered by creation time, newest first, while children stay beneath their parent. Activity and metadata updates do not move an existing session. Pinned shortcuts appear first in each project, ordered by pin time, while archived trees live in a collapsed section ordered by archive time. The terminal picker shows creation (`C`) and last-update (`U`) times, while the denser web sidebar keeps each session to one title row. `codecrab sessions` prints `PARENT`, `PINNED`, `ARCHIVED`, `CREATED`, and `UPDATED` columns. `codecrab resume` without an ID still considers archived sessions and resumes the most recently updated session.

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
| `/no-project` | Start a new session without selecting a project |
| `/processes` | Inspect and stop managed shell processes in the current session |
| `Ctrl+V`, `Alt+V`, `Command+V` when reported | Paste clipboard files or image pixels; terminals may consume `Command+V` before CodeCrab sees it |
| `Enter` | Complete the selected item or send |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | Insert a newline (`Alt+Enter` or `Ctrl+J` on macOS) |
| `Ctrl+Shift+S` | Start or stop dictation (`Ctrl+S` on macOS) |
| `Up` / `Down` | Navigate menus or move between visual editor rows |
| `PgUp` / `PgDn`, mouse wheel | Scroll the conversation |
| `Esc` | Discard an active recording; press twice within one second to stop an active turn |
| `Ctrl+D` or `Ctrl+C` | Save and quit while idle |

Run `/help` or press `F1` for the complete keyboard reference. The terminal handles Unicode-aware soft wrapping, international keyboard layouts including AltGr, Markdown and code highlighting, mouse selection, native clipboard copy, and explicit image/file clipboard paste.

Both composers identify references while you write. Built-in commands are green, skills are blue, existing files are cyan, directories are yellow, and unrecognized slash or `@path` references are red. The web presents these references as inline pills in its plain-text rich composer; the underlying prompt, selection, autocomplete, dictation, attachments, drafts, and queued-message behavior remain text-compatible.

Long shell commands are limited to one terminal activity row and end with an ellipsis only when they exceed the available width; copying the activity still returns the complete command. The web client keeps its expand/collapse control, while `codecrab run` prints complete command activity.

When a managed shell command remains active, its session row shows a live count. `/processes` opens the current session's process list in both clients; the web count opens the same view after selecting that session. Each entry shows its command and duration and can open a read-only, ANSI-styled live viewer, jump to the exact shell activity that created it, or stop its complete managed process tree after confirmation. The viewer retains the existing 1 MiB terminal scrollback, interprets overwritten lines, and stays open with a final state when the command finishes. Stopping a process does not stop the agent turn, so the agent may run it again if needed.

Sending a new turn while the agent is idle jumps the terminal conversation to the bottom and resumes automatic following. You can keep typing while an agent turn runs; sending then adds an editable follow-up to a queue without changing a manually scrolled position. Built-in slash commands remain local client actions: they run immediately when compatible with the active turn, or report why they cannot run, and never enter the follow-up queue. Skill invocations are agent prompts and may be queued normally. **Steer** cancels the current turn and sends one selected follow-up next without reordering the rest.

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

Open the web UI automatically with an optional launch mode:

```bash
codecrab serve --open-browser
codecrab serve --open-browser http
codecrab serve --open-browser app
codecrab serve --open-browser app-http
```

Without a value, `--open-browser` opens HTTPS in the operating system's default browser. `http` opens the HTTP URL instead. The `app` and `app-http` modes make a best-effort launch of the default HTTPS or HTTP browser, respectively, with its `--app=<url>` flag for a standalone, PWA-like window. CodeCrab passes that flag to the selected browser without checking its brand or engine, so a browser that does not support app windows may ignore or reject it. If the default handler cannot be resolved or launched, CodeCrab reports an error suggesting the matching non-app mode instead of silently choosing another browser. Omitting `--open-browser` keeps the previous behavior and does not open anything. Automatically selected port values are resolved before the URL is opened.

The frontend is embedded in the Rust executable and calls relative `/api` URLs, so it works from the same origin and behind a reverse proxy. HTTP and HTTPS stream assistant deltas, activity, cancellation, and session updates rather than waiting for a final response. A freshly started server opens without an active project or session; use the **No project** row or any registered project row to create one.

The web header also has separate **Code panel** and **Follow changes** controls. Opening the panel lazily starts one managed [`code-server`](https://github.com/coder/code-server#getting-started) process per project, proxied below the same CodeCrab origin with HTTP and WebSocket support. The panel is drag-resizable on wide screens and full-screen on compact screens. Its width is browser-local, while following is remembered per browser and project. Manual editor navigation suspends following; re-enabling it jumps to the latest pending batch and focuses the largest contiguous change. The explorer starts at the selected project, or at the filesystem root for a No project session, shows normally hidden entries such as `.git`, and retains CodeCrab's no-sandbox filesystem policy: users may open any path available to the server's operating-system account.

`code-server` is optional and is never downloaded or installed by CodeCrab. Install it yourself or set `code_server_path` in the global config. CodeCrab was tested with `code-server 4.131.0`, but compatibility is detected by loading the bundled CodeCrab extension rather than requiring that exact version. The managed integration supports platforms where `code-server` runs natively and is unavailable on native Windows; CodeCrab does not invoke WSL. It uses a persistent profile under CodeCrab's global data directory, separate from the user's ordinary VS Code/code-server profile. Settings, themes, and user-installed extensions in that isolated profile are retained. On every managed start, CodeCrab republishes and re-registers its bundled extension so upgrades repair stale integration metadata without removing unrelated extensions.

The first code-panel implementation presents files as visually read-only, but this is not a security boundary: `code-server`, installed extensions, and the process account may still provide ways to write. Git-tracked project files keep reconstructible per-operation diffs plus accumulated turn changes. New, ignored, untracked, external, and non-Git files use one temporary pre-edit copy per file and turn; their accumulated turn diff expires when the next turn begins. Turn changes may include concurrent filesystem edits from another source. Changes made indirectly through shell commands remain visible through `code-server` Source Control instead of CodeCrab's operation history.

The web UI is also an installable PWA. Open it from localhost or a trusted HTTPS origin, then use the browser's **Install CodeCrab** action. Its application shell is cached so the installed window can start without a connection, but conversations, sessions, and every `/api` operation still require the CodeCrab server to be running; API responses are never cached by the service worker.

> [!WARNING]
> The server has **no built-in HTTP authentication**. Keep it on localhost or place it behind an authenticated gateway before exposing it to a network. HTTPS encrypts traffic but does not authenticate users; its fresh in-memory certificate is self-signed and changes on every startup.

Press `Ctrl+C` once for graceful shutdown. If requests, connections, or managed terminal processes keep the process alive, CodeCrab explains why; press `Ctrl+C` again to stop managed process trees and force an immediate exit.

## Everyday workflows

### Sessions and projects

CodeCrab saves project-local JSON sessions under `.codecrab/sessions/` and maintains a global registry so both clients can browse work across projects. No project sessions are stored in CodeCrab's global data directory and do not persist the process working directory. A newly opened session remains available while active, but CodeCrab discards it on navigation or clean shutdown if it still has no conversation or other user-created session state.

```bash
codecrab sessions
codecrab resume
codecrab resume <session-id-or-prefix>
```

The terminal `/sessions` view and web sidebar both expose the complete project/session hierarchy, including the top-level **No project** group. Child sessions are indented recursively; parents use disclosure chevrons and show the number of hidden descendants when collapsed. In the terminal, left/right collapse and expand projects or session branches, `R` renames, `P` toggles a pin, and `A` archives or restores the selected session. The web edits a title directly and keeps pin and archive buttons visible on every session row. A manual title is permanent, trimmed, non-empty, limited to 120 characters, and unaffected by later edits to the first message.

Pinned child sessions remain in their canonical tree and also appear as contextual shortcuts at the start of the project. Archiving is organizational only: it does not pause workers, goals, cron runs, or processes, change conversation recency, close the active session, or exclude it from an implicit resume. Archiving a parent hides its descendants through an inherited state without overwriting their own archive metadata. On desktop, the web sidebar can be hidden completely from its header and reopened from the workspace header; that browser-local preference survives reloads, while the compact mobile drawer remains transient. Collapsing a branch is visual only and never pauses its workers. Children whose parent is missing or belongs to another project remain roots in their own project with a compact parent-path hint. Deleting a parent does not delete or rewrite its descendants.

Deleting a session with active managed processes requires one confirmation covering all of them. Accepting stops every managed process tree before deleting; declining leaves both the processes and session untouched. The terminal asks the same question before exiting while managed processes are active.

The web composer keeps model, reasoning, and speed selection beside its attachment, dictation, and send actions. Switching sessions also switches the agent's working directory, tools, file completion, skills, and `AGENTS.md` context. A turn can keep running in one session while you open or start another; returning restores its live stream.

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
| `/models` | Choose a provider-returned model, reasoning level, and speed |
| `/processes` | Inspect and stop managed shell processes in the current session |
| `/skills` | Browse installed Agent Skills |
| `/sessions` | Browse, resume, rename, pin, archive, restore, or delete sessions across projects |
| `/no-project` | Start a new session without a project |
| `/cron` | Browse, run, pause, edit, or delete scheduled agent tasks |
| `/branches` | Preview and select conversation branches |
| `/providers` | Choose the provider for the current session |
| `/goal <objective>` | Start a persistent goal |
| `/goals` | Browse and manage goals |
| `/usage` | Inspect OpenAI ChatGPT quota and use manual reset credits |
| `/quit` | Save and exit |

A built-in command runs only when it is the entire input. Text such as `Explain /help` is sent to the agent unchanged.

### Scheduled agent tasks

Run a scheduler for the platform-global `cron.json` (stored beside CodeCrab's global configuration), or pass another file explicitly:

```bash
codecrab cron
codecrab cron ./team-cron.json
codecrab cron --check
codecrab cron --list
codecrab cron --status
```

The process keeps running, hot-reloads valid file changes, and pauses scheduling without discarding the last good runtime state when JSON is invalid. Only one process can own a schedule file. CodeCrab uses an operating-system file lock on a separate stable lock file because the JSON itself is replaced atomically when edited; the mere presence of that lock file never means a daemon is alive.

The versioned JSON document uses job IDs as keys:

```json
{
  "version": 1,
  "timezone": "Europe/Madrid",
  "jobs": {
    "weekly-tests": {
      "schedule": "0 3 * * 2",
      "enabled": true,
      "project": "/absolute/path/to/project",
      "prompt": "Run the complete test suite, diagnose failures, and report the result.",
      "provider": "openai",
      "model": "gpt-5.6-sol",
      "reasoning": "high",
      "speed": "fast",
      "timezone": "Europe/Madrid",
      "overlap": "queue",
      "timeout_seconds": 7200
    }
  }
}
```

Schedules accept standard five-field cron syntax, common aliases such as `@daily`, `@reboot` for one run when the daemon starts, and one-time RFC 3339 instants such as `@at 2026-08-03T15:00:00+02:00`. Every time-based job uses a named IANA timezone, previews its next five computed occurrences, and starts in a fresh persistent session with the captured project, provider, model, reasoning, and speed. Recurring runs never inherit context from earlier runs. `timeout_seconds` is optional and there is no total timeout by default.

The terminal `/cron` command and the web calendar control expose the same definitions, next occurrences, recent execution status, last response, and full linked session. A manual **Run now** also works while the daemon is stopped as long as that CodeCrab process remains open. Missed recurring occurrences are not replayed. An overdue one-time job becomes `expired` and can be started manually. Overlap defaults to `skip`; `queue` retains only the newest pending occurrence and records the superseded occurrence as skipped.

The agent can preview, create, pause, resume, delete, and run jobs. Mutating a definition requires an exact deterministic preview and explicit user confirmation. If no daemon is running, recurring creation is rejected; a one-time request waits inside the current turn and CodeCrab displays that closing or stopping the process cancels the wait.

Install conservative per-user autostart, or remove every CodeCrab-owned artifact, with:

```bash
codecrab cron --install
codecrab cron --uninstall
```

CodeCrab preflights and verifies Windows Task Scheduler, Linux `systemd --user`, or a macOS LaunchAgent. A failed installation rolls back its artifacts. Installation state is derived from the actual operating-system registration and the live lock, not stored as a configuration boolean.

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

When you explicitly request another agent, parallel work, delegation, or independent validation, CodeCrab can start persistent sessions through the `session_create` model tool. Agent-created sessions are children by default: CodeCrab persists the calling session as `parent_session_id` even when the model omits the relationship argument. `relationship: "independent"` creates a project-level root with no parent and is reserved for an explicit request for a separate, detached, non-child, or user-like session.

Each child has isolated model context, tools, skills, goals, activity, and project instructions; it does not inherit or summarize the parent's transcript. Its runtime context contains only minimal lineage metadata identifying the parent session. That lineage does not imply a sandbox, worktree, permission boundary, or ownership restriction.

Children appear immediately in the terminal session picker and web sidebar. You can open a child while it runs to see streamed messages and tool activity while the parent continues updating independently. The parent can inspect status or messages, send follow-ups, wait efficiently, or stop exactly one child turn.

Delegation is process-local and uses the same operating-system account and filesystem. There is no automatic worktree, sandbox, write lock, or conflict prevention, so parallel agents should edit disjoint areas or coordinate explicitly.

### Automatic context compaction

CodeCrab uses provider-reported context metadata to compact long conversations automatically. It keeps recent complete turns verbatim and rolls older history into a structured summary, while preserving the full canonical transcript and tool activity in session JSON. Both clients display compaction activity, and a failed or cancelled compaction never replaces the previous projection. Provider-specific output limits are sent only when the selected protocol supports them. Deterministic request-validation failures are not replayed, and if emergency compaction cannot recover from a context-window rejection, CodeCrab stops that turn instead of resending the unchanged oversized request.

## Authentication and providers

### ChatGPT Plus/Pro

```bash
codecrab auth login
codecrab auth status
codecrab auth logout
```

CodeCrab uses OAuth PKCE, refreshes tokens automatically, and uses the subscription's Codex allowance. The default OpenAI profile has `auth = "auto"`: it selects ChatGPT OAuth when a login exists, otherwise its configured API key. Use `oauth` or `api_key` to require one path explicitly.

When the selected provider is the official OpenAI service and ChatGPT OAuth is active, both clients show a compact quota indicator. Open `/usage` in the terminal or select the indicator in the web header to inspect every provider-reported window, percentage used and remaining, and the automatic reset time in your local timezone. Window lengths and reset-credit availability come from OpenAI rather than from hardcoded plan assumptions.

If OpenAI grants the account manual reset credits, the same view can redeem one at any time after explicit confirmation. Credits and quota belong to the OpenAI account, not to an individual conversation, so they can be viewed or used from any eligible OpenAI session. CodeCrab refreshes usage after successful turns and reset attempts. A failed refresh leaves the last known values marked stale and disables redemption until a refresh succeeds; usage failures never block chatting.

Quota and reset-credit support uses private ChatGPT subscription endpoints. This is not a public compatibility contract and may change without notice. The feature is hidden for OpenAI API-key sessions and OpenAI-compatible providers.

### API keys and compatible providers

Provider profiles are managed in the global `config.toml` shown by `codecrab config`. For example, add a local provider without authentication with:

```toml
[providers.local]
model = "local-model"
base_url = "http://localhost:11434/v1"
auth = "none"
fetch_models = false
allowed_models = ["local-model"]

[providers.local.model_capabilities.local-model]
input_modalities = ["text"]
output_modalities = ["text"]
```

List the configured profiles without exposing their API keys:

```bash
codecrab providers
```

Provider authentication modes are `auto`, `oauth`, `api_key`, and `none`. Edit provider secrets directly in `config.toml`; normal configuration and provider-list output omit them.

Profiles keep their base URL, current key, model, discovered or manually declared catalog, reasoning options, service tiers, modalities, and context limits together. Sessions store the profile name and model selection but never copy provider secrets. Use `/providers` in the terminal or the provider control above the web composer to change only the current session; the choice survives resume without changing `active_provider` or any other session. See [`codecrab.example.toml`](codecrab.example.toml) for complete OpenAI, remote-compatible, and local-provider examples.

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

Project skills shadow later skills with the same name. CodeCrab initially exposes compact skill metadata, loads the full `SKILL.md` only when selected, and reads referenced resources progressively. Opening `/` completion lazily rediscovers skills in both clients, so added, edited, deleted, or newly invalid skills take effect in the active conversation without restarting it.

For general personal guidance, create `~/.config/codecrab/AGENTS.md`. For project guidance, CodeCrab loads `AGENTS.md` first from the Git repository root and then from the selected working directory, skipping the second copy when both directories are the same. CodeCrab snapshots these instructions when constructing an agent, so changes apply to new conversations, cold resumes, or project switches rather than an already-running agent.

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

Relative tool paths start at the selected working directory, but parent paths, absolute paths, other drives, and symbolic links are valid. In No project sessions, structured file tools and `@` completion require absolute paths; bare `@` begins at the platform filesystem root. Shells still start in the directory where the CodeCrab process was launched (or the effective `-C` directory). CodeCrab deliberately has no sandbox or project boundary.

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
| `GET` | `/api/usage` | Refresh official OpenAI ChatGPT quota and reset-credit state |
| `POST` | `/api/usage/reset` | Redeem one available OpenAI manual reset credit idempotently |
| `POST` | `/api/completions` | Shared command, skill, and filesystem completion |
| `POST` | `/api/completions/recursive` | Progressive fuzzy filesystem completion batches |
| `POST` | `/api/chat` | Run or edit a prompt as an ordered NDJSON event stream |
| `POST` | `/api/chat/cancel` | Cancel one session's active provider or tool operation |
| `GET` | `/api/processes` | List active managed processes for one session |
| `GET` | `/api/processes/{terminal_id}` | Read styled live or final output for one managed process |
| `POST` | `/api/processes/stop` | Stop one managed process tree without cancelling the agent turn |
| `POST` | `/api/attachments/preflight` | Look up a session attachment by browser-computed SHA-256 |
| `POST` | `/api/attachments/upload` | Stream and verify one bounded browser file upload for an explicit project/session |
| `POST` | `/api/transcribe` | Transcribe uploaded audio |
| `PUT` | `/api/model` | Change model, reasoning, and service tier |
| `PUT` | `/api/provider` | Change the provider and valid model selection for the current session |
| `POST` | `/api/branches/preview` | Preview a conversation path without persistence |
| `POST` | `/api/branches/select` | Select and persist a conversation path |
| `POST` | `/api/sessions` | Create and select a session in a project |
| `GET` | `/api/sessions/stream` | Stream the live session catalog, worker lifecycle, transcripts, and activities |
| `PUT` | `/api/sessions/metadata` | Rename, pin/unpin, or archive/restore one session |
| `POST` | `/api/sessions/delete` | Delete a session |
| `POST` | `/api/sessions/resume` | Resolve and resume a session across registered projects |
| `GET` | `/api/directories` | Browse directories on the backend host |
| `POST` | `/api/directories` | Create a directory on the backend host |
| `POST` | `/api/projects/open` | Open and register an existing directory |
| `GET` | `/api/cron` | Inspect the schedule document, daemon, installation, history, and previews |
| `PUT` | `/api/cron` | Replace the validated schedule document |
| `POST` | `/api/cron/jobs` | Create or replace one validated job |
| `POST` | `/api/cron/jobs/delete` | Delete one job |
| `PUT` | `/api/cron/jobs/enabled` | Pause or resume one job |
| `POST` | `/api/cron/jobs/run` | Run one job now |
| `POST` | `/api/cron/install` | Install per-user scheduler autostart |
| `POST` | `/api/cron/uninstall` | Remove CodeCrab-owned autostart artifacts |
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
- `src/cron.rs` — schedule schema, previews, runtime history, daemon locking, execution, and OS autostart.
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
