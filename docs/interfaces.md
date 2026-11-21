# Choose your interface

All three modes use the same agent, providers, tools, skills, instructions, sessions, and model-selection behavior. Session lists include a global **No project** group followed by project groups, and each group is arranged as a tree: roots and siblings are ordered by creation time, newest first, while children stay beneath their parent. Activity and metadata updates do not move an existing session. Pinned shortcuts appear first in each project, ordered by pin time, while archived trees live in a collapsed section ordered by archive time. The terminal picker shows creation (`C`) and last-update (`U`) times, while the denser web sidebar keeps each session to one title row. `codecrab sessions` prints `PARENT`, `PINNED`, `ARCHIVED`, `CREATED`, and `UPDATED` columns. `codecrab resume` without an ID still considers archived sessions and resumes the most recently updated session.

## Interactive terminal

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
| `Tab` | Complete the selected autocomplete item; otherwise focus the next visible terminal control |
| `Shift+Tab` | Focus the previous visible terminal control |
| `Space` | Activate the focused terminal control; insert a space while the composer is focused |
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

## One-shot CLI

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

## Web app and API

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
