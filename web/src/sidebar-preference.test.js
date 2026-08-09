import assert from "node:assert/strict";
import test from "node:test";

import {
  loadDesktopSidebarCollapsed,
  saveDesktopSidebarCollapsed
} from "./sidebar-preference.js";

function memoryStorage(initialValue = null) {
  let value = initialValue;
  return {
    getItem() {
      return value;
    },
    setItem(_key, nextValue) {
      value = nextValue;
    }
  };
}

test("desktop sidebar defaults open and restores the saved collapsed preference", () => {
  const storage = memoryStorage();

  assert.equal(loadDesktopSidebarCollapsed(storage), false);
  saveDesktopSidebarCollapsed(true, storage);
  assert.equal(loadDesktopSidebarCollapsed(storage), true);
  saveDesktopSidebarCollapsed(false, storage);
  assert.equal(loadDesktopSidebarCollapsed(storage), false);
});

test("sidebar storage failures never block rendering or toggling", () => {
  const storage = {
    getItem() {
      throw new Error("unavailable");
    },
    setItem() {
      throw new Error("unavailable");
    }
  };

  assert.equal(loadDesktopSidebarCollapsed(storage), false);
  assert.doesNotThrow(() => saveDesktopSidebarCollapsed(true, storage));
});
