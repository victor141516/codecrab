import assert from "node:assert/strict";
import test from "node:test";

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
