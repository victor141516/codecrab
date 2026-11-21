<p align="center">
  <img src="assets/codecrab-agent.png" alt="CodeCrab secret-agent crab mascot" width="128" height="128">
</p>

<h1 align="center">CodeCrab</h1>

<p align="center">
  <strong>A small, auditable coding agent for your terminal and browser, written in Rust.</strong>
</p>

<p align="center">
  One native binary that inspects code, edits files, runs commands and verifies the result until the task is finished.
</p>

## Say It, Ship It

Describe the outcome you want in plain language, for example `Find and fix the failing tests`.
CodeCrab inspects the project, makes changes, runs checks, and summarizes the result. The same
autonomous agent powers a terminal UI, one-shot CLI commands, and an embedded web app.

## Features

- 🤖 Autonomous. No approval prompts, artificial tool-round limits, or hidden iteration cap.
- 💬 ChatGPT login. Use your ChatGPT Plus/Pro subscription through browser-based OAuth, or configure an API key or an OpenAI-compatible provider, including local endpoints.
- 🖥️ Shared surfaces. The full-screen terminal UI, the one-shot CLI, the browser app and the streaming HTTP/NDJSON API all drive the same agent.
- 🛠️ Real tools. Read, write, replace, search, run non-interactive commands, and control persistent PTYs.
- 💾 Persistent sessions. Browse, resume, rename, pin, archive and branch conversations across every registered project.
- 🎯 Long-running work. Persistent goals, scheduled cron tasks and delegated child sessions keep going across turns.
- 🦀 One binary. The Vue web application and syntax highlighting are embedded; Node.js is not needed at runtime.

The complete feature list and a comparison with OpenAI Codex are in [docs/features.md](docs/features.md).

## Use It

| Command | Action |
| --- | --- |
| `codecrab auth login` | Sign in with the OpenAI account that owns your ChatGPT Plus or Pro subscription |
| `codecrab` | Open the interactive terminal UI in the current directory |
| `codecrab -C path/to/project` | Open another directory without changing your shell's working directory |
| `codecrab resume [<session-id-or-prefix>]` | Resume the most recently updated session, or a specific one |
| `codecrab run "Find and fix the failing test"` | Run one task, print the final answer, and exit |
| `git diff \| codecrab run "Review this diff for correctness"` | Combine a prompt with piped context |
| `codecrab serve` | Serve the web app at `http://127.0.0.1:4096/` and `https://127.0.0.1:4097/` (self-signed), with the API under `/api` |
| `codecrab sessions` / `skills` / `providers` / `config` | List sessions, skills, provider profiles, or the effective configuration |
| `codecrab cron` | Run the scheduler for the platform-global `cron.json` |

In the terminal, `/` completes commands and skills, `@path` references a file or directory, and
`/help` or `F1` shows the complete keyboard reference. The server has no built-in HTTP
authentication: keep it on localhost or behind an authenticated gateway.

More: [getting started](docs/getting-started.md) ·
[interfaces](docs/interfaces.md) ·
[workflows](docs/workflows.md) (sessions, attachments, commands, cron, branches, goals, delegation, compaction) ·
[authentication and providers](docs/authentication.md) ·
[skills and project instructions](docs/skills.md) ·
[configuration](docs/configuration.md) ·
[filesystem and shell access](docs/filesystem-and-shell.md) ·
[voice dictation](docs/voice-dictation.md) ·
[HTTP API](docs/http-api.md) ·
[diagnostics](docs/diagnostics.md)

## Install

Download a binary for Windows, macOS, or Linux from the
[latest GitHub Release](https://github.com/victor141516/codecrab/releases/latest), rename it to
`codecrab` (`codecrab.exe` on Windows), and put it on your `PATH`. On macOS or Linux, make it
executable first:

```bash
chmod +x codecrab
```

Then run `codecrab auth login` once and start `codecrab` inside a project.

## Build

A source build requires a recent Rust toolchain with Rust 2024 edition support and Node.js 20.19+
or 22.12+. Linux also needs ALSA development headers and `pkg-config` (for example,
`libasound2-dev` and `pkg-config` on Debian/Ubuntu).

```bash
git clone https://github.com/victor141516/codecrab.git
cd codecrab
cargo install --path .
```

Rust builds invoke `build.rs`. On a fresh checkout it runs `npm ci`, builds the frontend, verifies
that production output contains exactly `index.html`, `app.js`, and `app.css`, and embeds those
assets in the executable.

## Development

Same prerequisites as the build. Run the project checks:

```bash
npm --prefix web ci
npm --prefix web test
npm --prefix web run build
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

The code layout is described in [docs/architecture.md](docs/architecture.md). CodeCrab is
available under the [MIT License](LICENSE).

## Note

> [!IMPORTANT]
> CodeCrab is intentionally autonomous. It runs file changes and commands without approval prompts, has the same filesystem access as your operating-system account, and is not confined to the selected project. Use it only where you are comfortable granting that access.
