import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  clampEditorWidth,
  editorFollowStorageKey,
  nextUnseenLiveChanges
} from "./editor-panel.js";

test("clamps the editor while preserving room for chat", () => {
  assert.equal(clampEditorWidth(900, 1200), 835);
  assert.equal(clampEditorWidth(620, 823), 458);
  assert.equal(clampEditorWidth(100, 1200), 360);
});

test("isolates follow preferences by project", () => {
  assert.notEqual(editorFollowStorageKey("/a"), editorFollowStorageKey("/b"));
});

test("returns only unseen live changes in order", () => {
  const seen = new Set(["one:change-1"]);
  const changes = nextUnseenLiveChanges(
    [
      { id: "one", change: "change-1" },
      { id: "two", change: "change-2" },
      { id: "three", change: "change-3" }
    ],
    seen
  );
  assert.deepEqual(changes, ["change-2", "change-3"]);
  assert.equal(seen.size, 3);
});

test("the panel stays lazy and exposes independent panel and follow controls", () => {
  const app = readFileSync(new URL("./App.vue", import.meta.url), "utf8");
  assert.match(app, /const editorOpen = ref\(false\)/);
  assert.match(app, /const editorActivated = ref\(false\)/);
  assert.match(app, /Show code panel/);
  assert.match(app, /Follow file changes/);
  assert.match(app, /v-if="editorActivated"/);
  assert.match(app, /v-show="editorOpen"/);
  assert.match(app, /@pointerdown="startEditorResize"/);
  assert.match(app, /@load="bindEditorInteraction"/);
});
