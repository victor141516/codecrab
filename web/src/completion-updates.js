export function mergeCompletionUpdate(
  current,
  selectedIndex,
  update,
  activeRequestId
) {
  if (!update || update.request_id !== activeRequestId) {
    return { menu: current, selectedIndex, applied: false };
  }
  const selectedId = current?.items?.[selectedIndex]?.id;
  const menu = {
    ...(current ?? {}),
    ...update,
    items: update.items ?? current?.items ?? []
  };
  const preserved = selectedId
    ? menu.items.findIndex((item) => item.id === selectedId)
    : -1;
  const nextSelection =
    preserved >= 0
      ? preserved
      : Math.min(selectedIndex, Math.max(menu.items.length - 1, 0));
  return { menu, selectedIndex: nextSelection, applied: true };
}

export async function consumeCompletionNdjson(response, onMessage) {
  if (!response.body) {
    throw new Error("The browser did not expose the completion stream");
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  while (true) {
    const { value, done } = await reader.read();
    buffer += decoder.decode(value ?? new Uint8Array(), { stream: !done });
    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";
    for (const line of lines) {
      if (line.trim()) onMessage(JSON.parse(line));
    }
    if (done) break;
  }
  if (buffer.trim()) onMessage(JSON.parse(buffer));
}
