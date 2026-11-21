# HTTP API

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
| `GET` | `/api/sessions/search?query=...&request_id=...` | Stream immediate title matches and progressive content matches as NDJSON |
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
