# Configuration

Inspect the platform-global file path and effective non-secret configuration:

```bash
codecrab config
```

The complete format is documented in [`codecrab.example.toml`](../codecrab.example.toml). It covers provider profiles, manual model catalogs and allowlists, reasoning and service-tier options, context limits, request inactivity timeout, project registry, and deterministic shell selection.

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

## Secret storage

ChatGPT OAuth tokens and provider API keys are stored as plain text in the global `config.toml`, protected only by your operating-system account permissions. They are not written into project sessions, and normal CLI, TUI, and web configuration views redact them.
