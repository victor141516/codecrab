import assert from "node:assert/strict";
import test from "node:test";

import {
  expandKnownProject,
  newSessionPayload,
  toggleProjectExpansion,
  toggleSessionExpansion,
  visibleSessionRows
} from "./project-sidebar.js";

test("projects expand and collapse independently", () => {
  let expanded = new Set(["/first", "/second"]);
  expanded = toggleProjectExpansion(expanded, "/first");

  assert.deepEqual([...expanded], ["/second"]);
  assert.deepEqual([...toggleProjectExpansion(expanded, "/third")], [
    "/second",
    "/third"
  ]);
});

test("server state prunes removed projects and expands the active one", () => {
  const expanded = expandKnownProject(
    new Set(["/removed", "/kept"]),
    [{ root: "/kept" }, { root: "/active" }],
    "/active"
  );

  assert.deepEqual([...expanded], ["/kept", "/active"]);

  const collapsedGlobal = expandKnownProject(
    new Set(),
    [{ root: null }],
    undefined
  );
  assert.deepEqual([...collapsedGlobal], []);
});

test("new sessions always target the chosen project exactly", () => {
  assert.deepEqual(newSessionPayload("C:\\work\\other"), {
    project: "C:\\work\\other"
  });
  assert.deepEqual(newSessionPayload(null), { no_project: true });
});

test("session expansion is isolated and hides every recursive descendant", () => {
  const sessions = [
    { id: "root", depth: 0, descendant_count: 2 },
    { id: "child", depth: 1, descendant_count: 1 },
    { id: "grandchild", depth: 2, descendant_count: 0 },
    { id: "sibling", depth: 0, descendant_count: 0 }
  ];
  const collapsed = toggleSessionExpansion(new Set(), "root");

  assert.deepEqual(
    visibleSessionRows(sessions, collapsed).map((session) => session.id),
    ["root", "sibling"]
  );
  assert.deepEqual([...toggleSessionExpansion(collapsed, "root")], []);
});

test("collapsing one session does not affect projects or sibling branches", () => {
  const projectState = new Set(["project"]);
  const sessionState = toggleSessionExpansion(new Set(["first"]), "second");

  assert.deepEqual([...projectState], ["project"]);
  assert.deepEqual([...sessionState], ["first", "second"]);
});

test("a live child insertion respects the existing parent collapse state", () => {
  const collapsed = new Set(["parent"]);
  const updated = [
    { id: "parent", depth: 0, descendant_count: 1 },
    { id: "new-child", depth: 1, descendant_count: 0 },
    { id: "root", depth: 0, descendant_count: 0 }
  ];

  assert.deepEqual(
    visibleSessionRows(updated, collapsed).map((session) => session.id),
    ["parent", "root"]
  );
  assert.equal(updated[0].descendant_count, 1);
});
