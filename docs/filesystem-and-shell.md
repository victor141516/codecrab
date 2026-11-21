# Filesystem, shell, and terminal access

Relative tool paths start at the selected working directory, but parent paths, absolute paths, other drives, and symbolic links are valid. In No project sessions, structured file tools and `@` completion require absolute paths; bare `@` begins at the platform filesystem root. Shells still start in the directory where the CodeCrab process was launched (or the effective `-C` directory). CodeCrab deliberately has no sandbox or project boundary.

The agent can:

- list, read, and recursively search files;
- create files and overwrite UTF-8 text;
- perform exact, unique text replacements;
- run non-interactive commands with separate stdout and stderr;
- launch persistent interactive PTYs and send text, paste, key, mouse, and resize actions;
- observe, list, and close managed terminals.

A PTY command still running after five seconds returns a conversation-scoped terminal ID so the agent can continue interacting with it. Live terminals survive session navigation within one process; after a process restart they are marked interrupted because a PTY cannot be reattached.
