import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("./App.vue", import.meta.url), "utf8");

test("sidebar renders every project with nested sessions and exact project actions", () => {
  assert.match(appSource, /v-for="project in projects"/);
  assert.match(appSource, /v-if="projectExpanded\(project\.root\)"/);
  assert.match(appSource, /v-for="item in projectSessionRows\(project\)"/);
  assert.match(appSource, /newSession\(project\.root\)/);
  assert.match(appSource, /resumeSession\(project\.root, item\.id\)/);
  assert.doesNotMatch(appSource, /formatTime\(item\.(created_at|updated_at)\)/);
  assert.match(appSource, /current-session-dot/);
  assert.match(appSource, /sidebar-session-list/);
  assert.match(appSource, /:data-session-depth="item\.depth"/);
  assert.match(appSource, /item\.depth \* 0\.75/);
  assert.match(appSource, /item\.descendant_count > 0/);
  assert.match(appSource, /@click\.stop="toggleSession\(item\.id\)"/);
  assert.match(appSource, /item\.ancestor_titles\.slice\(-2\)/);
  assert.match(appSource, /togglePinned\(project\.root, item\)/);
  assert.match(appSource, /toggleArchived\(project\.root, item\)/);
  assert.match(appSource, /beginSessionRename\(item\)/);
  assert.match(appSource, /project\.pinned_sessions/);
  assert.match(appSource, /project\.archived_sessions/);
});

test("model controls live inside the composer instead of the header", () => {
  const composerIndex = appSource.indexOf('<div class="composer-shell">');
  const modelControlIndex = appSource.indexOf(
    'class="control composer-control composer-model-control"'
  );
  assert.ok(composerIndex >= 0);
  assert.ok(modelControlIndex > composerIndex);
});

test("web does not expose conversation clearing", () => {
  assert.doesNotMatch(appSource, /Clear conversation/);
  assert.doesNotMatch(appSource, /clearSession/);
  assert.doesNotMatch(appSource, /\/api\/session\/clear/);
});

test("server project picker remains usable in compact and wide viewports", () => {
  assert.match(appSource, /Open project on server/);
  assert.match(appSource, /max-h-\[90dvh\]/);
  assert.match(appSource, /p-3 backdrop-blur-sm sm:p-6/);
  assert.match(appSource, /flex-col-reverse gap-2 sm:flex-row/);
  assert.doesNotMatch(appSource, /Show projects on server/);
});
