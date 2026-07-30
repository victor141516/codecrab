import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("./App.vue", import.meta.url), "utf8");

test("sidebar renders every project with nested sessions and exact project actions", () => {
  assert.match(appSource, /v-for="project in projects"/);
  assert.match(appSource, /v-if="projectExpanded\(project\.root\)"/);
  assert.match(appSource, /v-for="item in project\.sessions"/);
  assert.match(appSource, /newSession\(project\.root\)/);
  assert.match(appSource, /resumeSession\(project\.root, item\.id\)/);
});

test("server project picker remains usable in compact and wide viewports", () => {
  assert.match(appSource, /Open project on server/);
  assert.match(appSource, /max-h-\[90dvh\]/);
  assert.match(appSource, /p-3 backdrop-blur-sm sm:p-6/);
  assert.match(appSource, /flex-col-reverse gap-2 sm:flex-row/);
  assert.doesNotMatch(appSource, /Show projects/);
});
