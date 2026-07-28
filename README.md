# CodeCrab

CodeCrab is a small, auditable coding agent for the terminal and web, written in Rust.
It takes the useful core of tools such as Pi and OpenCode—a model/tool loop,
project-aware file operations, shell access, and resumable sessions—without
starting with a large client/server platform.

This repository contains a working, compact coding agent with a full-screen
terminal interface and an optional embedded web application.

## What works

- Full-screen terminal UI with a scrollable conversation and multiline editor.
- Vue and Tailwind web interface served directly from the CodeCrab binary.
- JSON/NDJSON API for conversations, sessions, models, skills, and agent state.
- Live, persisted tool activity in both clients: reads, searches, writes,
  edits, shell commands, and skill loading are visible while they happen.
- Completed web turns collapse their intermediate progress into a one-line
  duration and operation summary; the final answer remains visible and the
  complete progress can be expanded again.
- Overflowing web tool rows can be expanded to reveal their complete command,
  path, query, or other recorded detail.
- Concise progress messages appear in the same language as the user's latest
  message and stay interleaved with the actions they describe.
- Sanitized Markdown responses in the web client with formatted headings,
  lists, tables, links, inline code, and syntax-highlighted code blocks.
- Per-segment web copy controls for user messages and assistant text, excluding
  intervening tool activity.
- Live working status and compact model context.
- Interactive slash autocomplete for built-in commands and installed skills.
- The web and terminal composers use the same Rust completion engine for slash
  commands, skills, and `@` filesystem paths.
- API-backed hierarchical model picker for model variants, reasoning effort,
  and service speed; selections persist for the session.
- `@` file and folder autocomplete from the project, parent directories, or
  filesystem root.
- Agent Skills support with `SKILL.md`, explicit `/skill-name` invocation, and
  description-based model selection.
- Complete project instructions from `AGENTS.md` in the selected project root.
- One-shot/pipe-friendly operation with `codecrab run`.
- OpenAI-compatible Chat Completions providers.
- Browser login with a ChatGPT Plus/Pro subscription—no API key or separate API
  billing required.
- Voice dictation through the signed-in ChatGPT subscription in terminal and
  web composers; transcripts are inserted for review instead of auto-sent.
- OAuth PKCE, automatic token refresh, and OS credential-store integration.
- Model tools for listing, reading, searching, writing, exact editing, and shell
  commands.
- Relative, parent, and absolute paths across the filesystem.
- Autonomous file mutations and shell commands in every execution mode.
- Unlimited model/tool rounds per turn, continuing until the model returns a
  final answer or an actual provider/tool error occurs.
- Persistent, session-scoped goals with automatic cross-turn continuation,
  explicit model-reported completion/blocking, pause/resume/edit/delete
  controls, and a history containing at most one active goal.
- Project-local JSON sessions with a global cross-project browser, resume, and
  deletion.
- Config file, environment overrides, and CLI overrides.

## Install

You need a recent Rust toolchain and Node.js 20.19+ or 22.12+. The JavaScript
toolchain is needed only while compiling CodeCrab; the resulting executable has
no Node.js runtime dependency.

```console
cargo install --path .
```

Prebuilt executables are also attached to each
[GitHub Release](https://github.com/victor141516/codecrab/releases) for Windows,
macOS, and Linux on x64 and ARM64. Each download is one self-contained CodeCrab
binary with the web application embedded; Node.js is not needed at runtime.
On macOS and Linux, make the downloaded file executable before running it:

```bash
chmod +x codecrab-v*-macos-* codecrab-v*-linux-*
```

Pushing a Git tag whose name starts with `v` automatically builds every
platform and creates the corresponding release. The tag should match the
package version, for example:

```console
git tag -a v1.0.0 -m "CodeCrab v1.0.0"
git push origin v1.0.0
```

Sign in with the OpenAI account that owns your ChatGPT subscription:

```powershell
codecrab auth login
```

```bash
codecrab auth login
```

Then start it inside a project:

```console
codecrab
```

Or point it at a different project:

```console
codecrab -C ../my-project
```

## Usage

Interactive:

```console
codecrab
codecrab --model provider-model-id
codecrab auth status
codecrab resume
codecrab resume 2f31a9c0
codecrab sessions
codecrab skills
```

One prompt and exit:

```console
codecrab run "Find and fix the failing test"
git diff | codecrab run "Review this diff for correctness"
```

### Embedded web application

Start the API and embedded frontend:

```console
codecrab serve
```

The command prints only the API and frontend URLs. The default address is
`http://127.0.0.1:4096`; use an automatically selected port with:

```console
codecrab serve --port 0
```

Press `Ctrl+C` to start a graceful shutdown. If active HTTP requests or open
connections still prevent exit after 100 ms, CodeCrab explains what it is
waiting for on `stderr`. Press `Ctrl+C` a second time to terminate the process
immediately.

The frontend always calls relative `/api` URLs, so it automatically uses the
same scheme, hostname, port, and reverse-proxy origin that served it. To listen
outside localhost:

```console
codecrab serve --host 0.0.0.0 --port 4096
```

Web mode allows file mutations and shell commands without confirmation. This
first server implementation has no built-in HTTP authentication; it is intended
for local use or deployment behind an authenticated gateway.

The initial API surface is:

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/health` | Process health and version |
| `GET` | `/api/state` | Current session, grouped projects/sessions, models, and skills |
| `POST` | `/api/completions` | Shared slash, skill, and filesystem suggestions |
| `POST` | `/api/chat` | Run one visible prompt or hidden goal-continuation turn as an ordered NDJSON stream |
| `POST` | `/api/chat/cancel` | Cancel the active agent turn and its in-flight provider/tool operation |
| `POST` | `/api/transcribe` | Transcribe recorded audio with ChatGPT OAuth |
| `PUT` | `/api/model` | Change model, thinking, and service tier |
| `POST` | `/api/session/clear` | Clear the current conversation |
| `POST` | `/api/sessions` | Create and select a session |
| `POST` | `/api/sessions/delete` | Delete `{ project?, id }`; select the next saved session when active |
| `POST` | `/api/sessions/resume` | Resume `{ project?, id }`, resolving the project globally when omitted |
| `POST` | `/api/goals/create` | Create and activate a goal, pausing the previous active goal |
| `PUT` | `/api/goals/edit` | Replace a goal objective |
| `POST` | `/api/goals/activate` | Activate a saved goal and pause any other active goal |
| `POST` | `/api/goals/pause` | Pause a goal |
| `POST` | `/api/goals/delete` | Delete a goal |

### Raw OpenAI HTTP debugging

Pass `--debug-openai` in any mode to write every OpenAI model/catalog request,
OAuth token request, and corresponding response to `stderr`:

```console
codecrab --debug-openai serve
codecrab --debug-openai run "Say hello"
```

This output is intentionally unredacted. It can contain access and refresh
tokens, authorization codes, prompts, tool arguments, repository content, and
model responses. Store or forward it only when that disclosure is acceptable.

The terminal UI adapts to narrow terminals. Its borderless, full-width
conversation supports `Page Up`, `Page Down`, and mouse-wheel scrolling. The
header shows the current model and thinking level, plus a lightning bolt only
when the selected provider service tier is Fast. The web client keeps speed in
its dedicated selector and does not add a bolt to the model name. Terminal
file activity uses paths relative to the active project whenever possible.
Assistant Markdown keeps its original delimiters visible while adding terminal
colors and supported font styles for headings, bold, italic, inline code,
links, quotes, and list markers. Fenced code blocks use embedded, language-aware
syntax highlighting; CodeCrab does not require or spawn `bat`.

Keyboard shortcuts:

| Key | Action |
| --- | --- |
| `Enter` | Complete a selection, send text, or stop, transcribe, and send an active recording |
| `Tab` | Complete the selected menu item |
| `Shift+Enter`, `Alt+Enter`, or `Ctrl+J` | Insert a newline |
| `Ctrl+Shift+S` | Start or stop voice dictation |
| `Up` / `Down` | Navigate an open menu, otherwise move between editor lines |
| `Ctrl`/`Alt` + `Left`/`Right` | Move by word |
| `Ctrl`/`Alt` + `Backspace` | Delete the previous word |
| `Home`/`End`, `Ctrl+A`/`Ctrl+E` | Move to the start or end of the logical line |
| `Ctrl+U`/`Ctrl+K` | Delete to the start or end of the logical line |
| `PgUp` / `PgDn`, mouse wheel | Navigate an open menu, otherwise scroll |
| `F1` or `?` | Open help |
| `F2` | Show available skills |
| `Delete` / `Backspace` | Delete the selected session in `/sessions` |
| `Esc`, twice within one second | Stop the active agent turn |
| `Ctrl+D` or `Ctrl+C` | Save and quit while idle |

Printable input uses the character resolved by the terminal and active keyboard
layout. This includes `AltGr` combinations on international keyboards; CodeCrab
does not map physical keys such as `2` to layout-specific symbols.
Long composer lines wrap visually to the terminal width without inserting
newlines into the prompt. Up/down navigation uses those visual rows. Word and
line editing consumes Crossterm's decoded terminal events and traditional
Readline sequences rather than checking the operating system; terminal-level
remappings that emit the same sequences therefore keep working.

Mouse interaction inside the terminal conversation uses the operating-system
clipboard on Windows, macOS, Linux/X11, and supported Wayland compositors.
Right-click an individual user or assistant message to copy that complete
message. Right-click a shell activity's detail to copy only its command, or a
read/write/edit activity's detail to copy only its displayed file path. The
copied region briefly uses reversed colors for feedback. Left-click and drag
to select arbitrary visible conversation text; releasing copies the selection,
including any labels or icons crossed by the drag. Dragging into the header or
composer keeps extending the selection while scrolling the conversation up or
down.

The composer remains editable while the agent is working. Sending during an
active turn queues one follow-up and sends it automatically when that turn
finishes. The queued row includes `Steer`: it stops the current turn and sends
the queued message immediately afterward. In the terminal, `Steer` is
deliberately mouse-only; in the browser it is a normal button. The browser Send
button becomes a square Stop button while work is active. Both clients also
support pressing `Esc` twice within one second to stop without steering.

Assistant text is streamed incrementally in both clients. CodeCrab consumes
SSE chunks directly from ChatGPT Responses and compatible Chat Completions,
then forwards text deltas through the agent event stream and `/api/chat`
without waiting for the complete model response. The reconstructed final
message remains the persisted source of truth for Markdown and tool-call
ordering.

Press `Ctrl+Shift+S` in the terminal to begin recording and press it again to
stop. CodeCrab transcribes the recording with the signed-in ChatGPT account and
inserts the result at the editor cursor without sending it. In the web client,
use the microphone button beside Send and press it again to stop recording.
Both clients require microphone permission from the operating system or
browser.

Commands inside the composer:

```text
/help       open keyboard and command help
/model      choose model, reasoning effort, and service speed
/sessions   browse, resume, or delete saved sessions
/goal ...   create and start a persistent goal
/goals      browse, describe, activate, pause, or delete goals
/skills     show available Agent Skills
/clear      clear conversation context
/quit       save and exit
```

### Persistent goals

`/goal <objective>` stores the objective in the current session, pauses any
previously active goal, and uses the objective as the first visible user
prompt. Only one goal can be active, but paused, completed, and blocked goals
remain available through `/goals`.

While a goal is active, CodeCrab includes its complete objective in every model
request and exposes `complete_goal` and `block_goal` as control tools. There is
no separate model request that judges the preceding answer. A normal final
answer leaves the goal active; the client then starts another turn with an
internal continuation prompt. That prompt is persisted for valid provider
history but marked hidden, so neither client renders or copies it as a user
message. The model must explicitly call `complete_goal` after verification or
`block_goal` when external state genuinely prevents progress.

The terminal shows the selected goal directly above the composer. Its
pause/play, edit, delete, and list controls are mouse-only. `/goals` opens a
keyboard-driven list: arrows select, `Enter` or `Space` activates a paused goal
or pauses the active one, `D` opens its scrollable full description,
`Delete`/`Backspace` removes it, and `Esc` closes the current view. The web
client provides the same actions through buttons and multiline dialogs.

Stopping an active turn with the normal Stop control or double `Esc` pauses its
goal, preventing immediate automatic continuation. `Steer` is different: it
cancels the current turn, preserves the active goal, and sends the queued user
message as additional direction. Editing an active goal pauses it during the
edit and resumes it after saving or cancelling.

Typing `/` as the first character opens a filtered menu containing both
built-in commands and skills. Typing `/` after existing text opens the same
menu with skills only. Use the arrow keys and `Enter`/`Tab` to insert the
selection.

Built-in commands execute only when they are the entire input and start at the
first character. For example, `Explain /help` is sent to the agent unchanged.
`/skills` and `F2` open the full interactive skill picker; choosing one inserts
`/skill-name ` into the composer without sending it.

`/model` (and its `/models` alias) opens a three-step picker. CodeCrab fetches
the provider's `/models?client_version=...` catalog and displays its model
variants, supported reasoning levels, and service tiers as a hierarchy rather
than hardcoding combinations. The chosen settings apply to the draft currently
being written and every later turn until changed, and are restored when the
session is resumed. Compatible providers that return the standard
`{"data":[{"id":"..."}]}` shape remain selectable but do not get invented
reasoning or speed options.

New conversations default to GPT-5.6 Sol with `high` reasoning and Fast speed.
CodeCrab resolves the reasoning and Fast service-tier identifiers from the live
provider catalog. If a compatible provider does not offer that model, `auto`
uses the provider's first model and its declared defaults instead.

`/sessions` opens a cross-project session tree. The current project is first,
expanded, and its active session is selected. `Left` moves from a session to
its project and then collapses it; `Right` expands a selected project; vertical
arrows move through the visible tree. `Enter` resumes a session and switches
the agent's working directory, relative tools, `AGENTS.md`, skills, and file
completion to that session's project. `Delete` / `Backspace` removes a session.
In the web sidebar, the normal view shows the selected project's name and its
sessions. Its back button opens the project list; choosing a project opens that
project's sessions. Selecting a session updates the browser URL to
`/sessions/<id>`. Opening or sharing that URL resolves the project globally and
restores the session without relying on browser storage, even when the server
was started from a different project. Deleting the active web session selects
the next saved session in that project and replaces the URL accordingly. If
none remain, no replacement is created: the URL returns to `/` and the sidebar
invites the user to choose `New session`.

Typing `@` opens files and folders from the selected project directory. Use
`@../` for its parent, `@/` for the filesystem root, and continue selecting
folders with `Enter` to browse deeper. Selecting a file inserts its `@path`
without sending the prompt. The compact menu assumes a Nerd Font and renders
only a type-specific icon followed by the entry name; directories remain
visually distinct. Path fragments use normal platform path semantics, so
`@../../`, drive-qualified paths, and other valid relative or absolute paths
work without special-case parsing.

## Agent Skills

When a conversation starts, CodeCrab reads `AGENTS.md` from the selected
project root and includes its complete contents in the system prompt. If the
file does not exist, startup continues without project-specific instructions.
Read errors are reported instead of silently ignoring the file.

The stable system policy asks the model to answer in the language of the
latest user message and to provide brief progress updates around meaningful
tool phases. A runtime block is regenerated for the selected project with the
operating system, CPU architecture, and full working directory. The local
account username is intentionally omitted because it does not normally help
the agent perform repository work.

CodeCrab implements the open Agent Skills `SKILL.md` format. A project skill
looks like this:

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
description: Review Rust changes for correctness and safety. Use for Rust code reviews.
---

Inspect the affected code and tests before reporting actionable findings.
```

Invoke it explicitly:

```text
Use /review-rust to review my changes
```

CodeCrab can also select a skill automatically. In the initial catalog it
splits each `SKILL.md` on lines containing exactly `---`, ignores empty
sections, and sends only the first section containing text. For a standard
skill this is its YAML metadata, and the same extraction works if the opening
`---` is missing. If the file has no separator, its complete contents are sent.
The full `SKILL.md` is loaded when selected, and referenced text resources are
loaded only when needed.

Discovery locations, in priority order:

1. `.agents/skills` from the selected directory up to its Git repository root.
2. `~/.agents/skills`.
3. `$CODEX_HOME/skills`, or `~/.codex/skills`.
4. Extra directories from `CODECRAB_SKILLS_DIR` (using the platform path-list
   separator).

Project skills shadow later user skills with the same name. Invalid skills are
skipped and reported by `codecrab skills`.

## Configuration

See [`codecrab.example.toml`](codecrab.example.toml). The config directory is
platform-specific and can be located from the comment at the top of that file.

Environment variables override the file:

| Variable | Meaning |
| --- | --- |
| `CODECRAB_PROVIDER` | Active provider profile for new sessions |
| `CODECRAB_MODEL` | Active profile's model (`auto` selects CodeCrab's catalog-backed default) |
| `CODECRAB_BASE_URL` | Active profile's OpenAI-compatible `/v1` base URL |
| `CODECRAB_AUTH` | Active profile's `auto`, `oauth`, `api_key`, or `none` mode |
| `CODECRAB_API_KEY` | Active profile's API key for this process only |
| `CODECRAB_SKILLS_DIR` | Extra skill directories, separated like `PATH` |

CLI `--model` and `--base-url` flags have the highest priority. Run
`codecrab config` to print two clearly labelled sections: the platform-global
configuration file path and the effective non-secret configuration content.

`request_timeout_seconds` is the maximum time a model request may receive no
new response data. Every streamed chunk resets the timer, so a response may run
for longer than this value while it remains active. CodeCrab retries model
timeouts and other request errors up to five times, showing and persisting each
retry; a terminal failure is persisted in the session and logged to `stderr`.

### ChatGPT Plus/Pro authentication

```console
codecrab auth login
codecrab auth status
codecrab auth logout
```

`auth login` opens OpenAI's browser authorization flow. CodeCrab uses OAuth
PKCE and stores the access token, refresh token, and metadata as separate
entries in the operating system credential store. Tokens are never written
inside the project.

The default OpenAI profile's `auth = "auto"` chooses the ChatGPT subscription
whenever a CodeCrab OAuth login exists. Use `auth = "oauth"` to require
subscription authentication or `auth = "api_key"` to force usage-based API
authentication.

The subscription path uses OpenAI's Codex Responses backend and your ChatGPT
plan's Codex allowance. It does not silently fall back to an API key when OAuth
is selected.

### API-key and compatible providers

Provider profiles keep their model, OpenAI-compatible base URL, authentication
mode, and API key together. API keys are deliberately stored as plain text in
the platform-global `config.toml`; protect that file using the normal
permissions of your operating-system account. Keys are never copied into
sessions or returned by `codecrab config`, provider listings, or the web API.

Manage profiles from the CLI:

```console
codecrab provider add example --base-url https://provider.example/v1 --model provider-model-name
codecrab provider list
codecrab provider show example
codecrab provider use example
codecrab provider remove example
```

`provider add` prompts for the API key without echo when it is omitted. For
automation, use `--api-key-stdin`; `--api-key` is also accepted but can expose
the key in shell history and process listings. Provider management is also
available through `/providers` and `/provider ...` in the terminal UI and the
Providers dialog in the web client.

Each session records its provider name, model, reasoning, and service tier.
Resuming therefore uses the profile's current key without copying the secret
into session JSON. Removing an active profile is rejected; removing a profile
that still has sessions makes those sessions unavailable until that profile is
created again.

For a trusted local provider that requires no Authorization header, use
`auth = "none"`. `CODECRAB_API_KEY` can temporarily override the active
profile's saved key for one process.

## Execution model

Relative file-tool paths resolve from the selected working directory, while
parent paths, absolute paths, other drives, and symbolic links are allowed.
Reads, writes, edits, and shell commands run immediately without confirmation
in terminal, one-shot, resume, and web modes.

OAuth tokens are stored in the OS credential manager and split across secure
entries where platform size limits require it. `codecrab auth logout` removes
only CodeCrab's copy; it does not sign the official Codex CLI out.

CodeCrab does not sandbox or impose a filesystem boundary on the agent. Its
effective access is exactly the access granted to the operating-system user
running the process. Only run it where fully autonomous execution is
acceptable.

Voice dictation deliberately uses ChatGPT's private subscription-backed
`/backend-api/transcribe` service, matching the installed Codex/ChatGPT desktop
client, rather than the separately billed public Audio API. This endpoint is
not a documented public integration contract and may need updating when the
desktop client changes.

Sessions live under `.codecrab/sessions/` in each project and are ignored by
the included `.gitignore`. CodeCrab maintains `session_directories` in the
platform-global `config.toml`, which lets terminal commands, `/sessions`, and
the web sidebar discover every project that has saved sessions. Empty projects
are removed from that registry. The field is managed automatically and can
also be edited manually.

## Architecture

```text
TUI ───────────────┐
                   v
embedded web ──> JSON API ──> agent loop ──> OpenAI-compatible model
                         ^          |
                         |       tool calls
                         |          v
                  session store <─ ToolBox
                                   |
                        read/search | write/shell
```

The code is intentionally split by responsibility:

- `agent.rs`: model/tool loop and system policy.
- `completion.rs`: shared slash, skill, and filesystem completion engine.
- `events.rs`: ordered assistant-message events plus persisted tool activity
  and lifecycle labels shared by clients.
- `provider.rs`: Chat Completions and ChatGPT Responses protocols.
- `http_debug.rs`: explicit unredacted HTTP request/response tracing.
- `audio.rs`: native microphone capture and WAV encoding for the terminal.
- `transcription.rs`: subscription-backed ChatGPT voice transcription.
- `auth.rs`: OAuth PKCE, token refresh, and secure credential storage.
- `tools.rs`: project-scoped file and shell capabilities.
- `session.rs`: persistence and resume.
- `skills.rs`: Agent Skills discovery, validation, activation, and resources.
- `ui.rs`: responsive terminal UI and composer.
- `server.rs`: Axum API and embedded static asset server.
- `web/`: Vue and Tailwind source application.
- `build.rs`: web build and exact three-asset embedding.
- `events.rs`: async events shared by the agent and terminal UI.
- `config.rs`: layered configuration.

## Development

```console
npm --prefix web install
npm --prefix web run build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo build` also runs the web build automatically. On a fresh checkout,
`build.rs` runs `npm ci`; subsequent Rust builds reuse `web/node_modules`.
Production output is required to contain exactly `index.html`, `app.js`, and
`app.css`, and those files are embedded into the executable at compile time.

Good next steps are token-aware context compaction, unified diffs, and a
provider trait with native Anthropic/Gemini adapters. The current module
boundaries are designed so those can be added without rewriting the agent core.
