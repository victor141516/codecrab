import assert from "node:assert/strict";
import test from "node:test";

import { sortChronologically } from "./timeline.js";

test("tool-only activities stay before a later assistant message", () => {
  const events = [
    { type: "message", message: { content: "First", sequence: 0 } },
    { type: "message", message: { content: "Second", sequence: 3 } },
    { type: "activity", activity: { id: "call-1", sequence: 1 } },
    { type: "activity", activity: { id: "call-2", sequence: 2 } }
  ];

  assert.deepEqual(
    sortChronologically(events).map((event) =>
      event.type === "message" ? event.message.content : event.activity.id
    ),
    ["First", "call-1", "call-2", "Second"]
  );
});

test("legacy events without sequences keep their reconstructed order", () => {
  const events = [
    { type: "message", message: { content: "First" } },
    { type: "activity", activity: { id: "call-1" } }
  ];

  assert.equal(sortChronologically(events), events);
});
