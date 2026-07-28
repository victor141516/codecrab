# CodeCrab Agent Guide

## Purpose

CodeCrab is a compact coding agent written in Rust. It offers the same agent
core through a Ratatui terminal UI, one-shot CLI commands, and an Axum JSON API
with an embedded Vue/Tailwind frontend.

The project is under active development and has no external users or backwards
compatibility requirements. Prefer deleting obsolete code over preserving
fallbacks, aliases, migrations, or compatibility layers that are no longer
needed.

## Product invariants

- All execution modes are autonomous. File writes, edits, and shell commands
  run without approval prompts.
- Agent turns have no tool-round limit. Continue the model/tool loop until the
  model returns a final response or a real provider/tool error occurs; do not
  reintroduce a configurable or hidden iteration cap.
- `request_timeout_seconds` is a model-response inactivity timeout, not a total
  request deadline. Reset it for every received response chunk. Retry model
  timeouts and errors at most five times, emitting and persisting each retry;
  persist the terminal error and write it to stderr when all retries fail.
- CodeCrab does not provide a sandbox or filesystem boundary. Relative paths
  resolve from the selected working directory, but parent paths, absolute
  paths, other drives, and symbolic links are valid. The operating-system
  account running the process is the only permission boundary. Do not
  reintroduce approval modes, a `--yes` flag, path confinement, or artificial
  file visibility filters unless the user explicitly changes this policy.
- The web server has no built-in HTTP authentication. It is intended for local
  use or deployment behind an external authenticated gateway.
- ChatGPT OAuth is a first-class authentication path and must work with a
  ChatGPT Plus/Pro subscription without requiring an API key. API-key and
  OpenAI-compatible providers remain supported.
- New conversations prefer GPT-5.6 Sol with high reasoning and Fast speed.
  Resolve the model capability and Fast service-tier identifier from the
  provider catalog; if the target is unavailable, use the provider's first
  model and declared defaults. Do not hardcode other current model metadata or
  synthesize model/variant combinations that the provider did not return.
- The TUI, one-shot mode, and web API must share the same `Agent`, provider,
  tools, skills, model-selection, and session behavior. Avoid implementing
  separate agent semantics in a presentation layer.
- The terminal and web clients must maintain 100% feature parity: every
  operation possible from one client must be possible from the other. Add or
  change both surfaces in the same change, while adapting presentation to each
  medium. Shared behavior and suggestion generation belong in Rust core code;
  web endpoints should expose that code rather than reimplementing it in
  JavaScript.
- Tool activity is structured core state, not presentation-layer log parsing.
  Every tool call must create and persist an `AgentActivity`, emit its running
  and terminal status to the active client, and remain visible after resume.
- Enable provider parallel tool calls so independent reads, searches, and skill
  loads can share one model response. Distinct-file writes may share that
  response, but a second write to the same resolved path must be deferred.
  Treat `shell` as a response barrier: the model must observe one command's
  output before requesting any later operation.
- Every visible assistant response and activity receives one monotonically
  increasing persisted event sequence. Both clients must order a turn by that
  sequence rather than collection position or tool-call matching alone, so
  tool-only model responses, retries, streaming, and resume retain their exact
  chronology.
- The system prompt has a stable communication policy: respond in the language
  of the latest user message and provide concise, user-facing progress before
  the first tool call and at material phase changes. Progress is normal
  assistant content, not hidden reasoning, and must be persisted and streamed
  in order with tool activity. Runtime context is regenerated for the selected
  project and includes OS, CPU architecture, and the full working directory;
  do not add the account username without a concrete agent need.
- `--debug-openai` intentionally logs complete, unredacted OpenAI and OAuth
  requests and responses to stderr. Do not silently redact or weaken this
  explicit debugging mode.

## Repository layout

- `src/main.rs`: CLI parsing and mode dispatch.
- `src/agent.rs`: model/tool loop, system prompt, and root `AGENTS.md` loading.
- `src/compaction/`: context projection, summary construction, safe boundaries,
  and centralized compaction tuning.
- `src/conversation.rs`: persistent Tokio conversation worker, typed command
  handle, cancellation, authoritative snapshots, and serialized persistence.
- `src/completion.rs`: shared slash-command, skill, and filesystem completion
  generation used directly by the TUI and exposed to the web client by the API.
- `src/events.rs`: shared assistant-message and tool-activity events plus
  persisted human-readable tool lifecycle state.
- `src/provider.rs`: ChatGPT Responses and OpenAI-compatible Chat Completions
  protocols, model-catalog parsing, and model selection.
- `src/auth.rs`: ChatGPT OAuth PKCE, refresh, and credential-store integration.
- `src/tools.rs`: filesystem and shell tools. Relative tool paths start at the
  selected working directory; arbitrary filesystem paths are allowed.
- `src/skills.rs`: Agent Skills discovery, catalog prompting, activation, and
  skill resource loading.
- `src/session.rs`: project-local JSON session persistence and global
  cross-project session catalog assembly.
- `src/ui.rs`: Ratatui conversation UI, composer, slash/skill/model menus, and
  file autocomplete.
- `src/server.rs`: Axum API and embedded static asset serving.
- `src/http_debug.rs`: raw HTTP debug rendering.
- `src/config.rs`: defaults plus config, environment, and CLI overrides.
- `web/`: Vue 3 and Tailwind frontend source.
- `build.rs`: builds the frontend and embeds exactly three generated assets.
- `codecrab.example.toml`: documented configuration example.
- `README.md`: user-facing behavior and usage documentation.

Sessions under `.codecrab/sessions/`, `target/`, `web/node_modules/`, and
`web/dist/` are generated state and must not be committed.

## Architecture and data flow

The main flow is:

```text
TUI / run / web API
        |
        v
 ConversationManager
        |
        +----> Session A ----> ConversationHandle ----> worker ----> Agent A
        +----> Session B ----> ConversationHandle ----> worker ----> Agent B
        +----> Session C ----> ConversationHandle ----> worker ----> Agent C
```

Each conversation worker exclusively owns its `Agent`, serializes typed
commands and persistence, emits authoritative snapshots/events, and exposes
out-of-band turn cancellation. `ConversationManager` prevents duplicate
workers for a session and keeps them alive across navigation. Presentation code
must never retain or lock an `Agent`.
`Agent` owns conversation behavior; UI code collects input and renders state,
while API code translates HTTP requests and responses. Persist model, reasoning
effort, and service tier in the active session so a selection applies to the
current draft and all later turns until changed.

## Agent Skills and project instructions

- Project skills are discovered from `.agents/skills` between the working
  directory and Git root, then user/global locations and
  `CODECRAB_SKILLS_DIR`.
- Project skills shadow later skills with the same name.
- A skill is invoked with `/skill-name`; slash autocomplete includes commands
  only at the beginning of an otherwise empty input, while skills are available
  after existing text.
- For the initial catalog, split `SKILL.md` on lines equal to `---` and send the
  first non-empty section. If there is no separator, send the entire file.
- Load the complete `SKILL.md` only after the skill is selected. Load referenced
  resources progressively.
- CodeCrab loads the complete root `AGENTS.md` once when constructing a new
  agent. Changes to this file require a new CodeCrab conversation/process to
  affect the model context.

## UI behavior to preserve

- Printable input uses the character resolved by the terminal and active
  keyboard layout. Never map physical key combinations to layout-specific
  characters; this is required for AltGr and international keyboards.
- `Shift+Enter`, `Alt+Enter`, and `Ctrl+J` insert a newline. Up/down arrows move
  between editor lines unless an interactive menu is open.
- Soft-wrap terminal composer text by terminal cell width without mutating the
  prompt. Render and vertical movement must consume the same Unicode-aware row
  layout so wide characters, the caret, preferred columns, and six-row
  scrolling remain aligned.
- Normalize decoded terminal events into editor actions rather than branching
  on the operating system or physical keyboard. Support word movement/deletion,
  logical-line start/end, and deletion to either line boundary through
  Crossterm modifiers plus traditional Readline sequences, while preserving
  printable AltGr characters.
- `/model` presents provider-returned model, reasoning, and speed choices as a
  hierarchy rather than a flat Cartesian-product list.
- Session management must retain parity: `/sessions` in the terminal and the
  web sidebar must both expose every registered project and its persisted
  sessions through their client-appropriate navigation. Resuming restores
  messages, activity, model, reasoning, and service tier, and switches the
  agent root, tools, skills, `AGENTS.md`, and relative completion paths to the
  selected project. In the web client, deleting the active session selects the
  next saved session in that project, or leaves the project with no active
  session until the user creates one.
- `session_directories` in the platform-global config is CodeCrab's persistent
  project registry. Register a project whenever a session is saved and remove
  it when its last session is deleted. CLI, TUI, API, and web session lists
  must all consume the shared catalog from `src/session.rs`.
- Web session navigation is URL-addressable. `/sessions/<id>` must resolve the
  session across every registered project, switch the complete agent root, and
  restore that session on a clean reload without `localStorage` or prior
  browser state. Selecting or replacing an active session must push or replace
  the corresponding URL as appropriate.
- The web sidebar is a two-level navigator rather than a fully expanded tree:
  show one project's sessions under its directory name, use a back control to
  show all projects, and open a project's session list when selected. Keep the
  project back control above `New session`; when a project is empty, explain
  that the user must create a session instead of creating one implicitly.
- Fast speed is derived exclusively from the selected provider service tier.
  The terminal header may show a lightning bolt for Fast; the web client must
  not add a bolt to the model name because its speed selector is always
  visible.
- Render progress text and tool activity chronologically as one agent turn in
  both clients. A typical block is progress text, the related actions, then the
  final assistant content, all under one `CRAB`/`CodeCrab` label; never
  visually attach actions to `YOU`.
- While a turn runs, each presentation layer owns one queued follow-up rather
  than putting queue state in `Agent`. Send it automatically after the active
  turn finishes. `Steer` cancels the active turn while preserving and then
  sending that queued prompt; its TUI control is mouse-only. Both clients stop
  on two `Esc` presses within one second, and the web Send control becomes a
  square Stop control while active.
- Goals are persisted inside `Session`; keep every historical goal and
  `visible_goal_id`, but enforce at most one `active` goal. Creating or
  activating a goal pauses the previous active one. Completed and blocked goals
  become paused when their objective is edited because the old terminal state
  no longer proves the new objective.
- A new `/goal <objective>` uses the objective as the first visible user
  message. Later automatic continuations use persisted `Message { hidden:
  true }` user prompts: send them to providers to preserve valid chronology,
  but never render or expose them as user-authored transcript content. Keep
  `/goal` and `/goals` in the shared completion registry.
- Do not add a separate goal-judging model request. Inject the active objective
  into the agent system context and expose `complete_goal` and `block_goal`
  control tools. A normal final answer keeps the goal active; each client starts
  another hidden continuation only after a successful turn. Provider or tool
  errors stop automatic continuation without silently completing the goal.
- Stop and double-Escape pause the active goal before another continuation can
  start. `Steer` preserves the goal and sends the queued visible prompt.
  Editing an active goal pauses it during editing and resumes it afterward.
- Goal management must retain terminal/web parity. The terminal goal row has
  mouse-only pause/play, edit, delete, and list controls; `/goals` provides
  keyboard selection, toggle, deletion, and a scrollable description. The web
  row and modal expose the same states and mutations through the Rust goal API.
- In terminal activity rows, display file and directory paths relative to the
  active project root when they are inside it. Keep paths outside the active
  project absolute so their location remains unambiguous.
- Persist complete tool-activity details rather than truncating them in core
  state. The web client keeps activity rows compact, measures actual visual
  overflow, and shows an expand/collapse control only when detail is clipped.
- Voice dictation must remain available in both composers. Terminal capture
  uses `Ctrl+Shift+S`; web capture uses `MediaRecorder`. Transcribed text is
  inserted at the current cursor. The normal stop action never sends it;
  submission happens only through the explicit Send/Enter variants below.
- While web dictation is recording, show a live waveform from the same
  `MediaStream`. Keep the normal microphone action as stop-and-insert, while
  the active Send control stops, transcribes, inserts, and immediately sends.
- While terminal dictation is recording, replace the composer contents with a
  left/right-inset `▁▂▃▄▅▆▇█` volume history. `Ctrl+Shift+S` stops,
  transcribes, and inserts; plain `Enter` stops, transcribes, inserts, and
  immediately submits through the normal queue-aware composer path.
- Dictation uses the private ChatGPT subscription endpoint through the same
  OAuth store as Codex responses. Keep this isolated in `src/transcription.rs`,
  retain the public-contract warning in the README, and preserve a manually
  runnable ignored live test because the endpoint may change without notice.
- `@` autocomplete accepts normal platform path semantics. Bare `@` starts at
  the working directory; fragments such as `../`, absolute paths, and other
  drives are resolved generically rather than through special cases.
- File completion assumes a Nerd Font, displays an icon followed by only the
  entry name, and visually distinguishes directories.
- Slash and `@` completion candidates, filtering, ordering, icons, and accepted
  replacement text must come from `src/completion.rs` for both clients. Never
  maintain a second command/skill/file list in Vue.
- Keep the terminal UI compact. Do not restore redundant sidebars, activity
  panels, shortcut footers, empty-state slogans, session IDs, message counts,
  skill counts, or duplicated provider/auth labels. Keep the conversation
  viewport borderless and full-width, with one blank row above and below its
  content. Manual keyboard or wheel scrolling must remain in place despite new
  live agent output and must reactivate automatic following only when the user
  returns to the bottom. Preserve this behavior in both terminal and web
  conversation viewports.
- Terminal conversation text has editor-like mouse selection backed by the
  native operating-system clipboard. Left-drag may start only inside the
  conversation, copies the exact displayed content on release, omits artificial
  newlines at soft wraps, and autoscrolls while held over the header or
  composer. Right-click copies a whole user/assistant message, only the command
  portion of a shell activity, or only the displayed path of a
  read/write/edit activity, with reversed-color feedback for 500 ms. Keep the
  clipboard handle alive for Linux selection ownership and retain both X11 and
  Wayland data-control support.
- Apply terminal Markdown styling only to user-facing assistant text, including
  streamed segments. Preserve every original delimiter and byte in the rendered
  source so mouse selection and semantic copy return the exact Markdown.
  Distinguish H1 from H2-H6, and style emphasis, inline code, links, quote/list
  markers, and ordered-list markers. Use the embedded pure-Rust `syntect`
  integration for language-aware fenced code; do not add a `bat` subprocess.

## Embedded web application

- The frontend calls relative `/api` URLs so it automatically shares the
  server origin and works behind a reverse proxy.
- `/api/chat` is an NDJSON stream. Preserve incremental
  `assistant_text_delta`, `assistant_message_completed`, and `activity` events
  in their original order, followed by terminal `done`, `cancelled`, or
  `error`; do not regress either provider protocol or either client to a
  final-response-only request. `/api/chat/cancel` must signal the active turn
  without waiting on the workspace mutex.
- Both ChatGPT Responses and compatible Chat Completions consume SSE by HTTP
  chunk and emit text deltas before the response body finishes. Keep the final
  reconstructed `Message` authoritative for persistence and tool-call
  association; clients replace their temporary streamed segment with it rather
  than appending a duplicate.
- Cancellation is core behavior even though queuing is presentation behavior.
  It must drop the active provider request or awaited tool future promptly,
  retain the user message and completed progress in the session, and add
  cancellation tool responses when needed to preserve valid assistant/tool
  call pairing.
- Assistant content is rendered as sanitized Markdown. Keep parsing,
  sanitization, and syntax highlighting in `web/src/markdown.js`; never pass
  raw model HTML directly to `v-html`.
- Every web user message and every individual assistant text segment has its
  own copy action. Copy the original plain/Markdown message content, never
  rendered HTML or adjacent tool activity, and show brief success feedback.
- Web transcript rows use only the compact `U` and `C` identity badges; keep
  message content aligned directly with its badge instead of restoring
  redundant `You`, `User`, or `CodeCrab` labels. When clearing the controlled
  web textarea programmatically, wait for Vue's DOM update before measuring
  `scrollHeight` so an empty composer cannot retain the previous draft height.
- Keep active web turns fully expanded. Once a turn completes successfully,
  collapse its intermediate assistant progress and tool activity into a
  one-line persisted duration/operation summary while leaving the final
  assistant message visible; users must be able to expand and collapse that
  progress again.
- Web controls use `@lucide/vue` components. Do not use Nerd Font private-use
  glyphs for buttons or status icons because browsers do not inherit the
  terminal font; Nerd Font glyphs remain intentional only for file completion.
- Rust is the production web server. Do not introduce a required Node.js
  runtime for the final executable.
- The first `Ctrl+C` in `codecrab serve` starts graceful shutdown. If active
  requests or connections keep it pending for 100 ms, explain that reason on
  stderr. A second `Ctrl+C` must force immediate process termination so a hung
  connection can never trap the user.
- `vite build` must produce exactly `index.html`, `app.js`, and `app.css`.
  Keep filenames stable unless `build.rs` and the embedded Axum routes are
  changed together.
- `build.rs` runs `npm ci` when `web/node_modules` is absent, always builds the
  frontend, verifies the exact output set, and copies it into `OUT_DIR`.
- Keep the first implementation intentionally small. Add dependencies only
  when they materially improve the product.

## Development commands

Prerequisites are a recent Rust toolchain with Rust 2024 edition support and
Node.js 20.19+ or 22.12+.

```console
npm --prefix web ci
npm --prefix web test
npm --prefix web run build
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Useful runtime checks:

```console
cargo run -- --help
cargo run -- auth status
cargo run -- serve --port 0
```

`cargo build`, `cargo test`, and `cargo clippy` invoke `build.rs`, so they also
build the frontend. Use `npm --prefix web run build` directly when changing
only frontend code and needing faster feedback.

The live provider catalog test is ignored because it requires network access
and an interactive ChatGPT OAuth login. Do not make normal test runs depend on
real credentials, network access, or a user's saved sessions.

## Engineering conventions

- Use Rust 2024 idioms, `anyhow` for contextual application errors, and
  `serde` types at serialization boundaries.
- Keep module responsibilities narrow. Put shared behavior in the agent/core,
  not in duplicated TUI and web implementations.
- Preserve unrelated working-tree changes. Inspect the diff before editing and
  again before handing off.
- GitHub issue #6 (`https://github.com/victor141516/codecrab/issues/6`) is the
  permanent umbrella for deferred small UX improvements and must remain open.
  When someone requests or mentions a small UX change but does not want it
  implemented now, ask for confirmation before adding it as a sub-issue of #6;
  never add it there without that confirmation. Keep bugs, substantial features,
  and architectural/design work as standalone issues instead.
- Prefer focused, direct implementations. Because compatibility is not
  required, remove superseded code, stale fields, dead events, and misleading
  documentation in the same change.
- Do not invent provider behavior. Parse capabilities defensively from actual
  responses and surface catalog failures instead of replacing them with a
  hardcoded list.
- Keep user-visible documentation synchronized with CLI flags, API endpoints,
  authentication behavior, and security/execution semantics.
- `codecrab config` must clearly separate the platform-global file path from
  the effective non-secret configuration content.
- Avoid logging secrets except inside the explicitly unredacted
  `--debug-openai` path.
- Do not commit, push, publish, or create a pull request unless the user
  explicitly asks.

## Tests and review expectations

Add or update focused tests for behavior changes. In particular:

- Provider changes need fixture-style protocol/catalog parsing tests.
- Tool changes need path and filesystem behavior tests using temporary
  directories; never mutate real user files in tests.
- Skill changes need discovery, precedence, metadata, activation, and resource
  tests as applicable.
- TUI changes need state/input tests and render tests for both compact and wide
  terminals.
- Server changes need API/state tests where practical and must preserve the
  embedded-asset and NDJSON event-contract tests.
- Frontend changes must at minimum pass the production Vite build.

For review, prioritize functional regressions, state persistence, protocol
correctness, deadlocks around the shared web agent, terminal input behavior on
international layouts, accidental reintroduction of security gates, and drift
between documented and implemented behavior.

## Definition of done

A change is complete when:

1. The requested behavior works through every affected surface.
2. Superseded code and documentation are removed.
3. Relevant focused tests exist and pass.
4. `cargo fmt --check`, `cargo test`, and strict Clippy pass.
5. Frontend changes produce exactly the three expected assets.
6. The final diff contains no unrelated edits, generated output, credentials,
   or session data.
7. The handoff states what changed, what was verified, and any real remaining
   limitation.
