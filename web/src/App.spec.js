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
