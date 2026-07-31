import assert from "node:assert/strict";
import test from "node:test";
import {
  acceptsRevision,
  consumeNdjson,
  localStreamEndState,
  liveTurnState,
  mergeCatalog
} from "./session-live.js";

test("session NDJSON parsing handles split chunks and a final partial line", async () => {
  const encoder = new TextEncoder();
  const chunks = [
    '{"type":"sync","revision":4,"sessions":[',
    ']}\n{"type":"event","revision":5,"session_id":"child"}'
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

  await consumeNdjson(response, (message) => messages.push(message));

  assert.deepEqual(
    messages.map((message) => message.type),
    ["sync", "event"]
  );
});

test("catalog revisions prevent a late parent state from hiding a new child", () => {
  const current = {
    live_revision: 8,
    projects: [
      {
        root: "project",
        sessions: [{ id: "parent" }, { id: "child" }]
      }
    ],
    workers: [{ id: "child", lifecycle: "running" }]
  };
  const stale = mergeCatalog(
    current,
    {
      revision: 7,
      projects: [{ root: "project", sessions: [{ id: "parent" }] }],
      workers: []
    },
    8
  );

  assert.equal(stale.applied, false);
  assert.equal(stale.state.projects[0].sessions.length, 2);
  assert.equal(stale.state.workers[0].lifecycle, "running");

  const fresh = mergeCatalog(
    current,
    {
      revision: 9,
      projects: current.projects,
      workers: [{ id: "child", lifecycle: "idle" }]
    },
    8
  );
  assert.equal(fresh.applied, true);
  assert.equal(fresh.revision, 9);
  assert.equal(fresh.state.workers[0].lifecycle, "idle");
});

test("equal session revisions are accepted for paired catalog and view events", () => {
  assert.equal(acceptsRevision(12, 12), true);
  assert.equal(acceptsRevision(12, 11), false);
});

test("delegated lifecycle drives the same running and Stop state as local turns", () => {
  assert.deepEqual(liveTurnState("running"), {
    sending: true,
    cancelling: false
  });
  assert.deepEqual(liveTurnState("stopping"), {
    sending: true,
    cancelling: true
  });
  assert.deepEqual(liveTurnState("idle"), {
    sending: false,
    cancelling: false
  });
});

test("a broken local stream keeps following a worker that is still running", () => {
  assert.deepEqual(localStreamEndState(false, "running"), {
    sending: true,
    cancelling: false,
    keepFollowing: true
  });
  assert.deepEqual(localStreamEndState(false, "stopping"), {
    sending: true,
    cancelling: true,
    keepFollowing: true
  });
  assert.deepEqual(localStreamEndState(false, "failed"), {
    sending: false,
    cancelling: false,
    keepFollowing: false
  });
  assert.deepEqual(localStreamEndState(true, "running"), {
    sending: false,
    cancelling: false,
    keepFollowing: false
  });
});
