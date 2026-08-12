import { createApp, nextTick, ref } from "vue";
import { afterEach, describe, expect, test, vi } from "vitest";

import ComposerEditor from "./ComposerEditor.vue";

let app;
let root;

afterEach(() => {
  app?.unmount();
  root?.remove();
  app = undefined;
  root = undefined;
});

function mountEditor(options = {}) {
  const draft = ref(options.draft ?? "");
  const segments = ref(options.segments ?? []);
  const disabled = ref(options.disabled ?? false);
  const inputEvents = [];
  const keydown = options.keydown ?? (() => {});
  const paste = options.paste ?? (() => {});
  const drop = options.drop ?? (() => {});
  const component = {
    components: { ComposerEditor },
    setup() {
      return { draft, segments, disabled, inputEvents, keydown, paste, drop };
    },
    template: `
      <ComposerEditor
        ref="composer"
        v-model="draft"
        :segments="segments"
        :disabled="disabled"
        placeholder="Message CodeCrab…"
        aria-label="Message CodeCrab"
        @input="inputEvents.push($event)"
        @keydown="keydown"
        @paste="paste"
        @drop="drop"
      />
    `
  };
  root = document.createElement("div");
  document.body.append(root);
  app = createApp(component);
  const instance = app.mount(root);
  return {
    draft,
    segments,
    disabled,
    inputEvents,
    instance,
    editor: root.querySelector('[role="textbox"]')
  };
}

describe("contenteditable composer", () => {
  test("exposes the same plain-text v-model contract as the textarea", async () => {
    const harness = mountEditor({ draft: "first\nsecond" });
    expect(harness.editor.textContent).toBe("first\nsecond");

    harness.editor.textContent = "changed\ndraft";
    harness.editor.dispatchEvent(
      new InputEvent("input", { bubbles: true, data: "t", inputType: "insertText" })
    );
    await nextTick();

    expect(harness.draft.value).toBe("changed\ndraft");
    expect(harness.inputEvents.at(-1)).toMatchObject({
      data: "t",
      inputType: "insertText"
    });

    harness.draft.value = "restored";
    await nextTick();
    expect(harness.editor.textContent).toBe("restored");
  });

  test("renders semantic pills while preserving text, selection, and focus", async () => {
    const harness = mountEditor({
      draft: "Use /review-rust and @src/main.rs",
      segments: [{ text: "Use /review-rust and @src/main.rs", kind: null }]
    });
    harness.instance.$refs.composer.focus();
    harness.instance.$refs.composer.setSelectionRange(8, 8);

    harness.segments.value = [
      { text: "Use ", kind: null },
      { text: "/review-rust", kind: "skill" },
      { text: " and ", kind: null },
      { text: "@src/main.rs", kind: "file" }
    ];
    await nextTick();

    expect(document.activeElement).toBe(harness.editor);
    expect(harness.instance.$refs.composer.getSelectionRange()).toEqual({
      start: 8,
      end: 8
    });
    expect(
      [...harness.editor.querySelectorAll("[data-composer-token]")].map(
        (token) => token.dataset.composerToken
      )
    ).toEqual(["skill", "file"]);
    expect(harness.editor.textContent).toBe(harness.draft.value);
  });

  test("inserts literal newlines for Shift+Enter and Alt+Enter", async () => {
    const harness = mountEditor({ draft: "ab" });
    harness.instance.$refs.composer.focus();
    harness.instance.$refs.composer.setSelectionRange(1, 1);

    harness.editor.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true })
    );
    await nextTick();
    expect(harness.draft.value).toBe("a\nb");

    harness.instance.$refs.composer.setSelectionRange(2, 2);
    harness.editor.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", altKey: true, bubbles: true })
    );
    await nextTick();
    expect(harness.draft.value).toBe("a\n\nb");
  });

  test("lets the parent consume Enter, autocomplete keys, and Escape", async () => {
    const keys = [];
    const harness = mountEditor({
      draft: "send",
      keydown(event) {
        keys.push(event.key);
        event.preventDefault();
      }
    });

    for (const key of ["Enter", "Tab", "ArrowUp", "ArrowDown", "PageUp", "PageDown", "Escape"]) {
      harness.editor.dispatchEvent(
        new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true })
      );
    }
    await nextTick();

    expect(keys).toEqual(["Enter", "Tab", "ArrowUp", "ArrowDown", "PageUp", "PageDown", "Escape"]);
    expect(harness.draft.value).toBe("send");
  });

  test("pastes plain text only at the current selection", async () => {
    const harness = mountEditor({ draft: "hello world" });
    harness.instance.$refs.composer.focus();
    harness.instance.$refs.composer.setSelectionRange(6, 11);
    const event = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "clipboardData", {
      value: {
        files: [],
        getData(type) {
          return type === "text/plain" ? "<b>safe</b>" : "<b>unsafe</b>";
        }
      }
    });

    harness.editor.dispatchEvent(event);
    await nextTick();

    expect(harness.draft.value).toBe("hello <b>safe</b>");
    expect(harness.editor.querySelector("b")).toBeNull();
    expect(harness.instance.$refs.composer.getSelectionRange()).toEqual({
      start: "hello <b>safe</b>".length,
      end: "hello <b>safe</b>".length
    });
  });

  test("preserves file paste and drop hooks for attachment insertion", async () => {
    const seen = [];
    const harness = mountEditor({
      draft: "keep",
      paste(event) {
        seen.push("paste");
        event.preventDefault();
      },
      drop(event) {
        seen.push("drop");
        event.preventDefault();
      }
    });
    const paste = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(paste, "clipboardData", {
      value: { files: [{ name: "paste.png" }], getData: () => "" }
    });
    const drop = new Event("drop", { bubbles: true, cancelable: true });
    Object.defineProperty(drop, "dataTransfer", {
      value: { files: [{ name: "drop.png" }], getData: () => "" }
    });

    harness.editor.dispatchEvent(paste);
    harness.editor.dispatchEvent(drop);
    await nextTick();

    expect(seen).toEqual(["paste", "drop"]);
    expect(harness.draft.value).toBe("keep");
  });

  test("defers decoration rewrites during IME composition", async () => {
    const harness = mountEditor({
      draft: "/ski",
      segments: [{ text: "/ski", kind: null }]
    });
    harness.instance.$refs.composer.focus();
    harness.editor.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    harness.editor.textContent = "/skill";
    harness.editor.dispatchEvent(
      new InputEvent("input", { bubbles: true, data: "ll", inputType: "insertCompositionText" })
    );
    harness.segments.value = [{ text: "/skill", kind: "skill" }];
    await nextTick();
    expect(harness.editor.querySelector("[data-composer-token]")).toBeNull();

    harness.editor.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
    await nextTick();
    expect(harness.editor.querySelector("[data-composer-token]").textContent).toBe("/skill");
    expect(harness.draft.value).toBe("/skill");
  });

  test("maps disabled and placeholder behavior to accessible textbox semantics", async () => {
    const harness = mountEditor({ disabled: true });
    expect(harness.editor.getAttribute("contenteditable")).toBe("false");
    expect(harness.editor.getAttribute("aria-disabled")).toBe("true");
    expect(harness.editor.getAttribute("data-placeholder")).toBe("Message CodeCrab…");
    expect(harness.editor.getAttribute("aria-multiline")).toBe("true");

    harness.disabled.value = false;
    await nextTick();
    expect(harness.editor.getAttribute("contenteditable")).toBe("true");
    expect(harness.editor.getAttribute("aria-disabled")).toBe("false");
  });

  test("preserves undo and redo after semantic DOM rewrites", async () => {
    const harness = mountEditor({
      draft: "/help",
      segments: [{ text: "/help", kind: "command" }]
    });
    harness.instance.$refs.composer.focus();
    harness.instance.$refs.composer.setSelectionRange(5, 5);
    harness.editor.dispatchEvent(
      new InputEvent("beforeinput", {
        bubbles: true,
        cancelable: true,
        data: "!",
        inputType: "insertText"
      })
    );
    harness.editor.querySelector("[data-composer-token]").textContent = "/help!";
    harness.editor.dispatchEvent(
      new InputEvent("input", { bubbles: true, data: "!", inputType: "insertText" })
    );
    await nextTick();
    expect(harness.draft.value).toBe("/help!");

    harness.segments.value = [{ text: "/help!", kind: "invalid" }];
    await nextTick();
    harness.editor.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "z",
        ctrlKey: true,
        bubbles: true,
        cancelable: true
      })
    );
    await nextTick();
    expect(harness.draft.value).toBe("/help");
    expect(harness.instance.$refs.composer.getSelectionRange()).toEqual({
      start: 5,
      end: 5
    });

    harness.editor.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "z",
        ctrlKey: true,
        shiftKey: true,
        bubbles: true,
        cancelable: true
      })
    );
    await nextTick();
    expect(harness.draft.value).toBe("/help!");
  });
});
