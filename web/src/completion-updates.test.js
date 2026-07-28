import assert from "node:assert/strict";
import test from "node:test";
import {
  consumeCompletionNdjson,
  mergeCompletionUpdate
} from "./completion-updates.js";

test("stale recursive completion updates are rejected by request identity", () => {
  const current = {
    request_id: 4,
    items: [{ id: "current", name: "current" }]
  };
  const merged = mergeCompletionUpdate(
    current,
    0,
    { request_id: 3, items: [{ id: "stale", name: "stale" }] },
    4
  );

  assert.equal(merged.applied, false);
  assert.deepEqual(merged.menu, current);
});

test("progressive completion updates preserve the selected stable item", () => {
  const current = {
    request_id: 7,
    replace_before: "@config",
    replace_after: "",
    items: [
      { id: "local-a", name: "a" },
      { id: "local-b", name: "b" }
    ]
  };
  const merged = mergeCompletionUpdate(
    current,
    1,
    {
      request_id: 7,
      items: [
        { id: "recursive", name: "nested/config" },
        { id: "local-a", name: "a" },
        { id: "local-b", name: "b" }
      ]
    },
    7
  );

  assert.equal(merged.applied, true);
  assert.equal(merged.menu.items[merged.selectedIndex].id, "local-b");
  assert.equal(merged.menu.replace_before, "@config");
});

test("NDJSON completion parsing handles split chunks and a final partial line", async () => {
  const encoder = new TextEncoder();
  const chunks = [
    '{"type":"update","request_',
    'id":9,"items":[]}\n{"type":"done","request_id":9}'
  ];
  const response = {
    body: new ReadableStream({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
        controller.close();
      }
    })
  };
  const messages = [];

  await consumeCompletionNdjson(response, (message) => messages.push(message));

  assert.deepEqual(
    messages.map((message) => message.type),
    ["update", "done"]
  );
});
