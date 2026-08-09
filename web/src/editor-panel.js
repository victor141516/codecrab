export const EDITOR_MIN_WIDTH = 360;
export const CHAT_MIN_WIDTH = 360;
export const EDITOR_DIVIDER_WIDTH = 5;
export const DEFAULT_EDITOR_WIDTH = 620;

export function clampEditorWidth(width, viewportWidth) {
  return Math.max(
    EDITOR_MIN_WIDTH,
    Math.min(
      Math.max(
        EDITOR_MIN_WIDTH,
        viewportWidth - CHAT_MIN_WIDTH - EDITOR_DIVIDER_WIDTH
      ),
      width
    )
  );
}

export function editorFollowStorageKey(project) {
  return `codecrab:editor-follow:${project ?? ""}`;
}

export function nextUnseenLiveChanges(changes, seen) {
  const incoming = [];
  for (const item of changes) {
    const key = `${item.id}:${item.change}`;
    if (seen.has(key)) continue;
    seen.add(key);
    incoming.push(item.change);
  }
  return incoming;
}
