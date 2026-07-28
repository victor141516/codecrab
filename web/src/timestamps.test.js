import test from "node:test";
import assert from "node:assert/strict";
import {
  activityEventTimestamp,
  formatEventTimestamp
} from "./timestamps.js";

test("formats backend timestamps using the browser locale", () => {
  const value = "2026-07-28T11:31:03.219Z";
  const expected = new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "long"
  }).format(new Date(value));

  assert.equal(formatEventTimestamp(value), expected);
  assert.equal(formatEventTimestamp(undefined), undefined);
  assert.equal(formatEventTimestamp("invalid"), undefined);
});

test("uses activity completion time for terminal statuses", () => {
  const activity = {
    status: "completed",
    started_at: "2026-07-28T11:30:00Z",
    completed_at: "2026-07-28T11:31:00Z"
  };

  assert.equal(activityEventTimestamp(activity), activity.completed_at);
  assert.equal(
    activityEventTimestamp({ ...activity, status: "failed" }),
    activity.completed_at
  );
  assert.equal(
    activityEventTimestamp({ ...activity, status: "running" }),
    activity.started_at
  );
});
