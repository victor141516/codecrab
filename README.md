# CodeCrab

CodeCrab is a small, auditable coding agent for the terminal, written in Rust.
It takes the useful core of tools such as Pi and OpenCode—a model/tool loop,
project-aware file operations, shell access, approvals, and resumable sessions—
without starting with a large client/server platform.

This repository contains a working, compact coding agent with a full-screen
terminal interface.

## What works

- Full-screen terminal UI with a scrollable conversation and multiline editor.
- Live working status, compact model context, and in-app approvals.
- Interactive slash autocomplete for built-in commands and installed skills.
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
- OAuth PKCE, automatic token refresh, and OS credential-store integration.
- Model tools for listing, reading, searching, writing, exact editing, and shell
  commands.
- A hard project-root boundary for file tools.
- Explicit approval before writes and commands; `--yes` is available for
  trusted automation.
- Project-local JSON sessions with listing and resume.
- Config file, environment overrides, and CLI overrides.

## Install

You need a recent Rust toolchain.

```console
cargo install --path .
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

One-shot mode denies mutations unless explicitly enabled:

```console
codecrab --yes run "Run the tests and fix the failure"
```

The interactive UI adapts to narrow terminals. Its header shows the current
model and thinking level, plus a lightning bolt when fast mode is selected.

Keyboard shortcuts:

| Key | Action |
| --- | --- |
| `Enter` | Complete the selected menu item, advance the model picker, or send |
| `Tab` | Complete the selected menu item |
| `Shift+Enter`, `Alt+Enter`, or `Ctrl+J` | Insert a newline |
| `Up` / `Down` | Navigate an open menu, otherwise move between editor lines |
| `PgUp` / `PgDn`, mouse wheel | Navigate an open menu, otherwise scroll |
| `Ctrl+U` | Clear the editor |
| `F1` or `?` | Open help |
| `F2` | Show available skills |
| `Ctrl+D` or `Ctrl+C` | Save and quit while idle |

Printable input uses the character resolved by the terminal and active keyboard
layout. This includes `AltGr` combinations on international keyboards; CodeCrab
does not map physical keys such as `2` to layout-specific symbols.

Commands inside the composer:

```text
/help       open keyboard and command help
/model      choose model, reasoning effort, and service speed
/skills     show available Agent Skills
/clear      clear conversation context
/quit       save and exit
```

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

Typing `@` opens files and folders from the selected project directory. Use
`@../` for its parent, `@/` for the filesystem root, and continue selecting
folders with `Enter` to browse deeper. Selecting a file inserts its `@path`
without sending the prompt. The compact menu assumes a Nerd Font and renders
only a type-specific icon followed by the entry name; directories remain
visually distinct. Path fragments use normal platform path semantics, so
`@../../`, drive-qualified paths, and other valid relative or absolute paths
work without special-case parsing.

When CodeCrab wants to edit a file or run a command, the approval request
appears as a modal. Press `Enter`/`Y` to allow it or `N`/`Esc` to deny it.

## Agent Skills

When a conversation starts, CodeCrab reads `AGENTS.md` from the selected
project root and includes its complete contents in the system prompt. If the
file does not exist, startup continues without project-specific instructions.
Read errors are reported instead of silently ignoring the file.

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
skipped and reported by `codecrab skills`. Resource reads are restricted to
the selected skill directory; scripts still go through the normal shell
approval boundary.

## Configuration

See [`codecrab.example.toml`](codecrab.example.toml). The config directory is
platform-specific and can be located from the comment at the top of that file.

Environment variables override the file:

| Variable | Meaning |
| --- | --- |
| `CODECRAB_MODEL` | Model name (`auto` selects the catalog default) |
| `CODECRAB_BASE_URL` | OpenAI-compatible `/v1` base URL |
| `CODECRAB_AUTH` | `auto`, `oauth`, or `api_key` |
| `CODECRAB_API_KEY_ENV` | Name of the environment variable holding the key |
| `CODECRAB_SKILLS_DIR` | Extra skill directories, separated like `PATH` |

CLI `--model` and `--base-url` flags have the highest priority. Run
`codecrab config` to inspect the effective non-secret configuration.

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

The default `auth = "auto"` chooses the ChatGPT subscription whenever a
CodeCrab OAuth login exists. Use `auth = "oauth"` to require subscription
authentication or `auth = "api_key"` to force usage-based API authentication.

The subscription path uses OpenAI's Codex Responses backend and your ChatGPT
plan's Codex allowance. It does not silently fall back to an API key when OAuth
is selected.

### API-key and compatible providers

API keys remain supported for automation or another OpenAI-compatible
provider. Set the environment variable named by `api_key_env`:

```powershell
$env:OPENAI_API_KEY = "sk-..."
```

For another compatible provider, for example:

```console
codecrab --base-url https://provider.example/v1 --model provider-model-name
```

Set `CODECRAB_API_KEY_ENV` to the name of the variable containing that
provider's token. For a trusted local provider that requires no Authorization
header, set `auth = "api_key"` and `api_key_env = ""` in the config file.

## Safety model

Read-only tools can only resolve paths below the selected project root.
Mutating tools and shell commands require a confirmation in interactive mode.
In one-shot mode they are denied by default, because there is no reliable
interactive approval channel. `--yes` deliberately opts out of those prompts.

OAuth tokens are stored in the OS credential manager and split across secure
entries where platform size limits require it. `codecrab auth logout` removes
only CodeCrab's copy; it does not sign the official Codex CLI out.

Shell commands are still powerful: approval is a boundary, not a sandbox.
Review commands before accepting them, especially in untrusted repositories.

Sessions live under `.codecrab/sessions/` in each project and are ignored by
the included `.gitignore`.

## Architecture

```text
CLI / prompt
     |
     v
agent loop -----> OpenAI-compatible model
     ^                    |
     |                 tool calls
     |                    v
session store <----- project ToolBox
                         |
              read/search | write/shell + approval
```

The code is intentionally split by responsibility:

- `agent.rs`: model/tool loop and system policy.
- `provider.rs`: Chat Completions and ChatGPT Responses protocols.
- `auth.rs`: OAuth PKCE, token refresh, and secure credential storage.
- `tools.rs`: project-scoped capabilities and approval boundary.
- `session.rs`: persistence and resume.
- `skills.rs`: Agent Skills discovery, validation, activation, and resources.
- `ui.rs`: responsive terminal UI, composer, and approval dialogs.
- `events.rs`: async events shared by the agent and terminal UI.
- `config.rs`: layered configuration.

## Development

```console
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Good next steps are token-aware context compaction, streaming, unified diffs,
and a provider trait with native Anthropic/Gemini adapters. The current module
boundaries are designed so those can be added without rewriting the agent
core.
