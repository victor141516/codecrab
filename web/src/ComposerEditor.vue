<script setup>
import { nextTick, onMounted, ref, watch } from "vue";

import {
  composerSelectionOffsets,
  insertComposerText,
  readComposerText,
  renderComposerSegments,
  setComposerSelectionOffsets
} from "./composer-editor.js";

const props = defineProps({
  modelValue: { type: String, required: true },
  segments: { type: Array, default: () => [] },
  disabled: { type: Boolean, default: false },
  placeholder: { type: String, default: "" }
});
const emit = defineEmits([
  "update:modelValue",
  "input",
  "keydown",
  "paste",
  "drop",
  "dragover",
  "select",
  "blur",
  "focus"
]);

const editor = ref(null);
let composing = false;
let pendingRender = false;
let pendingSelection = null;
let pendingInputHistory = null;
let lastEmittedValue = null;
let undoStack = [];
let redoStack = [];
const HISTORY_LIMIT = 100;

function selection() {
  return composerSelectionOffsets(editor.value, editor.value?.ownerDocument.getSelection());
}

function render() {
  const element = editor.value;
  if (!element) return;
  if (composing) {
    pendingRender = true;
    return;
  }
  const segmentsMatch =
    props.segments.map((segment) => segment.text).join("") === props.modelValue;
  const segments = segmentsMatch
    ? props.segments
    : [{ text: props.modelValue, kind: null }];
  const currentText = readComposerText(element);
  if (currentText === props.modelValue && !segmentsMatch && !pendingSelection) {
    return;
  }
  const currentTokens = [...element.querySelectorAll("[data-composer-token]")].map(
    (token) => [token.dataset.composerToken, token.textContent]
  );
  const desiredTokens = segments
    .filter((segment) => segment.kind)
    .map((segment) => [segment.kind, segment.text]);
  if (
    currentText === props.modelValue &&
    !pendingSelection &&
    JSON.stringify(currentTokens) === JSON.stringify(desiredTokens)
  ) {
    return;
  }
  const focused = element.ownerDocument.activeElement === element;
  const restore = pendingSelection ?? (focused ? selection() : null);
  pendingSelection = null;
  renderComposerSegments(element, segments);
  if (restore) {
    setComposerSelectionOffsets(
      element,
      restore.start,
      restore.end,
      element.ownerDocument.getSelection()
    );
  }
  pendingRender = false;
}

function updateFromDom(event) {
  const value = readComposerText(editor.value);
  if (pendingInputHistory && pendingInputHistory.value !== value) {
    undoStack.push(pendingInputHistory);
    if (undoStack.length > HISTORY_LIMIT) undoStack.shift();
    redoStack = [];
  }
  pendingInputHistory = null;
  lastEmittedValue = value;
  emit("update:modelValue", value);
  emit("input", {
    data: event.data ?? null,
    inputType: event.inputType ?? null,
    isComposing: event.isComposing ?? composing
  });
}

function replaceSelection(text, inputType = "insertText") {
  const range = selection();
  const result = insertComposerText(props.modelValue, range.start, range.end, text);
  undoStack.push({ value: props.modelValue, selection: range });
  if (undoStack.length > HISTORY_LIMIT) undoStack.shift();
  redoStack = [];
  pendingSelection = { start: result.cursor, end: result.cursor };
  lastEmittedValue = result.value;
  emit("update:modelValue", result.value);
  emit("input", { data: text, inputType, isComposing: false });
  void nextTick(render);
}

function applyHistory(source, destination, inputType) {
  const entry = source.pop();
  if (!entry) return false;
  destination.push({ value: props.modelValue, selection: selection() });
  pendingSelection = entry.selection;
  lastEmittedValue = entry.value;
  emit("update:modelValue", entry.value);
  emit("input", { data: null, inputType, isComposing: false });
  void nextTick(render);
  return true;
}

function handleKeydown(event) {
  if (
    !event.ctrlKey &&
    !event.metaKey &&
    !event.altKey &&
    (event.key.length === 1 || event.key === "Backspace" || event.key === "Delete")
  ) {
    pendingInputHistory ??= { value: props.modelValue, selection: selection() };
  }
  emit("keydown", event);
  const accelerator = event.ctrlKey || event.metaKey;
  const key = event.key.toLowerCase();
  if (!event.defaultPrevented && accelerator && key === "z") {
    event.preventDefault();
    applyHistory(
      event.shiftKey ? redoStack : undoStack,
      event.shiftKey ? undoStack : redoStack,
      event.shiftKey ? "historyRedo" : "historyUndo"
    );
    return;
  }
  if (!event.defaultPrevented && event.ctrlKey && key === "y") {
    event.preventDefault();
    applyHistory(redoStack, undoStack, "historyRedo");
    return;
  }
  if (event.key === "Enter" && !event.defaultPrevented) {
    event.preventDefault();
    replaceSelection("\n", "insertLineBreak");
  }
}

function handleBeforeInput(event) {
  if (event.inputType === "historyUndo" || event.inputType === "historyRedo") {
    event.preventDefault();
    applyHistory(
      event.inputType === "historyUndo" ? undoStack : redoStack,
      event.inputType === "historyUndo" ? redoStack : undoStack,
      event.inputType
    );
    return;
  }
  if (
    !event.defaultPrevented &&
    (event.inputType === "insertParagraph" || event.inputType === "insertLineBreak")
  ) {
    event.preventDefault();
    replaceSelection("\n", "insertLineBreak");
    return;
  }
  pendingInputHistory ??= { value: props.modelValue, selection: selection() };
}

function handlePaste(event) {
  emit("paste", event);
  if (event.defaultPrevented) return;
  event.preventDefault();
  const text = event.clipboardData?.getData("text/plain").replace(/\r\n?/g, "\n") ?? "";
  if (text) replaceSelection(text, "insertFromPaste");
}

function handleDrop(event) {
  emit("drop", event);
  if (event.defaultPrevented) return;
  const text = event.dataTransfer?.getData("text/plain") ?? "";
  if (!text) return;
  event.preventDefault();
  replaceSelection(text.replace(/\r\n?/g, "\n"), "insertFromDrop");
}

function handleCompositionStart() {
  composing = true;
}

function handleCompositionEnd(event) {
  composing = false;
  if (readComposerText(editor.value) !== props.modelValue) updateFromDom(event);
  if (pendingRender || props.segments.some((segment) => segment.kind)) {
    void nextTick(render);
  }
}

function handleKeyup(event) {
  if (["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) {
    emit("select");
  }
}

function focus() {
  editor.value?.focus();
}

function isFocused() {
  return editor.value?.ownerDocument.activeElement === editor.value;
}

function getSelectionRange() {
  return selection();
}

function setSelectionRange(start, end = start) {
  if (!editor.value) return;
  setComposerSelectionOffsets(
    editor.value,
    start,
    end,
    editor.value.ownerDocument.getSelection()
  );
}

function resize() {
  if (!editor.value) return;
  if (!props.modelValue) editor.value.scrollTop = 0;
}

defineExpose({ focus, isFocused, getSelectionRange, setSelectionRange, resize });

watch(
  () => [props.modelValue, props.segments],
  render,
  { deep: true, flush: "post" }
);

watch(
  () => props.modelValue,
  (value) => {
    if (value === lastEmittedValue) {
      lastEmittedValue = null;
      return;
    }
    undoStack = [];
    redoStack = [];
    pendingInputHistory = null;
  },
  { flush: "sync" }
);

onMounted(() => {
  render();
});
</script>

<template>
  <div
    ref="editor"
    class="composer-editor max-h-48 min-h-12 w-full overflow-y-auto bg-transparent px-4 py-3.5 text-sm leading-6 text-zinc-100 outline-none"
    role="textbox"
    aria-multiline="true"
    :aria-disabled="String(disabled)"
    :contenteditable="disabled ? 'false' : 'true'"
    :data-placeholder="placeholder"
    spellcheck="true"
    @beforeinput="handleBeforeInput"
    @input="updateFromDom"
    @keydown="handleKeydown"
    @keyup="handleKeyup"
    @mouseup="emit('select')"
    @paste="handlePaste"
    @drop="handleDrop"
    @dragover="emit('dragover', $event)"
    @compositionstart="handleCompositionStart"
    @compositionend="handleCompositionEnd"
    @blur="emit('blur', $event)"
    @focus="emit('focus', $event)"
  ></div>
</template>
