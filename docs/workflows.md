# Everyday workflows

## Sessions and projects

CodeCrab saves project-local JSON sessions under `.codecrab/sessions/` and maintains a global registry so both clients can browse work across projects. No project sessions are stored in CodeCrab's global data directory and do not persist the process working directory. A newly opened session remains available while active, but CodeCrab discards it on navigation or clean shutdown if it still has no conversation or other user-created session state.

```bash
codecrab sessions
codecrab resume
codecrab resume <session-id-or-prefix>
```

The terminal `/sessions` view and web sidebar both expose the complete project/session hierarchy, including the top-level **No project** group. Both search case-insensitively across every registered project, including archived sessions. Ranked title matches appear immediately in their own group; after two query characters, bounded and cancelable scanning adds a separate ranked content group with a user/assistant marker and contextual snippet. Content matching collapses whitespace and uses Unicode lowercase comparison, ranking exact messages, message prefixes, word-boundary matches, and other substrings in that order; user messages precede assistant messages at the same rank, then newer sessions win. It reads only visible user and assistant messages on each session's active conversation path, so hidden goal continuations, system context, tool responses, and inactive branches are excluded. Malformed, unreadable, and oversized session files are skipped without hiding valid results from other sessions. Selecting either kind of result resumes its exact project and session. Press `/` in the terminal session picker to search, or use the web sidebar search field; clearing the query restores the hierarchy. Child sessions are indented recursively; parents use disclosure chevrons and show the number of hidden descendants when collapsed. In the terminal, left/right collapse and expand projects or session branches, `R` renames, `P` toggles a pin, and `A` archives or restores the selected session. The web edits a title directly and keeps pin and archive buttons visible on every session row. A manual title is permanent, trimmed, non-empty, limited to 120 characters, and unaffected by later edits to the first message.

Pinned child sessions remain in their canonical tree and also appear as contextual shortcuts at the start of the project. Archiving is organizational only: it does not pause workers, goals, cron runs, or processes, change conversation recency, close the active session, or exclude it from an implicit resume. Archiving a parent hides its descendants through an inherited state without overwriting their own archive metadata. On desktop, the web sidebar can be hidden completely from its header and reopened from the workspace header; that browser-local preference survives reloads, while the compact mobile drawer remains transient. Collapsing a branch is visual only and never pauses its workers. Children whose parent is missing or belongs to another project remain roots in their own project with a compact parent-path hint. Deleting a parent does not delete or rewrite its descendants.

Deleting a session with active managed processes requires one confirmation covering all of them. Accepting stops every managed process tree before deleting; declining leaves both the processes and session untouched. The terminal asks the same question before exiting while managed processes are active.

The web composer keeps model, reasoning, and speed selection beside its attachment, dictation, and send actions. Switching sessions also switches the agent's working directory, tools, file completion, skills, and `AGENTS.md` context. A turn can keep running in one session while you open or start another; returning restores its live stream.

Web session URLs use `/sessions/<id>`, so a clean reload or shared URL resolves the correct registered project without relying on browser storage. Unsent composer drafts remain browser-local and isolated by project and session.

## File and image attachments

The web composer accepts multiple files through its paperclip button, paste, or drag and drop. Browser files are hashed before upload, checked against the selected session, and streamed to the agent host only when that SHA-256 is not already present. Uploads are limited to 25 MiB per file. Generic files remain unchanged and are exposed to the agent as stable host-side paths; CodeCrab does not automatically parse, OCR, or upload them to the model provider.

The terminal keeps ordinary local files lightweight: `@path` and pasted non-image files remain direct absolute or project-relative host paths. Images selected through `@`, copied as files, or copied as raw screenshot/browser pixels are imported into session storage. `Ctrl+V` is the reliable in-app clipboard action, with `Alt+V` as a fallback; `Command+V` works only when the terminal forwards it.

Attachment metadata is stored in the session JSON while bytes live under `.codecrab/session-data/<session-id>/attachments/<sha256>/`. Deduplication is deliberately per session, and deleting a session deletes its attachment data. Composer bindings preserve the order of text and images across drafts, queued prompts, edits, branches, persistence, and resume.

Images retain their original bytes and receive a metadata-stripped, aspect-preserving model rendition no larger than 1024 px on either edge. CodeCrab never upscales. PNG is used when alpha matters and JPEG otherwise; the agent can call `view_attachment_image` for a bounded higher-detail rendition. Structured image tool results use the ChatGPT Responses path; compatible Chat Completions backends receive an explicit unsupported-output error. Image submission is rejected without clearing the draft unless the provider catalog explicitly declares image input support for the selected model.

Because browser uploads write files on the CodeCrab host, the existing server warning is especially important: remotely exposed servers must be placed behind an authenticated gateway. Attachment contents are not logged except when the explicitly unredacted `--debug-openai` mode includes a multimodal provider request.

## Commands

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

## Scheduled agent tasks

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

## Branches

Conversation history is stored as a tree. Editing a previous visible user message creates a new sibling branch rather than overwriting the original continuation. `/branches` opens a reversible preview and lets you select any saved path; terminal and web clients use the same underlying branch semantics.

## Persistent goals

Start one with:

```text
/goal Upgrade the dependency and verify the complete test suite
```

CodeCrab stores the objective, pauses any previous active goal, and continues automatically across hidden follow-up turns. The model must explicitly mark the goal complete after verification or blocked when external state prevents progress. A normal final answer does not silently complete it.

Use `/goals` to pause, resume, edit, inspect, or delete historical goals. Normal Stop and double-`Esc` pause the active goal; **Steer** preserves it and sends additional direction.

## Agent delegation

When you explicitly request another agent, parallel work, delegation, or independent validation, CodeCrab can start persistent sessions through the `session_create` model tool. Agent-created sessions are children by default: CodeCrab persists the calling session as `parent_session_id` even when the model omits the relationship argument. `relationship: "independent"` creates a project-level root with no parent and is reserved for an explicit request for a separate, detached, non-child, or user-like session.

Each child has isolated model context, tools, skills, goals, activity, and project instructions; it does not inherit or summarize the parent's transcript. Its runtime context contains only minimal lineage metadata identifying the parent session. That lineage does not imply a sandbox, worktree, permission boundary, or ownership restriction.

Children appear immediately in the terminal session picker and web sidebar. You can open a child while it runs to see streamed messages and tool activity while the parent continues updating independently. The parent can inspect status or messages, send follow-ups, wait efficiently, or stop exactly one child turn.

Delegation is process-local and uses the same operating-system account and filesystem. There is no automatic worktree, sandbox, write lock, or conflict prevention, so parallel agents should edit disjoint areas or coordinate explicitly.

## Automatic context compaction

CodeCrab uses provider-reported context metadata to compact long conversations automatically. It keeps recent complete turns verbatim and rolls older history into a structured summary, while preserving the full canonical transcript and tool activity in session JSON. Both clients display compaction activity, and a failed or cancelled compaction never replaces the previous projection. Provider-specific output limits are sent only when the selected protocol supports them. Deterministic request-validation failures are not replayed, and if emergency compaction cannot recover from a context-window rejection, CodeCrab stops that turn instead of resending the unchanged oversized request.
