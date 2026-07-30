export function latestVisibleUserMessage(session) {
  const messages = session?.messages ?? [];
  const nodeIds = session?.active_message_ids ?? [];
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    const nodeId = nodeIds[index];
    if (
      nodeId &&
      message?.role === "user" &&
      !message.hidden &&
      message.content?.trim()
    ) {
      return { nodeId, content: message.content };
    }
  }
  return null;
}

export function recalledMessageForComposer(
  session,
  draft,
  recalledMessageNode
) {
  if (draft !== "" || recalledMessageNode) return null;
  return latestVisibleUserMessage(session);
}
