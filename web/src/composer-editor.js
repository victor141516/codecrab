function textLength(node) {
  return node?.textContent?.length ?? 0;
}

function nodeBaseOffset(root, node) {
  if (node === root) return 0;
  let current = node;
  let offset = 0;
  while (current && current !== root) {
    let sibling = current.previousSibling;
    while (sibling) {
      offset += textLength(sibling);
      sibling = sibling.previousSibling;
    }
    current = current.parentNode;
  }
  return current === root ? offset : null;
}

function pointOffset(root, node, offset) {
  const base = nodeBaseOffset(root, node);
  if (base === null) return null;
  if (node.nodeType === node.TEXT_NODE) {
    return base + Math.min(Math.max(offset, 0), node.data.length);
  }
  const children = [...node.childNodes];
  return (
    base +
    children
      .slice(0, Math.min(Math.max(offset, 0), children.length))
      .reduce((length, child) => length + textLength(child), 0)
  );
}

function textNodes(root) {
  const nodes = [];
  const visit = (node) => {
    if (node.nodeType === node.TEXT_NODE) {
      nodes.push(node);
      return;
    }
    for (const child of node.childNodes) visit(child);
  };
  visit(root);
  return nodes;
}

function pointAtOffset(root, requested) {
  const length = textLength(root);
  let remaining = Math.min(Math.max(requested, 0), length);
  const nodes = textNodes(root);
  for (const node of nodes) {
    if (remaining <= node.data.length) return [node, remaining];
    remaining -= node.data.length;
  }
  return [root, root.childNodes.length];
}

export function readComposerText(editor) {
  return editor?.textContent ?? "";
}

export function renderComposerSegments(editor, segments) {
  const document = editor.ownerDocument;
  const fragment = document.createDocumentFragment();
  for (const segment of segments) {
    if (!segment.text) continue;
    if (!segment.kind) {
      fragment.append(document.createTextNode(segment.text));
      continue;
    }
    const token = document.createElement("span");
    token.dataset.composerToken = segment.kind;
    token.className = `composer-token composer-token-${segment.kind}`;
    token.append(document.createTextNode(segment.text));
    fragment.append(token);
  }
  editor.replaceChildren(fragment);
}

export function composerSelectionOffsets(editor, selection) {
  const length = textLength(editor);
  if (!selection?.rangeCount) return { start: length, end: length };
  const anchor = pointOffset(editor, selection.anchorNode, selection.anchorOffset);
  const focus = pointOffset(editor, selection.focusNode, selection.focusOffset);
  if (anchor === null || focus === null) return { start: length, end: length };
  return {
    start: Math.min(anchor, focus),
    end: Math.max(anchor, focus)
  };
}

export function setComposerSelectionOffsets(editor, start, end, selection) {
  if (!selection) return;
  const document = editor.ownerDocument;
  const [startNode, startOffset] = pointAtOffset(editor, Math.min(start, end));
  const [endNode, endOffset] = pointAtOffset(editor, Math.max(start, end));
  const range = document.createRange();
  range.setStart(startNode, startOffset);
  range.setEnd(endNode, endOffset);
  selection.removeAllRanges();
  selection.addRange(range);
}

export function insertComposerText(value, start, end, insertion) {
  const from = Math.min(Math.max(Math.min(start, end), 0), value.length);
  const to = Math.min(Math.max(Math.max(start, end), 0), value.length);
  return {
    value: `${value.slice(0, from)}${insertion}${value.slice(to)}`,
    cursor: from + insertion.length
  };
}
