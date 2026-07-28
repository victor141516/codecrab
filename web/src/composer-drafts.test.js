import assert from "node:assert/strict";
import test from "node:test";
import {
  composerDraftKey,
  createComposerDraftController,
  createComposerDraftStore
} from "./composer-drafts.js";

function memoryStorage() {
  const values = new Map();
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: (key) => values.delete(key),
    values
  };
}

function controllerHarness(storage = memoryStorage()) {
  let draft = "";
  let resized = 0;
  let focused = 0;
  const store = createComposerDraftStore(storage);
  const controller = createComposerDraftController({
    store,
    getDraft: () => draft,
    setDraft: (value) => {
      draft = value;
    },
    afterUpdate: async () => {},
    resize: () => {
      resized += 1;
    },
    focus: () => {
      focused += 1;
    }
  });
  return {
    controller,
    get draft() {
      return draft;
    },
    set draft(value) {
      draft = value;
      controller.persist(value);
    },
    get resized() {
      return resized;
    },
    get focused() {
      return focused;
    },
    store
  };
}

test("draft keys are versioned and isolate normalized project/session identities", () => {
  assert.equal(
    composerDraftKey("C:\\work\\codecrab\\", "session A"),
    composerDraftKey("c:/work/codecrab", "session A")
  );
  assert.notEqual(
    composerDraftKey("c:/work/codecrab", "session A"),
    composerDraftKey("c:/work/other", "session A")
  );
  assert.notEqual(
    composerDraftKey("c:/work/codecrab", "session A"),
    composerDraftKey("c:/work/codecrab", "session B")
  );
});

test("navigation saves, clears, restores, resizes, and isolates exact drafts", async () => {
  const harness = controllerHarness();
  await harness.controller.activate("/project/a", "a");
  harness.draft = "  first\nsession  ";

  const source = await harness.controller.beginNavigation();
  assert.equal(harness.draft, "");
  await harness.controller.activate("/project/b", "b");
  assert.equal(harness.draft, "");
  harness.draft = "second";

  await harness.controller.beginNavigation();
  await harness.controller.activate("/project/a", "a");
  assert.equal(harness.draft, "  first\nsession  ");
  assert.ok(harness.resized >= 4);

  await harness.controller.rollbackNavigation(source);
  assert.equal(harness.draft, "  first\nsession  ");
});

test("new-session activation focuses only when requested", async () => {
  const harness = controllerHarness();
  await harness.controller.activate("/project", "existing");
  assert.equal(harness.focused, 0);
  await harness.controller.activate("/project", "new", {
    focusComposer: true
  });
  assert.equal(harness.focused, 1);
});

test("successful send removes only the sent draft and deletion forgets drafts", async () => {
  const harness = controllerHarness();
  await harness.controller.activate("/project", "session");
  harness.draft = "sent text";
  const sent = harness.controller.snapshot();
  await harness.controller.clear({ keepStored: true });
  await harness.controller.finishSend(sent, true);
  assert.equal(harness.store.read("/project", "session"), "");

  harness.draft = "repeated text";
  const repeated = harness.controller.snapshot();
  await harness.controller.clear({ keepStored: true });
  harness.draft = "repeated text";
  await harness.controller.finishSend(repeated, true);
  assert.equal(harness.store.read("/project", "session"), "repeated text");

  harness.controller.forget("/project", "session");
  assert.equal(harness.store.read("/project", "session"), "");
});

test("failed navigation and failed send recover the source draft", async () => {
  const harness = controllerHarness();
  await harness.controller.activate("/project", "session");
  harness.draft = "recover me";
  const source = await harness.controller.beginNavigation();
  await harness.controller.rollbackNavigation(source);
  assert.equal(harness.draft, "recover me");

  const sent = harness.controller.snapshot();
  await harness.controller.clear({ keepStored: true });
  await harness.controller.finishSend(sent, false);
  assert.equal(harness.draft, "recover me");
});

test("throwing storage falls back to isolated in-memory drafts", () => {
  const unavailable = {
    getItem() {
      throw new Error("blocked");
    },
    setItem() {
      throw new Error("blocked");
    },
    removeItem() {
      throw new Error("blocked");
    }
  };
  const store = createComposerDraftStore(unavailable);
  store.write("/project", "a", "alpha");
  store.write("/project", "b", "beta");

  assert.equal(store.read("/project", "a"), "alpha");
  assert.equal(store.read("/project", "b"), "beta");
  store.remove("/project", "a");
  assert.equal(store.read("/project", "a"), "");
});
