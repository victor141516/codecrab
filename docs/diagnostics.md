# Diagnostics

Interactive TUI errors are written to a lazily created execution log so stderr cannot corrupt the terminal. Select a fixed location with:

```bash
codecrab --error-log ./codecrab-errors.log
```

Non-TUI modes report diagnostics to stderr.

For protocol troubleshooting, print complete OpenAI/OAuth requests and responses to stderr or a file:

```bash
codecrab --debug-openai run "Say hello"
codecrab --debug-openai=./openai-debug.log serve
```

> [!CAUTION]
> `--debug-openai` is intentionally **unredacted**. Its output can contain API keys, access and refresh tokens, authorization codes, prompts, repository contents, tool arguments, and model responses. Never share it without reviewing the complete log.

The `=` before a debug path is required. Explicit debug files are appended to lazily, and a file error is surfaced rather than silently falling back to stderr.
