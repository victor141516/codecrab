import assert from "node:assert/strict";
import test from "node:test";
import { Window } from "happy-dom";

import {
  composerSelectionOffsets,
  insertComposerText,
  readComposerText,
  renderComposerSegments,
  setComposerSelectionOffsets
} from "./composer-editor.js";

function harness() {
  const window = new Window();
  const document = window.document;
  const editor = document.createElement("div");
  editor.contentEditable = "true";
  document.body.append(editor);
  return { window, document, editor };
}

test("decorated composer segments preserve the exact plain-text draft", () => {
  const { editor } = harness();
  const draft = "Use /review-rust on @src/main.rs\nthen /missing";

  renderComposerSegments(editor, [
    { text: "Use ", kind: null },
    { text: "/review-rust", kind: "skill" },
    { text: " on ", kind: null },
    { text: "@src/main.rs", kind: "file" },
    { text: "\nthen ", kind: null },
    { text: "/missing", kind: "invalid" }
  ]);

  assert.equal(readComposerText(editor), draft);
  assert.equal(editor.textContent, draft);
  assert.deepEqual(
    [...editor.querySelectorAll("[data-composer-token]")].map((token) => [
      token.textContent,
      token.dataset.composerToken
    ]),
    [
      ["/review-rust", "skill"],
      ["@src/main.rs", "file"],
      ["/missing", "invalid"]
    ]
  );
});

test("composer rendering treats draft text as text instead of markup", () => {
  const { editor } = harness();
  const unsafe = '<img src=x onerror="alert(1)"> /help';

  renderComposerSegments(editor, [
    { text: '<img src=x onerror="alert(1)"> ', kind: null },
    { text: "/help", kind: "command" }
  ]);

  assert.equal(readComposerText(editor), unsafe);
  assert.equal(editor.querySelector("img"), null);
  assert.equal(editor.querySelector("[data-composer-token]").textContent, "/help");
});

test("selection offsets span plain and decorated nodes in both directions", () => {
  const { window, editor } = harness();
  renderComposerSegments(editor, [
    { text: "before ", kind: null },
    { text: "/skill", kind: "skill" },
    { text: " after", kind: null }
  ]);

  setComposerSelectionOffsets(editor, 3, 15, window.getSelection());
  assert.deepEqual(composerSelectionOffsets(editor, window.getSelection()), {
    start: 3,
    end: 15
  });

  setComposerSelectionOffsets(editor, 15, 3, window.getSelection());
  assert.deepEqual(composerSelectionOffsets(editor, window.getSelection()), {
    start: 3,
    end: 15
  });
});

test("collapsed selections use JavaScript UTF-16 offsets like the old textarea", () => {
  const { window, editor } = harness();
  const draft = "🦀 /skill café";
  renderComposerSegments(editor, [
    { text: "🦀 ", kind: null },
    { text: "/skill", kind: "skill" },
    { text: " café", kind: null }
  ]);
  const cursor = "🦀 /ski".length;

  setComposerSelectionOffsets(editor, cursor, cursor, window.getSelection());

  assert.deepEqual(composerSelectionOffsets(editor, window.getSelection()), {
    start: cursor,
    end: cursor
  });
});

test("selection offsets clamp safely when a restored draft becomes shorter", () => {
  const { window, editor } = harness();
  renderComposerSegments(editor, [{ text: "short", kind: null }]);

  setComposerSelectionOffsets(editor, 50, 80, window.getSelection());

  assert.deepEqual(composerSelectionOffsets(editor, window.getSelection()), {
    start: 5,
    end: 5
  });
});

test("insertion replaces the current selection and returns the next caret", () => {
  assert.deepEqual(insertComposerText("hello brave world", 6, 11, "new"), {
    value: "hello new world",
    cursor: 9
  });
});

test("line breaks remain literal newlines through decoration and selection", () => {
  const { window, editor } = harness();
  const draft = "first\n/help\nthird";
  renderComposerSegments(editor, [
    { text: "first\n", kind: null },
    { text: "/help", kind: "command" },
    { text: "\nthird", kind: null }
  ]);

  setComposerSelectionOffsets(editor, 6, 11, window.getSelection());

  assert.equal(readComposerText(editor), draft);
  assert.deepEqual(composerSelectionOffsets(editor, window.getSelection()), {
    start: 6,
    end: 11
  });
});

test("empty drafts retain a usable collapsed selection", () => {
  const { window, editor } = harness();
  renderComposerSegments(editor, []);

  setComposerSelectionOffsets(editor, 0, 0, window.getSelection());

  assert.equal(readComposerText(editor), "");
  assert.deepEqual(composerSelectionOffsets(editor, window.getSelection()), {
    start: 0,
    end: 0
  });
});
