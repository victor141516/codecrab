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
- Completed turns in both clients collapse their intermediate progress into a
  one-line duration and operation summary; the final answer remains visible
  and the complete progress can be expanded again. The terminal summary is a
  mouse-only control.
- Overflowing web tool rows can be expanded to reveal their complete command,
  path, query, or other recorded detail.
- Concise progress messages appear in the same language as the user's latest
  message and stay interleaved with the actions they describe.
- Persisted event sequencing keeps streamed progress, tool activity, retries,
  and final responses in their exact chronological order in both clients.
- Automatic conversation scrolling pauses when the user scrolls upward and
  resumes only after they return to the bottom, even while new output streams.
- Conversation history is stored as a tree. `/branches` opens a compact,
  reversible branch preview beside the transcript in both clients.
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
- Personal instructions from `~/.config/codecrab/AGENTS.md` plus complete
  project instructions from `AGENTS.md` in the selected project root.
- One-shot/pipe-friendly operation with `codecrab run`.
- OpenAI-compatible Chat Completions providers.
- Browser login with a ChatGPT Plus/Pro subscription—no API key or separate API
  billing required.
- Voice dictation through the signed-in ChatGPT subscription in terminal and
  web composers; transcripts are inserted for review instead of auto-sent.
- OAuth PKCE, automatic token refresh, and global TOML persistence under
  `~/.config/codecrab/`.
- Model tools for listing, reading, searching, writing, exact editing,
  non-interactive commands, and persistent interactive terminal sessions.
- Relative, parent, and absolute paths across the filesystem.
- Autonomous file mutations, shell commands, and managed PTY interaction in
  every execution mode.
- Unlimited model/tool rounds per turn, continuing until the model returns a
  final answer or an actual provider/tool error occurs.
- Persistent, session-scoped goals with automatic cross-turn continuation,
  explicit model-reported completion/blocking, pause/resume/edit/delete
  controls, and a history containing at most one active goal.
- Project-local JSON sessions with a global cross-project browser, resume, and
  deletion.
- Concurrent session workers in one process: a turn keeps running after
  navigation, another session can start immediately, and Stop/model/goals are
  routed to the selected session only.
- Agent-to-agent delegation through persistent child sessions with isolated
  context, live status/message observation, follow-up, waiting, and exact-turn
  cancellation.
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

Pushing a Git tag whose name starts with `v` immediately creates a draft
release and starts every platform build in parallel. Each binary is attached
as soon as its build finishes; the draft is published after the complete build
matrix finishes and the required x64 binaries are present. The tag should match
the package version, for example:

```console
git tag -a v1.2.0 -m "CodeCrab v1.2.0"
git push origin v1.2.0
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

One process serves the same frontend, API routes, streaming responses, and
conversation state over HTTP and HTTPS. The default origins are
`http://127.0.0.1:4096` and `https://127.0.0.1:4097`. Use automatically
selected ports for both listeners with:

```console
codecrab serve --port 0 --https-port 0
```

The command prints the HTTP and HTTPS API and frontend origins after both
listeners bind, including the actual ports selected for `0`.

CodeCrab generates a fresh self-signed certificate and private key at every
`serve` startup. Both remain in memory only and are discarded when the process
exits. The certificate covers `localhost`, `127.0.0.1`, `::1`, and a configured
concrete host or IP when applicable. Browsers show a certificate warning unless
that execution's certificate is explicitly trusted, and its fingerprint changes
on every restart.

Press `Ctrl+C` to start a graceful shutdown of both listeners. If active
HTTP/HTTPS requests or open connections still prevent exit after 100 ms,
CodeCrab explains what it is waiting for on `stderr`. Press `Ctrl+C` a second
time to terminate the process immediately.

The frontend always calls relative `/api` URLs, so it automatically uses the
same scheme, hostname, port, and reverse-proxy origin that served it. To listen
outside localhost:

```console
codecrab serve --host 0.0.0.0 --port 4096 --https-port 4097
```

Web mode allows file mutations and shell commands without confirmation. This
server has no built-in authentication on either HTTP or HTTPS; it is intended
for local use or deployment behind an authenticated gateway. HTTPS encrypts the
connection but does not authenticate users.

The initial API surface is:

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/health` | Process health and version |
| `GET` | `/api/state` | Current session, grouped projects/sessions, models, and skills |
| `POST` | `/api/completions` | Shared slash, skill, and filesystem suggestions for `session_id` |
| `POST` | `/api/completions/recursive` | Progressive NDJSON batches for the same identified filesystem query |
| `POST` | `/api/chat` | Run a prompt for `session_id` as an ordered NDJSON stream; optional `edit_node_id` branches from an existing user message |
| `POST` | `/api/chat/cancel` | Cancel only `session_id` and its in-flight provider/tool operation |
| `POST` | `/api/transcribe` | Transcribe audio for the `X-CodeCrab-Session` session |
| `PUT` | `/api/model` | Change model, thinking, and service tier for `session_id` |
| `POST` | `/api/session/clear` | Clear `session_id` |
| `POST` | `/api/branches/preview` | Preview the oldest leaf descending from `{ session_id, node_id }` without persistence |
| `POST` | `/api/branches/select` | Select and persist the oldest leaf descending from `{ session_id, node_id }` |
| `POST` | `/api/sessions` | Create and select a session in `{ project }` |
| `POST` | `/api/sessions/delete` | Delete `{ project?, id }`; select the next saved session when active |
| `POST` | `/api/sessions/resume` | Resume `{ project?, id }`, resolving the project globally when omitted |
| `GET` | `/api/directories` | Browse server directories from optional `path` |
| `POST` | `/api/directories` | Create one server directory from `{ parent, name }` without opening it |
| `POST` | `/api/projects/open` | Open and persist an existing server directory from `{ path }` |
| `POST` | `/api/goals/create` | Create and activate a goal in `session_id`, pausing its previous active goal |
| `PUT` | `/api/goals/edit` | Replace a goal objective |
| `POST` | `/api/goals/activate` | Activate a saved goal and pause any other active goal |
| `POST` | `/api/goals/pause` | Pause a goal |
| `POST` | `/api/goals/delete` | Delete a goal |

### Diagnostics and raw OpenAI HTTP debugging

During an interactive terminal session, model request and compaction errors are
written to an execution-specific log under the operating system's temporary
directory instead of `stderr`, so they cannot corrupt the TUI. The file is
created only after the first error. After the TUI restores the terminal,
CodeCrab prints its path and explains how to select another location:

```console
codecrab --error-log ./codecrab-errors.log
```

The explicit path is appended to and is also created lazily. Non-interactive
commands and the web server continue to report these diagnostics on `stderr`.

Pass `--debug-openai` in any mode to write every OpenAI model/catalog request,
OAuth token request, and corresponding response to `stderr`:

```console
codecrab --debug-openai serve
codecrab --debug-openai run "Say hello"
```

To keep that output away from the TUI, attach an output path to the flag with
`=`:

```console
codecrab --debug-openai=./openai-debug.log
```

The `=` is required so the optional path cannot consume a subcommand or prompt.
The selected file is opened lazily and appended to. An explicit debug file
error is surfaced and never falls back silently to `stderr`.

This output is intentionally unredacted. It can contain access and refresh
tokens, authorization codes, prompts, tool arguments, repository content, and
model responses. Store or forward it only when that disclosure is acceptable.

The terminal UI adapts to narrow terminals. Its borderless, full-width
conversation supports `Page Up`, `Page Down`, and mouse-wheel scrolling. The
header shows the current model and thinking level, plus a lightning bolt only
when the selected provider service tier is Fast. The web client keeps speed in
its dedicated selector and does not add a bolt to the model name. Terminal
file activity uses paths relative to the active project whenever possible.
On terminals that support the enhanced keyboard protocol, CodeCrab enables it
to preserve modified keys such as `Shift+Enter`; portable fallbacks remain
available for older macOS terminals. Assistant Markdown keeps its original
delimiters visible while adding terminal colors and supported font styles for headings, bold, italic, inline code,
links, quotes, and list markers. Fenced code blocks use embedded, language-aware
syntax highlighting; CodeCrab does not require or spawn `bat`.

Keyboard shortcuts:

| Key | Action |
| --- | --- |
| `Enter` | Complete a selection, send text, or stop, transcribe, and send an active recording |
| `Tab` | Complete the selected menu item |
| `Shift+Enter`, `Alt+Enter`, or `Ctrl+J` | Insert a newline (`Alt+Enter` or `Ctrl+J` on macOS terminals that cannot report `Shift+Enter`) |
| `Ctrl+Shift+S` | Start or stop voice dictation (`Ctrl+S` on macOS) |
| `Up` / `Down` | Navigate an open menu; otherwise move between editor rows. `Up` recalls the latest visible user message when the idle composer is empty |
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
newlines into the prompt. Up/down navigation uses those visual rows and never
scrolls the conversation. With an empty idle composer, `Up` recalls only the
latest visible user message; `Esc` cancels that staged edit, while submitting
creates and selects a conversation branch. The web composer provides the same
recall behavior. Word and line editing consumes Crossterm's decoded terminal
events and traditional Readline sequences rather than checking the operating
system; terminal-level remappings that emit the same sequences therefore keep
working.

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
/branches   browse and preview conversation branches
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

`/branches` opens the conversation tree only when requested. Each visible user
message is a node; selecting any node previews the complete path through the
oldest descendant leaf and keeps that message in view. In the terminal, arrows
move between nodes in their rendered tree order, `E` loads the selected user
message into the composer, `Enter` keeps the preview, and `Esc` restores the
original branch and scroll position. Active terminal edges stay coral across
intervening sibling subtrees instead of inheriting the color of those rows. The
web panel uses the same branch-selection semantics with clickable nodes, a
check action, and a cancel action. Each web user message also has a
pencil action that opens an inline editor, including while the branch panel is
open on a previewed route. The panel remains visible during editing, but its
navigation is paused until the edit is saved or cancelled. Submitting an edit
creates and activates a new sibling node, runs the agent from the edited
prompt, keeps the original message plus its complete continuation available in
the tree, and rebases the still-open panel onto the resulting branch. The
previewed route is coral, while the route that was active when the panel opened
remains cyan where it diverges. Hovering a web tree node highlights its message
on the displayed route and reveals it through the virtual timeline even when
the row is not currently mounted; keyboard focus has the same behavior. Hover
and focus never change branches; clicking a node does. The normal composer is
disabled during preview or inline editing so a prompt cannot be sent to a
branch other than the one currently shown.

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
Switching sessions does not cancel a running turn. The session picker shows
each live worker as running, idle, stopping, or failed; returning to a running
session restores its live event stream and queued follow-up.
The web sidebar keeps every registered project visible as an independently
collapsible tree with its sessions nested below. Each project row creates a new
session in that exact directory. The top action opens a server-side directory
browser, so remote browsers and phones inspect the filesystem of the machine
running CodeCrab rather than their own; it can create a directory without
opening it, and opening an empty project persists it without creating a session.
Selecting a session updates the browser URL to `/sessions/<id>`. Opening or
sharing that URL resolves the project globally and restores the session without
relying on browser storage, even when the server was started from a different
project. Deleting the active web session selects the next saved session in that
project and replaces the URL accordingly. If none remain, no replacement is
created: the URL returns to `/` and the project remains visible and empty.

Unsent web composer text is browser-local and scoped to the normalized project
path plus session ID. Switching sessions restores each session's exact draft,
including whitespace and line breaks; creating a session focuses its composer,
successful sends and session deletion remove the saved draft, and storage
failures do not prevent navigation. Browser storage is never used to resolve a
session URL or choose the active project.

Typing `@` immediately opens files and folders from the selected directory.
Direct entries match the final fragment case-insensitively anywhere in the
filename, with exact and prefix matches first. After two query characters,
CodeCrab lazily searches descendants and streams fuzzy matches into the same
menu while preserving the highlighted item. Recursive rows include their
relative path context and insert the complete path; accepting a directory still
enters it and refreshes the menu.

Use `@../` for the project parent, `@/` for the current drive/filesystem root,
and continue with `@../../`, absolute paths, or drive-qualified paths using
normal platform semantics. Bare `@` and one-character queries show direct
children only, so a filesystem root is never scanned automatically. Recursive
work is capped at 80 results, 12,000 visited entries, 768 directories, ten
levels, and 750 ms, with at most two scans process-wide; the immediate phase
examines at most 4,096 entries. Directory symlinks are shown but not followed
recursively. `.git`, `target`, `node_modules`, and `dist` trees are skipped;
other hidden or VCS-ignored paths are not implicitly filtered. Permission
errors and disappearing entries are skipped without closing the menu.

Selecting a file inserts its `@path` without sending the prompt. The compact
menu assumes a Nerd Font and renders a type-specific icon; directories remain
visually distinct.

## Agent Skills

When a conversation starts, CodeCrab reads personal instructions from
`~/.config/codecrab/AGENTS.md` and project instructions from `AGENTS.md` in
the selected project root. The personal file is optional: missing,
non-regular, and whitespace-only candidates are ignored, while read errors are
reported without preventing the project instructions or conversation from
loading. Project-file read errors remain startup errors.

When both files exist, CodeCrab sends them as one contextual user message,
with the trimmed personal contents first, then
`--- project-doc ---`, then the complete project file. If both paths resolve
to the same file, it is included only once. This contextual message precedes
the visible conversation but is never rendered or persisted as a
user-authored message; the stable CodeCrab policy remains the provider system
instruction. Normal requests, token estimation, and compaction all use this
same projection.

Instructions are snapshotted when an agent is constructed. Editing either
file affects new conversations, cold-resumed sessions, and project switches,
but does not change an already-running agent.

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

See [`codecrab.example.toml`](codecrab.example.toml). On every supported
platform, the global configuration file is
`~/.config/codecrab/config.toml`, resolved from the user's home directory.

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
`codecrab config` to print two clearly labelled sections: the global
configuration file path and the effective non-secret configuration content.

### Manual model catalogs and capabilities

Provider catalogs are discovered from `GET /models` by default. The global
`config.toml` can enrich discovered models, declare models omitted by the
endpoint, disable catalog discovery, and restrict a provider to an explicit
model list. These settings must be edited directly in TOML; the CLI, terminal
UI, and web provider-management interfaces intentionally do not edit them.

Model settings belong to the provider profile and are keyed by the exact model
ID sent to the provider:

```toml
[providers.example]
model = "auto"
base_url = "https://provider.example/v1"
auth = "api_key"
api_key = "provider-secret"

[providers.example.model_capabilities."vendor/model-pro"]
display_name = "Model Pro"
description = "General-purpose multimodal model"
default_reasoning_level = "high"
default_service_tier = "priority"
input_modalities = ["text", "image"]
output_modalities = ["text"]
context_window_tokens = 200000
maximum_output_tokens = 32000
auto_compact_token_limit = 150000

reasoning_levels = [
    { id = "low" },
    { id = "medium", name = "Balanced" },
    { id = "high", name = "Deep", description = "More reasoning for complex tasks" },
]

service_tiers = [
    { id = "priority", name = "Fast", description = "Priority processing" },
]
```

Quote model IDs in table names because IDs commonly contain `/`, `.`, or `:`.
Declaring a `model_capabilities` table adds that model even when `GET /models`
does not return it. `display_name`, `description`, defaults, reasoning levels,
service tiers, modalities, context window, maximum output, and auto-compaction
limit are all optional, so the smallest useful manual model can be as simple
as:

```toml
[providers.example.model_capabilities."hidden-model"]
input_modalities = ["text"]
output_modalities = ["text"]
```

Reasoning levels and service tiers use the same option shape:

```toml
reasoning_levels = [
    { id = "low" },
    { id = "high", name = "Deep", description = "Maximum reasoning effort" },
]
service_tiers = [
    { id = "priority", name = "Fast" },
]
```

Only `id` is required. It is the literal value sent to the provider. If `name`
is omitted, CodeCrab displays the ID; if `description` is omitted, it uses an
empty description. This is why Fast is configured as a service tier rather than
as `fast = true`: the provider may expect an ID such as `priority`, while users
should see a name such as `Fast`.

Manual capabilities merge additively with the discovered entry:

- reasoning levels and service tiers merge by `id`;
- input and output modalities are appended without duplicates;
- manually supplied `name` and `description` values replace those fields on a
  matching option, while omitted fields preserve the endpoint values;
- manually supplied scalar fields such as `display_name`,
  `default_reasoning_level`, and `default_service_tier` replace the discovered
  values;
- discovered model order is preserved, and models found only in the
  configuration are added after them.

A configured default reasoning level or service tier must exist in the final
merged options for that model. Duplicate or empty IDs and modalities are
rejected when the configuration is loaded.

For an API that does not expose `GET /models`, disable discovery and declare the
complete catalog manually:

```toml
[providers.local]
model = "local-model"
base_url = "http://localhost:11434/v1"
auth = "none"
api_key = ""
fetch_models = false

[providers.local.model_capabilities."local-model"]
reasoning_levels = [
    { id = "low" },
    { id = "high" },
]
input_modalities = ["text"]
output_modalities = ["text"]
```

When `fetch_models` is omitted or `true`, a failed `/models` request remains a
visible catalog error even if manual models are configured. When it is `false`,
CodeCrab does not make the request and requires the resulting manual catalog to
contain at least one model.

Use `allowed_models` to expose only a closed, ordered subset of a large provider
catalog:

```toml
[providers.example]
model = "auto"
base_url = "https://provider.example/v1"
auth = "api_key"
api_key = "provider-secret"
allowed_models = ["vendor/model-pro", "hidden-model"]

[providers.example.model_capabilities."hidden-model"]
input_modalities = ["text"]
output_modalities = ["text"]
```

Only those IDs appear in model selectors, in the order listed, and only those
models may be used. Every allowed ID must either be returned by `GET /models` or
be declared under `model_capabilities`; otherwise catalog loading fails with an
error. If the provider's `model` is an explicit ID instead of `"auto"`, it must
also be present in `allowed_models`.

Modalities are open string identifiers, allowing values such as `text`,
`image`, or `audio`. They currently describe catalog capabilities only;
declaring image or audio support does not by itself add a corresponding upload
or generation workflow to CodeCrab. See
[`codecrab.example.toml`](codecrab.example.toml) for another complete example.

`request_timeout_seconds` is the maximum time a model request may receive no
new response data. Every streamed chunk resets the timer, so a response may run
for longer than this value while it remains active. CodeCrab retries model
timeouts and other request errors up to five times, showing and persisting each
retry. A terminal failure is persisted in the session and logged to `stderr`
outside the TUI, or to the TUI's lazy error log during an interactive terminal
session.

### Managed terminal sessions

The `shell` model tool always starts the detected user shell inside a PTY
(ConPTY on Windows). Commands that finish within five seconds return their exit
status and normalized combined terminal text. A command still running after
five seconds returns a conversation-scoped terminal ID; the model can then use
semantic text, paste, key, mouse, resize, read, list, and close operations. The
terminal emulator supplies the final rendered screen and style spans instead
of raw ANSI or transient spinner frames.

Use the separate `shell_noninteractive` tool when distinct stdout/stderr is
more useful than terminal behavior. Its timeout is 120 seconds by default and
accepts values up to 300 seconds; a timeout kills the process tree and returns
the partial captured streams.

Set a global `shell` executable in `config.toml`, or temporarily set
`CODECRAB_SHELL`, for deterministic selection. Otherwise Unix uses `$SHELL`
then the account shell, while Windows checks the parent shell and then `pwsh`,
Windows PowerShell, and `%ComSpec%`. Shell profiles are loaded. Managed
terminals use `TERM=xterm-256color`, true color, and UTF-8 where the selected
shell supports it.

Terminal metadata, the latest structured screen, and bounded transcript tail
are saved with the conversation. Live terminals remain available when
switching sessions within one CodeCrab process. They are killed and reaped on
session deletion or normal shutdown; a terminal that was live when CodeCrab
stopped is marked `interrupted` after resume because PTYs cannot be reattached
across process restarts. Graphics protocols and direct user-takeover views are
not supported.

### ChatGPT Plus/Pro authentication

```console
codecrab auth login
codecrab auth status
codecrab auth logout
```

`auth login` opens OpenAI's browser authorization flow. CodeCrab uses OAuth
PKCE and stores the access token, refresh token, and metadata as plain text
under `[chatgpt_oauth]` in `~/.config/codecrab/config.toml`. Protect that file
with the normal permissions of your operating-system account. Tokens are never
written inside the project.

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
`~/.config/codecrab/config.toml`; protect that file using the normal
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
Reads, writes, edits, shell commands, and managed terminal interactions run
immediately without confirmation in terminal, one-shot, resume, and web modes.

OAuth tokens and provider API keys are stored as plain text in the
`~/.config/codecrab/config.toml`. `codecrab auth logout` removes the
`[chatgpt_oauth]` section only; it does not sign the official Codex CLI out.

CodeCrab does not sandbox or impose a filesystem boundary on the agent. Its
effective access is exactly the access granted to the operating-system user
running the process. Only run it where fully autonomous execution is
acceptable.

Voice dictation is available only when the current session uses the official
OpenAI provider. With ChatGPT OAuth it deliberately uses ChatGPT's private
subscription-backed `/backend-api/transcribe` service, matching the installed
Codex/ChatGPT desktop client; with an OpenAI API key it uses the provider's
`/audio/transcriptions` endpoint. The private subscription endpoint is not a
documented public integration contract and may need updating when the desktop
client changes. Compatible providers are disabled by default; internally,
transcription URLs are always derived from the selected provider's `base_url`
so a non-OpenAI profile can never send audio or credentials to OpenAI.

Sessions live under `.codecrab/sessions/` in each project and are ignored by
the included `.gitignore`. CodeCrab maintains `session_directories` in the
global `~/.config/codecrab/config.toml`, which lets terminal commands,
`/sessions`, and the web sidebar discover every opened project, including empty
ones. Projects are registered when opened or when a session is saved and
remain registered until the field is edited manually.

Each session persists one conversation tree with stable node and parent IDs,
plus the selected leaf. The ordinary `messages` field returned by the web API
is only the active root-to-leaf projection; inactive branches remain in
`conversation.nodes` and are never discarded by branch selection.

### Agent-to-agent session delegation

Every agent can control other CodeCrab sessions in the same process through
seven model tools:

| Tool | Contract |
| --- | --- |
| `session_create` | Persist a fresh child with a required visible `prompt`, optional existing `project`, and `parent_session_id`; start its first turn asynchronously and return its stable UUID immediately |
| `session_list` | Discover persisted and live sessions in the current project, an explicit project, or every registered project |
| `session_status` | Read content-free lifecycle, revision, timestamps, visible-message outline, goal state, and current/latest activity metadata |
| `session_messages` | Read bounded, cursor-based visible user/assistant content, including live partial text without hidden or tool-protocol messages |
| `session_send` | Append one visible prompt to an idle target and return when the turn is accepted |
| `session_stop` | Cancel exactly the target's active provider/tool future without deleting or shutting down its reusable worker |
| `session_wait` | Wait up to 60 seconds for one of up to eight observation revisions to change, without hot polling |

A child is a normal session with its own `Agent`, model state, tools, skills,
root `AGENTS.md`, transcript, activities, goals, and worker. It receives only
the delegated prompt and normal system/project context: the parent's
transcript, compaction summary, hidden prompts, and tool history are never
copied. Persisted lineage makes the relationship inspectable, and child
sessions appear in the existing terminal session picker and web sidebar and
remain resumable after restart.

The default policy is deliberately conservative. Explicit requests for another
agent/session/conversation, delegation, parallel work, or independent
validation enable these tools; ordinary tasks do not fan out automatically.
Project or personal `AGENTS.md` instructions may opt into proactive
delegation. A delegated prompt must therefore contain all required context.

Coordination is process-local; there is no cross-process IPC or remote worker
control. All sessions run as the same operating-system account and share the
same filesystem, with no automatic worktree, sandbox, write locking, or
conflict prevention. Delegate disjoint writes or coordinate them explicitly.
Normal shutdown cancels active child turns, persists their terminal outcome,
closes managed terminals, and stops every worker before exit.

### Automatic context compaction

Before model requests, CodeCrab compares the latest provider-reported input
usage plus newly appended messages with the selected model's usable context.
When needed, it asks the active model for one rolling structured summary and
keeps a token-budgeted tail of complete recent turns verbatim. The same check
runs before a user turn, between tool/model rounds, after selecting a smaller
model, and when a provider reports a context-length overflow.

If the whole historical head does not fit in one summary request, CodeCrab
compacts the oldest complete-turn chunk that fits, keeps the omitted later
turns raw, and then rolls additional chunks into the same latest summary until
the normal request is safe. A context-window error from the summarizer retries
with fewer final turns in that chunk.

Large `read_file` results are omitted entirely from summarizer input while
retaining the path, tool-call ID, and size marker. Large contents in
`write_file` and `replace_in_file` arguments are similarly replaced with
markers. The canonical tool calls and results remain untouched, so the agent
can re-read the file when exact contents are needed.

Compaction changes only the projection sent to the provider. Session JSON keeps
the complete original messages, tool calls, results, activities, and ordering.
It also stores every linked compaction checkpoint, its covered message range,
trigger, model settings, summary, and reported token usage. Resuming continues
from the latest checkpoint while terminal and web still display the full
transcript.

Both clients show shared persisted `Context compaction started`, completed, and
failed activities. A failed or cancelled summary never replaces the previous
projection. Compaction is intentionally automatic; there is no normal
`/compact` command.

## Architecture

```text
TUI / run / JSON API / session tools
                 |
                 v
       SessionCoordinator
                 |
                 v
       ConversationManager
   ├── session A ──> ConversationHandle ──> Tokio worker ──> Agent A
   ├── session B ──> ConversationHandle ──> Tokio worker ──> Agent B
   └── session C ──> ConversationHandle ──> Tokio worker ──> Agent C
```

Each persistent conversation worker is the exclusive owner of its `Agent`.
Presentation layers communicate through typed commands, streamed events,
authoritative snapshots, and an out-of-band cancellation signal; they never
lock or borrow an agent while a turn is running. `ConversationManager` prevents
duplicate owners for a session, keeps background workers alive across
navigation, exposes per-session lifecycle state, and shuts down every worker
when the process exits. Workers share the operating-system filesystem and can
therefore conflict if two sessions edit the same project simultaneously.
Persisted sessions are started lazily when opened; once live, an idle worker is
kept until that session is deleted or the process shuts down.

The code is intentionally split by responsibility:

- `agent.rs`: model/tool loop and system policy.
- `compaction/`: context projection, safe turn boundaries, summary prompt, and
  all documented production tuning values.
- `conversation.rs`: multi-worker manager plus persistent Tokio workers that
  serialize per-session mutations and expose commands, live observations, and
  authoritative snapshots.
- `coordination.rs`: acyclic shared agent factory and weak session-control
  facade used by terminal, one-shot, web, and delegated sessions.
- `completion.rs`: shared slash, skill, and filesystem completion engine.
- `events.rs`: ordered assistant-message events plus persisted tool activity
  and lifecycle labels shared by clients.
- `provider.rs`: Chat Completions and ChatGPT Responses protocols.
- `http_debug.rs`: explicit unredacted HTTP request/response tracing.
- `audio.rs`: native microphone capture and WAV encoding for the terminal.
- `transcription.rs`: subscription-backed ChatGPT voice transcription.
- `auth.rs`: OAuth PKCE, token refresh, and global TOML persistence.
- `tools.rs`: project-scoped file capabilities and non-interactive commands.
- `terminal/`: shared PTY lifecycle, terminal emulation, semantic input,
  snapshots, observation, and cleanup.
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

Good next steps are unified diffs and a provider trait with native
Anthropic/Gemini adapters. The current module boundaries are designed so those
can be added without rewriting the agent core.
