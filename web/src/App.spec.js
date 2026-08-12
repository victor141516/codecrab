import { createApp, nextTick } from "vue";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import App from "./App.vue";
import { DESKTOP_SIDEBAR_STORAGE_KEY } from "./sidebar-preference.js";

let app;
let root;
let desktopViewport;

beforeEach(() => {
  desktopViewport = true;
  localStorage.clear();
  window.history.replaceState({}, "", "/");
  vi.stubGlobal("fetch", vi.fn(() => Promise.reject(new Error("offline test"))));
  vi.stubGlobal("matchMedia", (query) => ({
    matches: desktopViewport && query === "(min-width: 1024px)",
    media: query,
    addEventListener() {},
    removeEventListener() {}
  }));
});

afterEach(() => {
  app?.unmount();
  root?.remove();
  app = undefined;
  root = undefined;
  vi.unstubAllGlobals();
});

function mountApp() {
  root = document.createElement("div");
  document.body.append(root);
  app = createApp(App);
  app.mount(root);
  return root;
}

function get(selector) {
  const element = root.querySelector(selector);
  expect(element).not.toBeNull();
  return element;
}

describe("desktop project sidebar", () => {
  test("defaults open, then closes and reopens from the rendered controls", async () => {
    mountApp();
    const sidebar = get("aside.app-sidebar");
    const workspace = get("main.workspace-shell");

    expect(sidebar.classList).toContain("lg:translate-x-0");
    expect(workspace.classList).toContain("lg:pl-72");

    get('button[aria-label="Hide projects and sessions"]').click();
    await nextTick();
    expect(sidebar.classList).toContain("lg:-translate-x-full");
    expect(workspace.classList).not.toContain("lg:pl-72");
    expect(localStorage.getItem(DESKTOP_SIDEBAR_STORAGE_KEY)).toBe("true");

    const showButtons = root.querySelectorAll(
      'button[aria-label="Show projects and sessions"]'
    );
    showButtons.item(showButtons.length - 1).click();
    await nextTick();
    expect(sidebar.classList).toContain("lg:translate-x-0");
    expect(workspace.classList).toContain("lg:pl-72");
    expect(localStorage.getItem(DESKTOP_SIDEBAR_STORAGE_KEY)).toBe("false");
  });

  test("restores the saved desktop preference without persisting mobile drawer state", async () => {
    localStorage.setItem(DESKTOP_SIDEBAR_STORAGE_KEY, "true");
    desktopViewport = false;
    mountApp();
    const sidebar = get("aside.app-sidebar");

    expect(sidebar.classList).toContain("lg:-translate-x-full");
    expect(get("main.workspace-shell").classList).not.toContain("lg:pl-72");

    const mobileButton = [...root.querySelectorAll(
      'button[aria-label="Show projects and sessions"]'
    )].find((button) => button.classList.contains("lg:hidden"));
    mobileButton.click();
    await nextTick();

    expect(root.querySelector(".fixed.inset-0.z-30.lg\\:hidden")).not.toBeNull();
    expect(sidebar.classList).toContain("translate-x-0");
    expect(sidebar.classList).toContain("lg:-translate-x-full");
    expect(localStorage.getItem(DESKTOP_SIDEBAR_STORAGE_KEY)).toBe("true");

    desktopViewport = true;
    window.dispatchEvent(new Event("resize"));
    await nextTick();
    expect(sidebar.classList).toContain("lg:-translate-x-full");
    expect(get("main.workspace-shell").classList).not.toContain("lg:pl-72");
  });
});

describe("session sidebar actions", () => {
  function response(body) {
    return { ok: true, status: 200, json: async () => body };
  }

  function sidebarState(activeSessionId = "regular") {
    const pinned = {
      id: "pinned",
      title: "Pinned session",
      depth: 0,
      descendant_count: 0,
      ancestor_titles: [],
      pinned_at: "2026-08-12T12:00:00Z",
      active_terminal_count: 0
    };
    const regular = {
      id: "regular",
      title: "Regular session",
      depth: 0,
      descendant_count: 0,
      ancestor_titles: [],
      pinned_at: null,
      active_terminal_count: 0
    };
    const active = activeSessionId === "pinned" ? pinned : regular;
    return {
      live_revision: 1,
      project: "/workspace",
      filesystem_root: "/",
      session: {
        ...active,
        provider: "openai",
        model: "gpt-test",
        messages: [],
        activities: [],
        turns: [],
        goals: [],
        branch_nodes: [],
        active_message_ids: []
      },
      projects: [{
        root: "/workspace",
        sessions: [pinned, regular],
        pinned_sessions: [pinned],
        active_sessions: [pinned, regular],
        archived_sessions: []
      }],
      skills: [],
      models: [],
      providers: [],
      workers: [],
      usage: { available: false },
      cron: null
    };
  }

  test("separates pinned rows and reserves rename for a double click", async () => {
    const resumeBodies = [];
    vi.stubGlobal("fetch", vi.fn(async (input, options = {}) => {
      const url = String(input);
      if (url === "/api/state") return response(sidebarState());
      if (url === "/api/sessions/resume") {
        const body = JSON.parse(options.body);
        resumeBodies.push(body);
        return response(sidebarState(body.id));
      }
      throw new Error("offline test");
    }));

    mountApp();
    await vi.waitFor(() =>
      expect(root.textContent).toContain("Regular session")
    );

    const titles = [...root.querySelectorAll('[title="Double-click to rename"]')];
    expect(titles.map((item) => item.textContent.trim())).toEqual([
      "Pinned session",
      "Regular session"
    ]);
    expect(root.textContent).toContain("Pinned");
    expect(root.textContent).toContain("Sessions");

    titles[1].click();
    await nextTick();
    expect(root.querySelector('[aria-label="Session title"]')).toBeNull();
    await new Promise((resolve) => window.setTimeout(resolve, 250));
    await vi.waitFor(() =>
      expect(resumeBodies).toEqual([{ project: "/workspace", id: "regular" }])
    );

    const pinnedTitle = [...root.querySelectorAll('[title="Double-click to rename"]')]
      .find((item) => item.textContent.trim() === "Pinned session");
    pinnedTitle.click();
    pinnedTitle.click();
    pinnedTitle.dispatchEvent(
      new MouseEvent("dblclick", { bubbles: true, cancelable: true })
    );
    await nextTick();
    expect(get('[aria-label="Session title"]').value).toBe("Pinned session");
    await new Promise((resolve) => window.setTimeout(resolve, 250));
    expect(resumeBodies).toHaveLength(1);
  });
});

describe("No project sessions", () => {
  test("renders the global group and creates a session without a project path", async () => {
    const neutralState = {
      live_revision: 1,
      project: null,
      filesystem_root: "C:\\",
      session: null,
      projects: [{ root: null, sessions: [] }],
      skills: [],
      models: [],
      providers: [],
      workers: [],
      usage: { available: false },
      cron: null
    };
    let creationBody;
    vi.stubGlobal("fetch", vi.fn(async (input, options = {}) => {
      const url = String(input);
      if (url === "/api/state") {
        return { ok: true, status: 200, json: async () => neutralState };
      }
      if (url === "/api/sessions") {
        creationBody = JSON.parse(options.body);
        return {
          ok: true,
          status: 200,
          json: async () => ({
            ...neutralState,
            live_revision: 2,
            session: {
              id: "global-session",
              title: "New conversation",
              provider: "openai",
              model: "gpt-test",
              messages: [],
              activities: [],
              turns: [],
              goals: [],
              branch_nodes: [],
              active_message_ids: []
            }
          })
        };
      }
      throw new Error("offline test");
    }));

    mountApp();
    await vi.waitFor(() =>
      expect(root.textContent).toContain("Global sessions")
    );
    get('button[aria-label="New session in No project"]').click();
    await vi.waitFor(() => expect(creationBody).toEqual({ no_project: true }));
  });
});

describe("OpenAI usage", () => {
  const usage = {
    available: true,
    stale: false,
    can_reset: true,
    last_updated_at: 1786826000,
    snapshot: {
      plan_type: "plus",
      windows: [{
        limit_id: "codex",
        limit_name: null,
        kind: "primary",
        used_percent: 37,
        remaining_percent: 63,
        window_duration_seconds: 604800,
        resets_at: 1786826526
      }],
      reset_credits: {
        available_count: 1,
        applicable_available_count: 1,
        credits: []
      }
    }
  };

  function response(body) {
    return { ok: true, status: 200, json: async () => body };
  }

  function workspaceState(currentUsage = usage) {
    return {
      live_revision: 1,
      project: "/workspace",
      session: {
        id: "session-1",
        title: "Usage test",
        provider: "openai",
        model: "gpt-test",
        reasoning_effort: null,
        service_tier: null,
        messages: [],
        activities: [],
        turns: [],
        goals: [],
        branch_nodes: [],
        active_message_ids: []
      },
      projects: [{ root: "/workspace", sessions: [] }],
      skills: [],
      models: [],
      providers: [],
      workers: [],
      usage: currentUsage,
      cron: null
    };
  }

  test("opens, confirms, and sends an idempotent account reset", async () => {
    const resetUsage = {
      ...usage,
      can_reset: false,
      snapshot: {
        ...usage.snapshot,
        reset_credits: { ...usage.snapshot.reset_credits, available_count: 0 }
      }
    };
    let resetBody;
    let usageRequests = 0;
    let refreshedUsage = usage;
    vi.stubGlobal("fetch", vi.fn(async (input, options = {}) => {
      const url = String(input);
      if (url === "/api/state") return response(workspaceState());
      if (url.startsWith("/api/usage?")) {
        usageRequests += 1;
        return response(refreshedUsage);
      }
      if (url === "/api/usage/reset") {
        resetBody = JSON.parse(options.body);
        return response({ outcome: "reset", windows_reset: 1, usage: resetUsage });
      }
      throw new Error("offline test");
    }));

    mountApp();
    await vi.waitFor(() => expect(root.querySelector('[data-testid="usage-indicator"]')).not.toBeNull());
    get('[data-testid="usage-indicator"]').click();
    await nextTick();
    expect(get('[data-testid="usage-modal"]').textContent).toContain("63% remaining");

    await vi.waitFor(() =>
      expect(get('[data-testid="usage-reset"]').disabled).toBe(false)
    );
    const requestsBeforeRetry = usageRequests;
    get('[data-testid="usage-refresh"]').click();
    await vi.waitFor(() => expect(usageRequests).toBeGreaterThan(requestsBeforeRetry));
    await vi.waitFor(() =>
      expect(get('[data-testid="usage-reset"]').disabled).toBe(false)
    );
    get('[data-testid="usage-reset"]').click();
    await nextTick();
    expect(get('[data-testid="usage-modal"]').textContent).toContain("Use one manual reset credit?");
    refreshedUsage = resetUsage;
    get('[data-testid="usage-refresh"]').click();
    await vi.waitFor(() =>
      expect(get('[data-testid="usage-reset-confirm"]').disabled).toBe(true)
    );
    await vi.waitFor(() =>
      expect(get('[data-testid="usage-refresh"]').disabled).toBe(false)
    );
    expect(get('[data-testid="usage-modal"]').textContent).toContain("confirmation is paused");
    refreshedUsage = usage;
    get('[data-testid="usage-refresh"]').click();
    await vi.waitFor(() =>
      expect(get('[data-testid="usage-reset-confirm"]').disabled).toBe(false)
    );
    get('[data-testid="usage-reset-confirm"]').click();
    await vi.waitFor(() => expect(resetBody).toBeDefined());
    expect(resetBody.session_id).toBe("session-1");
    expect(resetBody.idempotency_key).toEqual(expect.any(String));
    expect(resetBody).not.toHaveProperty("credit_id");
    await vi.waitFor(() =>
      expect(get('[data-testid="usage-modal"]').textContent).toContain("Usage reset completed")
    );
  });
});

describe("session provider selection", () => {
  function response(body) {
    return { ok: true, status: 200, json: async () => body };
  }

  function workspaceState(provider = "openai", model = "gpt-test") {
    return {
      live_revision: 1,
      project: "/workspace",
      filesystem_root: "/",
      session: {
        id: "session-1",
        title: "Provider test",
        provider,
        model,
        reasoning_effort: null,
        service_tier: null,
        messages: [],
        activities: [],
        turns: [],
        goals: [],
        branch_nodes: [],
        active_message_ids: []
      },
      projects: [{ root: "/workspace", sessions: [] }],
      skills: [],
      models: [{
        slug: model,
        display_name: model,
        supported_reasoning_levels: [],
        service_tiers: []
      }],
      providers: [
        { name: "openai", active: true },
        { name: "local", active: false }
      ],
      workers: [],
      usage: { available: false },
      cron: null
    };
  }

  test("switches the current session and handles plural slash commands locally", async () => {
    let providerBody;
    let chatRequested = false;
    vi.stubGlobal("fetch", vi.fn(async (input, options = {}) => {
      const url = String(input);
      if (url === "/api/state") return response(workspaceState());
      if (url === "/api/provider") {
        providerBody = JSON.parse(options.body);
        return response(workspaceState("local", "local-model"));
      }
      if (url === "/api/completions") return response(null);
      if (url === "/api/chat") chatRequested = true;
      throw new Error("offline test");
    }));

    mountApp();
    await vi.waitFor(() =>
      expect(root.querySelector('select[aria-label="Provider"]')).not.toBeNull()
    );
    const provider = get('select[aria-label="Provider"]');
    provider.value = "local";
    provider.dispatchEvent(new Event("change", { bubbles: true }));
    await vi.waitFor(() =>
      expect(providerBody).toEqual({ session_id: "session-1", provider: "local" })
    );
    await vi.waitFor(() =>
      expect(get('select[aria-label="Model"]').value).toBe("local-model")
    );

    const composer = get('[role="textbox"][aria-label="Message CodeCrab"]');
    composer.textContent = "/providers";
    composer.dispatchEvent(new InputEvent("input", { bubbles: true, data: "s" }));
    await nextTick();
    composer.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true })
    );
    await vi.waitFor(() => expect(document.activeElement).toBe(provider));
    expect(chatRequested).toBe(false);

    composer.textContent = "/models";
    composer.dispatchEvent(new InputEvent("input", { bubbles: true, data: "s" }));
    await nextTick();
    composer.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true })
    );
    await vi.waitFor(() =>
      expect(document.activeElement).toBe(get('select[aria-label="Model"]'))
    );
    expect(chatRequested).toBe(false);
  });

  test("renders a single configured provider as static text", async () => {
    const onlyProvider = workspaceState();
    onlyProvider.providers = [onlyProvider.providers[0]];
    vi.stubGlobal("fetch", vi.fn(async (input) => {
      if (String(input) === "/api/state") return response(onlyProvider);
      throw new Error("offline test");
    }));

    mountApp();
    await vi.waitFor(() =>
      expect(root.querySelector('span[title="Provider"]')?.textContent.trim()).toBe("openai")
    );
    expect(root.querySelector('select[aria-label="Provider"]')).toBeNull();
  });

  test("renders server-classified composer pills without changing the draft", async () => {
    vi.stubGlobal("fetch", vi.fn(async (input, options = {}) => {
      const url = String(input);
      if (url === "/api/state") return response(workspaceState());
      if (url === "/api/completions") {
        const request = JSON.parse(options.body);
        return response({
          request_id: request.request_id,
          items: [],
          replace_before: "",
          replace_after: "",
          recursive: false,
          slash_context: false,
          segments: [
            { text: "Use ", kind: null },
            { text: "/review-rust", kind: "skill" },
            { text: " on ", kind: null },
            { text: "@src/main.rs", kind: "file" },
            { text: " then ", kind: null },
            { text: "/missing", kind: "invalid" }
          ]
        });
      }
      throw new Error("offline test");
    }));

    mountApp();
    await vi.waitFor(() => expect(root.textContent).toContain("Provider test"));
    const composer = get('[role="textbox"][aria-label="Message CodeCrab"]');
    const draft = "Use /review-rust on @src/main.rs then /missing";
    composer.textContent = draft;
    composer.dispatchEvent(
      new InputEvent("input", { bubbles: true, data: "/missing" })
    );

    await vi.waitFor(() =>
      expect(composer.querySelectorAll("[data-composer-token]").length).toBe(3)
    );
    expect(
      [...composer.querySelectorAll("[data-composer-token]")].map((token) => [
        token.textContent,
        token.dataset.composerToken
      ])
    ).toEqual([
      ["/review-rust", "skill"],
      ["@src/main.rs", "file"],
      ["/missing", "invalid"]
    ]);
    expect(composer.textContent).toBe(draft);
  });

  test("accepts autocomplete at a contenteditable caret and restores that caret", async () => {
    vi.stubGlobal("fetch", vi.fn(async (input, options = {}) => {
      const url = String(input);
      if (url === "/api/state") return response(workspaceState());
      if (url === "/api/completions") {
        const request = JSON.parse(options.body);
        const completing = request.before_cursor === "/rev";
        return response({
          request_id: request.request_id,
          items: completing
            ? [{
                id: "skill:review-rust",
                name: "review-rust",
                display: "review-rust",
                description: "Review Rust changes.",
                icon: null,
                kind: "skill",
                replacement: "/review-rust "
              }]
            : [],
          replace_before: completing ? "/rev" : "",
          replace_after: "",
          recursive: false,
          slash_context: completing,
          segments: completing
            ? [{ text: "/rev", kind: "invalid" }]
            : [{ text: request.before_cursor, kind: "skill" }]
        });
      }
      throw new Error("offline test");
    }));

    mountApp();
    await vi.waitFor(() => expect(root.textContent).toContain("Provider test"));
    const composer = get('[role="textbox"][aria-label="Message CodeCrab"]');
    composer.textContent = "/rev";
    composer.focus();
    const range = document.createRange();
    range.selectNodeContents(composer);
    range.collapse(false);
    document.getSelection().removeAllRanges();
    document.getSelection().addRange(range);
    composer.dispatchEvent(new InputEvent("input", { bubbles: true, data: "v" }));
    await vi.waitFor(() => expect(root.querySelector("#completion-0")).not.toBeNull());

    composer.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true })
    );

    await vi.waitFor(() => expect(composer.textContent).toBe("/review-rust "));
    expect(document.activeElement).toBe(composer);
    expect(document.getSelection().anchorOffset).toBeGreaterThanOrEqual(0);
  });
});

describe("managed terminal processes", () => {
  function response(body) {
    return { ok: true, status: 200, json: async () => body };
  }

  function workspaceState() {
    return {
      live_revision: 1,
      project: "/workspace",
      filesystem_root: "/",
      session: {
        id: "session-1",
        title: "Process test",
        provider: "openai",
        model: "gpt-test",
        reasoning_effort: null,
        service_tier: null,
        messages: [],
        activities: [],
        turns: [],
        goals: [],
        branch_nodes: [],
        active_message_ids: []
      },
      projects: [{
        root: "/workspace",
        sessions: [{
          id: "session-1",
          title: "Process test",
          model: "gpt-test",
          active_terminal_count: 1
        }]
      }],
      skills: [],
      models: [],
      providers: [],
      workers: [],
      usage: { available: false },
      cron: null
    };
  }

  test("opens from /processes and keeps output visible after stopping", async () => {
    const createdAt = new Date(Date.now() - 3_000).toISOString();
    const process = {
      terminal_id: "terminal_1",
      command: "long-running-command",
      created_at: createdAt,
      origin_activity_id: "call-shell"
    };
    let stopped = false;
    let stopBody;
    let chatRequested = false;
    vi.stubGlobal("fetch", vi.fn(async (input, options = {}) => {
      const url = String(input);
      if (url === "/api/state") return response(workspaceState());
      if (url === "/api/completions") return response(null);
      if (url.startsWith("/api/processes/terminal_1")) {
        return response({
          ...process,
          process_state: stopped ? "closed" : "running",
          completed_at: stopped ? new Date().toISOString() : null,
          exit_code: null,
          screen_sequence: 2,
          rows: 24,
          columns: 80,
          lines: [{
            spans: [{
              text: "colored output",
              style: {
                foreground: "#00ff00",
                background: "#000000",
                bold: true,
                faint: false,
                italic: false,
                underline: "none",
                reverse: false,
                strikethrough: false
              }
            }]
          }]
        });
      }
      if (url.startsWith("/api/processes?")) {
        return response(stopped ? [] : [process]);
      }
      if (url === "/api/processes/stop") {
        stopBody = JSON.parse(options.body);
        stopped = true;
        return response({
          ...process,
          process_state: "closed",
          completed_at: new Date().toISOString(),
          exit_code: null,
          screen_sequence: 3,
          rows: 24,
          columns: 80,
          lines: []
        });
      }
      if (url === "/api/chat") chatRequested = true;
      throw new Error(`offline test: ${url}`);
    }));

    mountApp();
    await vi.waitFor(() =>
      expect(root.querySelector('button[aria-label^="Open 1 active process"]')).not.toBeNull()
    );
    const composer = get('[role="textbox"][aria-label="Message CodeCrab"]');
    composer.textContent = "/processes";
    composer.dispatchEvent(new Event("input", { bubbles: true }));
    await nextTick();
    get('button[aria-label="Send message"]').click();

    await vi.waitFor(() =>
      expect(get('[data-testid="processes-modal"]').textContent).toContain("long-running-command")
    );
    expect(chatRequested).toBe(false);
    get('button[aria-label="View output"]').click();
    await vi.waitFor(() =>
      expect(get('[data-testid="processes-modal"]').textContent).toContain("colored output")
    );
    expect(get('[data-testid="processes-modal"] span[style]').style.color).toBe("#00ff00");

    [...get('[data-testid="processes-modal"]').querySelectorAll("button")]
      .find((button) => button.textContent.includes("Stop process"))
      .click();
    await nextTick();
    const confirmationButtons = [...get('[data-testid="processes-modal"]').querySelectorAll("button")]
      .filter((button) => button.textContent.includes("Stop process"));
    confirmationButtons.at(-1).click();

    await vi.waitFor(() =>
      expect(stopBody).toEqual({
        session_id: "session-1",
        terminal_id: "terminal_1"
      })
    );
    await vi.waitFor(() =>
      expect(get('[data-testid="processes-modal"]').textContent).toContain("Stopped")
    );
  });
});
