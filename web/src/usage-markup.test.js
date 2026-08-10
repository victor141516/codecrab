import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("./App.vue", import.meta.url), "utf8");

test("OpenAI usage has a persistent indicator and detailed modal", () => {
  assert.match(appSource, /v-if="usage\.available"/);
  assert.match(appSource, /\{\{ usageLabel \}\}/);
  assert.match(appSource, />OpenAI usage</);
  assert.match(appSource, /v-for="window in usage\.snapshot\.windows"/);
  assert.match(appSource, /\{\{ window\.remaining_percent \}\}% remaining/);
  assert.match(appSource, /Resets \{\{ formatUsageReset/);
  assert.match(appSource, /if \(prompt === "\/usage" && usage\.value\.available\)/);
});

test("manual reset requires confirmation and is blocked for stale usage", () => {
  assert.match(appSource, />Use one manual reset credit\?</);
  assert.match(appSource, /This immediately resets every eligible usage window/);
  assert.match(appSource, /:disabled="usageResetting \|\| !usageCanReset"/);
  assert.match(appSource, /Reset confirmation is paused/);
  assert.match(appSource, /idempotency_key: usageResetKey/);
  assert.doesNotMatch(appSource, /credit_id: creditId/);
});
