# Quick start

## 1. Install

Download a binary for Windows, macOS, or Linux from the [latest GitHub Release](https://github.com/victor141516/codecrab/releases/latest), rename it to `codecrab` (`codecrab.exe` on Windows), and put it on your `PATH`. The Windows executable embeds the CodeCrab mascot as its application icon.

On macOS or Linux, make the downloaded file executable:

```bash
chmod +x codecrab
```

Or build from source:

```bash
git clone https://github.com/victor141516/codecrab.git
cd codecrab
cargo install --path .
```

A source build requires a recent Rust toolchain and Node.js 20.19+ or 22.12+. Linux also needs ALSA development headers and `pkg-config` (for example, `libasound2-dev` and `pkg-config` on Debian/Ubuntu).

## 2. Sign in

Use the OpenAI account that owns your ChatGPT Plus or Pro subscription:

```bash
codecrab auth login
```

A browser opens for the OAuth flow. An OpenAI API key or another OpenAI-compatible provider can be configured instead.

## 3. Open a project

```bash
cd path/to/your/project
codecrab
```

Then describe the outcome you want—for example, `Find and fix the failing tests`. CodeCrab will inspect the project, make changes, run checks, and summarize the result.

To open another directory without changing your shell's working directory:

```bash
codecrab -C path/to/project
```
