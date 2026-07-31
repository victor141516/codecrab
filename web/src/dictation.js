export function isMacPlatform(platform = "") {
  return /mac|iphone|ipad|ipod/i.test(platform);
}

export function isDictationShortcut(event, platform = "") {
  if (
    event.key?.toLowerCase() !== "s" ||
    !event.ctrlKey ||
    event.altKey ||
    event.metaKey
  ) {
    return false;
  }
  return isMacPlatform(platform) || event.shiftKey;
}

export function insertTranscriptAtSelection(
  draft,
  text,
  { focused = false, start = draft.length, end = start } = {}
) {
  const transcript = text.trim();
  if (!transcript) {
    return { inserted: false, value: draft, cursor: focused ? start : draft.length };
  }

  const insertionStart = focused
    ? Math.max(0, Math.min(start, draft.length))
    : draft.length;
  const insertionEnd = focused
    ? Math.max(insertionStart, Math.min(end, draft.length))
    : draft.length;
  const before = draft.slice(0, insertionStart);
  const after = draft.slice(insertionEnd);
  const leading = before && !/\s$/.test(before) ? " " : "";
  const trailing = after && !/^\s/.test(after) ? " " : "";
  const insertion = `${leading}${transcript}${trailing}`;

  return {
    inserted: true,
    value: before + insertion + after,
    cursor: insertionStart + insertion.length
  };
}
