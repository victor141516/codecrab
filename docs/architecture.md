# Architecture

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
