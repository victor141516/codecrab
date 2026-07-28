import assert from "node:assert/strict";
import test from "node:test";

import {
  branchEdgePath,
  layoutBranchGraph,
  projectEditedSession,
  routeContainsEdge
} from "./branches.js";

test("branch layout preserves insertion order and produces an acyclic tree", () => {
  const graph = layoutBranchGraph([
    { id: "root", parent_id: null },
    { id: "original", parent_id: "root" },
    { id: "newer", parent_id: "root" },
    { id: "deep", parent_id: "newer" }
  ]);

  assert.deepEqual(
    graph.nodes.map(({ id, depth, row }) => ({ id, depth, row })),
    [
      { id: "root", depth: 0, row: 0 },
      { id: "original", depth: 1, row: 1 },
      { id: "newer", depth: 1, row: 2 },
      { id: "deep", depth: 2, row: 3 }
    ]
  );
  assert.deepEqual(
    graph.edges.map((edge) => [edge.parent.id, edge.child.id]),
    [
      ["root", "original"],
      ["root", "newer"],
      ["newer", "deep"]
    ]
  );
  assert.match(branchEdgePath(graph.edges[0]), /^M .+ C .+$/);
});

test("route styling only includes edges fully contained in the route", () => {
  const graph = layoutBranchGraph([
    { id: "root", parent_id: null },
    { id: "original", parent_id: "root" },
    { id: "newer", parent_id: "root" }
  ]);
  const route = new Set(["root", "newer"]);

  assert.equal(routeContainsEdge(route, graph.edges[0]), false);
  assert.equal(routeContainsEdge(route, graph.edges[1]), true);
});

test("editing projects only the prefix and temporary edited message", () => {
  const session = {
    title: "Original",
    messages: [
      { role: "user", content: "root" },
      { role: "assistant", content: "answer" },
      { role: "user", content: "old" },
      { role: "assistant", content: "old continuation" }
    ],
    active_message_ids: ["root", "answer", "old", "continuation"],
    activities: [
      { id: "before", turn_message_index: 0 },
      { id: "removed", turn_message_index: 2 }
    ],
    turns: [
      { message_index: 0 },
      { message_index: 2 }
    ]
  };

  const projected = projectEditedSession(
    session,
    "old",
    "edited",
    "2026-07-28T20:00:00Z"
  );

  assert.deepEqual(
    projected.messages.map((message) => message.content),
    ["root", "answer", "edited"]
  );
  assert.deepEqual(projected.active_message_ids, [
    "root",
    "answer",
    "editing-old"
  ]);
  assert.deepEqual(projected.activities.map((activity) => activity.id), [
    "before"
  ]);
  assert.equal(projected.turns.length, 1);
  assert.equal(session.messages.length, 4);
});
