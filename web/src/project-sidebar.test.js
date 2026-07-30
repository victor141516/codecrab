import assert from "node:assert/strict";
import test from "node:test";

import {
  expandKnownProject,
  newSessionPayload,
  toggleProjectExpansion
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
});

test("new sessions always target the chosen project exactly", () => {
  assert.deepEqual(newSessionPayload("C:\\work\\other"), {
    project: "C:\\work\\other"
  });
});
