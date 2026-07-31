export function reconcileAttachmentBindings(previous, next, bindings) {
  let start = 0;
  while (
    start < previous.length &&
    start < next.length &&
    previous[start] === next[start]
  ) {
    start += 1;
  }
  let previousEnd = previous.length;
  let nextEnd = next.length;
  while (
    previousEnd > start &&
    nextEnd > start &&
    previous[previousEnd - 1] === next[nextEnd - 1]
  ) {
    previousEnd -= 1;
    nextEnd -= 1;
  }
  const delta = nextEnd - start - (previousEnd - start);
  return bindings.flatMap((binding) => {
    const candidate = { ...binding };
    if (candidate.end <= start) {
      // unchanged
    } else if (candidate.start >= previousEnd) {
      candidate.start += delta;
      candidate.end += delta;
    } else {
      return [];
    }
    return next.slice(candidate.start, candidate.end) === candidate.reference
      ? [candidate]
      : [];
  });
}

export function trimAttachmentDraft(text, bindings) {
  const leading = text.length - text.trimStart().length;
  const trailing = text.trimEnd().length;
  return {
    prompt: text.slice(leading, trailing),
    attachments: bindings
      .filter((binding) => binding.start >= leading && binding.end <= trailing)
      .map(({ attachment_id, start, end }) => {
        const relativeStart = start - leading;
        const relativeEnd = end - leading;
        const prompt = text.slice(leading, trailing);
        return {
          attachment_id,
          start: new TextEncoder().encode(prompt.slice(0, relativeStart)).length,
          end: new TextEncoder().encode(prompt.slice(0, relativeEnd)).length
        };
      })
  };
}

export function insertAttachmentReference(text, start, end, attachment) {
  const leading = start > 0 && !/\s/.test(text[start - 1]) ? " " : "";
  const trailing = end < text.length && !/\s/.test(text[end]) ? " " : "";
  const insertion = `${leading}${attachment.reference}${trailing}`;
  const referenceStart = start + leading.length;
  return {
    text: `${text.slice(0, start)}${insertion}${text.slice(end)}`,
    cursor: start + insertion.length,
    binding: {
      attachment_id: attachment.attachment.id,
      reference: attachment.reference,
      start: referenceStart,
      end: referenceStart + attachment.reference.length
    }
  };
}

export function bindingsFromMessage(message) {
  let cursor = 0;
  const bindings = [];
  for (const part of message?.parts ?? []) {
    if (part.type === "text") {
      cursor += part.text.length;
    } else if (part.type === "attachment") {
      bindings.push({
        attachment_id: part.attachment_id,
        reference: part.reference,
        start: cursor,
        end: cursor + part.reference.length
      });
      cursor += part.reference.length;
    }
  }
  return bindings;
}
