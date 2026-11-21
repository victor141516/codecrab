# Agent Skills and project instructions

CodeCrab implements the `SKILL.md` Agent Skills format. A project skill can look like this:

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
description: Review Rust changes for correctness and safety.
---

Inspect the affected code and tests before reporting actionable findings.
```

List available skills:

```bash
codecrab skills
```

Invoke one in any prompt:

```text
Use /review-rust to review my changes
```

Discovery precedence is:

1. `.agents/skills` from the selected directory up to its Git root.
2. `~/.agents/skills`.
3. `$CODEX_HOME/skills`, or `~/.codex/skills`.
4. Extra path-list entries from `CODECRAB_SKILLS_DIR`.

Project skills shadow later skills with the same name. CodeCrab initially exposes compact skill metadata, loads the full `SKILL.md` only when selected, and reads referenced resources progressively. Opening `/` completion lazily rediscovers skills in both clients, so added, edited, deleted, or newly invalid skills take effect in the active conversation without restarting it.

For general personal guidance, create `~/.config/codecrab/AGENTS.md`. For project guidance, CodeCrab loads `AGENTS.md` first from the Git repository root and then from the selected working directory, skipping the second copy when both directories are the same. CodeCrab snapshots these instructions when constructing an agent, so changes apply to new conversations, cold resumes, or project switches rather than an already-running agent.
