import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { runInNewContext } from "node:vm";

import { registerServiceWorker } from "./pwa.js";

test("registers the service worker after the page loads", async () => {
  let load;
  const registrations = [];
  const browser = {
    navigator: {
      serviceWorker: {
        register: async (path) => registrations.push(path)
      }
    },
    addEventListener: (event, listener) => {
      assert.equal(event, "load");
      load = listener;
    }
  };

  registerServiceWorker(browser);
  assert.deepEqual(registrations, []);
  load();
  await Promise.resolve();
  assert.deepEqual(registrations, ["/service-worker.js"]);
});

test("does nothing when service workers are unavailable", () => {
  let listenerAdded = false;
  registerServiceWorker({
    navigator: {},
    addEventListener: () => {
      listenerAdded = true;
    }
  });
  assert.equal(listenerAdded, false);
});

test("service worker never handles API or managed editor requests", () => {
  const listeners = new Map();
  const worker = {
    location: { origin: "http://codecrab.test" },
    clients: { claim() {} },
    skipWaiting() {},
    addEventListener(event, listener) {
      listeners.set(event, listener);
    }
  };
  const source = readFileSync(
    new URL("../pwa/service-worker.js", import.meta.url),
    "utf8"
  );
  runInNewContext(source, {
    self: worker,
    caches: {},
    URL,
    Response,
    fetch: async () => ({ ok: false })
  });
  const fetchListener = listeners.get("fetch");
  const isHandled = (pathname) => {
    let handled = false;
    fetchListener({
      request: {
        method: "GET",
        mode: "same-origin",
        url: `http://codecrab.test${pathname}`
      },
      respondWith() {
        handled = true;
      }
    });
    return handled;
  };

  assert.equal(isHandled("/api/state"), false);
  assert.equal(isHandled("/code-server/instance/stable/app.js"), false);
  assert.equal(isHandled("/sessions/example"), true);
});
