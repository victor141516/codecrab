import test from "node:test";
import assert from "node:assert/strict";
import {
  insertAttachmentReference,
  reconcileAttachmentBindings,
  trimAttachmentDraft
} from "./attachment-bindings.js";

test("inserts an attachment at the cursor and keeps an explicit binding", () => {
  const result = insertAttachmentReference("hello goodbye", 6, 6, {
    reference: "@preview.png",
    attachment: { id: "attachment" }
  });
  assert.equal(result.text, "hello @preview.png goodbye");
  assert.deepEqual(result.binding, {
    attachment_id: "attachment",
    reference: "@preview.png",
    start: 6,
    end: 18
  });
});

test("editing inside a reference removes its binding while outside edits shift it", () => {
  const binding = {
    attachment_id: "a",
    reference: "@image.png",
    start: 6,
    end: 16
  };
  assert.deepEqual(
    reconcileAttachmentBindings("hello @image.png", "well hello @image.png", [binding]),
    [{ ...binding, start: 11, end: 21 }]
  );
  assert.deepEqual(
    reconcileAttachmentBindings("hello @image.png", "hello @Ximage.png", [binding]),
    []
  );
});

test("trimming a prompt preserves byte-compatible attachment offsets", () => {
  assert.deepEqual(
    trimAttachmentDraft("  hi @x  ", [
      { attachment_id: "a", reference: "@x", start: 5, end: 7 }
    ]),
    {
      prompt: "hi @x",
      attachments: [{ attachment_id: "a", start: 3, end: 5 }]
    }
  );
});
