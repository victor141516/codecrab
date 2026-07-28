import assert from "node:assert/strict";
import test from "node:test";
import {
  VirtualTimelineModel,
  VirtualTimelineStore,
  createVirtualTimelineFixture
} from "./virtual-timeline.js";

test("a thousand-row timeline returns only the visible range plus overscan", () => {
  const model = new VirtualTimelineModel();
  model.setItems(createVirtualTimelineFixture(1_000, 100));

  const range = model.range(45_000, 600, 300);

  assert.ok(range.end - range.start <= 14);
  assert.equal(range.total, 100_000);
  assert.equal(range.top + range.bottom + (range.end - range.start) * 100, range.total);
});

test("dynamic height correction preserves a stable key and intra-row anchor", () => {
  const model = new VirtualTimelineModel();
  model.setItems(createVirtualTimelineFixture(4, 100));
  const anchor = model.anchorAt(150);

  model.measure("fixture-0", 220);

  assert.deepEqual(anchor, { key: "fixture-1", offset: 50 });
  assert.equal(model.scrollTopForAnchor(anchor), 270);
  assert.equal(model.totalHeight(), 520);
});

test("revealing an unmounted row scrolls only when it is outside the viewport", () => {
  const model = new VirtualTimelineModel();
  model.setItems(createVirtualTimelineFixture(10, 100));

  assert.equal(model.scrollTopToRevealIndex(3, 200, 300), 200);
  assert.equal(model.scrollTopToRevealIndex(1, 200, 300), 100);
  assert.equal(model.scrollTopToRevealIndex(6, 200, 300), 400);
  assert.equal(model.scrollTopToRevealIndex(9, 200, 300), 700);
});

test("overscan clamps naturally at the beginning and end", () => {
  const model = new VirtualTimelineModel();
  model.setItems(createVirtualTimelineFixture(20, 50));

  const start = model.range(0, 100, 100);
  const end = model.range(950, 100, 100);

  assert.equal(start.start, 0);
  assert.equal(start.top, 0);
  assert.equal(end.end, 20);
  assert.equal(end.bottom, 0);
});

test("list append preserves measured heights without rebuilding on content-only updates", () => {
  const model = new VirtualTimelineModel();
  const first = createVirtualTimelineFixture(2, 90);
  model.setItems(first);
  model.measure("fixture-1", 180);

  const sameKeysChanged = model.setItems([
    first[0],
    { ...first[1], signature: "streamed", mounted: true }
  ]);
  model.setItems([
    first[0],
    { ...first[1], signature: "streamed", mounted: false },
    { key: "fixture-2", estimate: 70, signature: "new", mounted: false }
  ]);

  assert.equal(sameKeysChanged, false);
  assert.equal(model.totalHeight(), 340);
});

test("unmounted content mutations replace stale measurements with a fresh estimate", () => {
  const model = new VirtualTimelineModel();
  model.setItems([
    { key: "row", estimate: 100, signature: "short", mounted: false }
  ]);
  model.measure("row", 240);

  model.setItems([
    { key: "row", estimate: 160, signature: "long", mounted: false }
  ]);

  assert.equal(model.totalHeight(), 160);
});

test("session models, anchors, and follow state remain isolated", () => {
  const store = new VirtualTimelineStore();
  const descriptor = [
    { key: "same-key", estimate: 100, signature: "a", mounted: false }
  ];
  const first = store.activate("session-a", descriptor);
  first.measure("same-key", 220);
  store.saveView("session-a", {
    anchor: { key: "same-key", offset: 40 },
    followBottom: false
  });
  const second = store.activate("session-b", descriptor);
  second.measure("same-key", 80);

  assert.equal(store.models.get("session-a").totalHeight(), 220);
  assert.equal(store.models.get("session-b").totalHeight(), 80);
  assert.equal(store.view("session-a").followBottom, false);
  assert.equal(store.view("session-b"), null);
});
