import assert from "node:assert/strict";
import test from "node:test";
import {
  insertTranscriptAtSelection,
  isDictationShortcut
} from "./dictation.js";

function keyEvent(overrides = {}) {
  return {
    key: "s",
    ctrlKey: true,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    ...overrides
  };
}

test("dictation shortcut follows the client platform", () => {
  assert.equal(
    isDictationShortcut(keyEvent({ shiftKey: true }), "Windows"),
    true
  );
  assert.equal(isDictationShortcut(keyEvent(), "Linux x86_64"), false);
  assert.equal(isDictationShortcut(keyEvent(), "MacIntel"), true);
  assert.equal(
    isDictationShortcut(keyEvent({ shiftKey: true }), "MacIntel"),
    true
  );
  assert.equal(
    isDictationShortcut(keyEvent({ altKey: true, shiftKey: true }), "Windows"),
    false
  );
  assert.equal(
    isDictationShortcut(keyEvent({ metaKey: true }), "MacIntel"),
    false
  );
});

test("focused transcript replaces the current selection", () => {
  assert.deepEqual(
    insertTranscriptAtSelection("Review old text now", "the project", {
      focused: true,
      start: 7,
      end: 15
    }),
    {
      inserted: true,
      value: "Review the project now",
      cursor: "Review the project".length
    }
  );
});

test("focused transcript is inserted at the current cursor", () => {
  assert.deepEqual(
    insertTranscriptAtSelection("Reviewthis", "the project", {
      focused: true,
      start: 6,
      end: 6
    }),
    {
      inserted: true,
      value: "Review the project this",
      cursor: "Review the project ".length
    }
  );
});

test("unfocused transcript ignores a stale selection and appends", () => {
  assert.deepEqual(
    insertTranscriptAtSelection("Keep this draft", "and append", {
      focused: false,
      start: 4,
      end: 8
    }),
    {
      inserted: true,
      value: "Keep this draft and append",
      cursor: "Keep this draft and append".length
    }
  );
});

test("empty transcript leaves the draft unchanged", () => {
  assert.deepEqual(insertTranscriptAtSelection("draft", "  "), {
    inserted: false,
    value: "draft",
    cursor: 5
  });
});
