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
