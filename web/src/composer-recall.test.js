import assert from "node:assert/strict";
import test from "node:test";

import {
  latestVisibleUserMessage,
  recalledMessageForComposer
} from "./composer-recall.js";

const session = {
  messages: [
    { role: "user", content: "older" },
    { role: "assistant", content: "answer" },
    { role: "user", content: "latest visible" },
    { role: "assistant", content: "latest answer" },
    { role: "user", content: "hidden continuation", hidden: true }
  ],
  active_message_ids: [
    "older",
    "answer",
    "latest",
    "latest-answer",
    "hidden"
  ]
};

test("recall selects the latest visible user message and skips hidden prompts", () => {
  assert.deepEqual(latestVisibleUserMessage(session), {
    nodeId: "latest",
    content: "latest visible"
  });
});

test("recall only starts from an empty composer and never walks farther back", () => {
  assert.deepEqual(recalledMessageForComposer(session, "", null), {
    nodeId: "latest",
    content: "latest visible"
  });
  assert.equal(
    recalledMessageForComposer(session, "latest visible", "latest"),
    null
  );
  assert.equal(recalledMessageForComposer(session, "new draft", null), null);
});

test("recall requires an active visible message node", () => {
  assert.equal(
    latestVisibleUserMessage({
      messages: [{ role: "user", content: "hidden", hidden: true }],
      active_message_ids: ["hidden"]
    }),
    null
  );
  assert.equal(
    latestVisibleUserMessage({
      messages: [{ role: "user", content: "orphan" }],
      active_message_ids: []
    }),
    null
  );
});
