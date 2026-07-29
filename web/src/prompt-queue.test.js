import test from "node:test";
import assert from "node:assert/strict";
import {
  createPromptQueue,
  enqueuePrompt,
  removeQueuedPrompt,
  steerQueuedPrompt,
  takeNextQueuedPrompt,
  updateQueuedPrompt
} from "./prompt-queue.js";

test("queued prompts are dispatched in insertion order", () => {
  const queue = createPromptQueue();
  enqueuePrompt(queue, "first");
  enqueuePrompt(queue, "second");
  enqueuePrompt(queue, "third");

  assert.equal(takeNextQueuedPrompt(queue).content, "first");
  assert.equal(takeNextQueuedPrompt(queue).content, "second");
  assert.equal(takeNextQueuedPrompt(queue).content, "third");
});

test("editing a queued prompt preserves its identity and position", () => {
  const queue = createPromptQueue();
  const first = enqueuePrompt(queue, "first");
  const second = enqueuePrompt(queue, "second");
  const third = enqueuePrompt(queue, "third");

  assert.equal(updateQueuedPrompt(queue, second, "edited second"), true);

  assert.deepEqual(
    queue.items.map((item) => [item.id, item.content]),
    [
      [first, "first"],
      [second, "edited second"],
      [third, "third"]
    ]
  );
});

test("deleting and steering target exact queued prompts", () => {
  const queue = createPromptQueue();
  const first = enqueuePrompt(queue, "first");
  const second = enqueuePrompt(queue, "second");
  const third = enqueuePrompt(queue, "third");

  removeQueuedPrompt(queue, second);
  assert.deepEqual(queue.items.map((item) => item.id), [first, third]);

  assert.equal(steerQueuedPrompt(queue, third), true);
  assert.equal(takeNextQueuedPrompt(queue).id, third);
  assert.deepEqual(queue.items.map((item) => item.id), [first]);
});

test("an item being edited blocks only when it is next", () => {
  const queue = createPromptQueue();
  const first = enqueuePrompt(queue, "first");
  const second = enqueuePrompt(queue, "second");
  queue.editingId = second;

  assert.equal(takeNextQueuedPrompt(queue).id, first);
  assert.equal(takeNextQueuedPrompt(queue), null);
  assert.deepEqual(queue.items.map((item) => item.id), [second]);
});
