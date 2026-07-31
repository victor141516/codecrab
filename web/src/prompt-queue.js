export function createPromptQueue() {
  return {
    items: [],
    nextId: 1,
    editingId: null,
    steeredId: null
  };
}

export function enqueuePrompt(
  queue,
  content,
  attachments = [],
  composerAttachments = []
) {
  const item = { id: queue.nextId, content, attachments, composerAttachments };
  queue.nextId += 1;
  queue.items.push(item);
  return item.id;
}

export function updateQueuedPrompt(
  queue,
  id,
  content,
  attachments = [],
  composerAttachments = []
) {
  const item = queue.items.find((candidate) => candidate.id === id);
  if (!item) return false;
  item.content = content;
  item.attachments = attachments;
  item.composerAttachments = composerAttachments;
  return true;
}

export function removeQueuedPrompt(queue, id) {
  const index = queue.items.findIndex((candidate) => candidate.id === id);
  if (index < 0) return null;
  if (queue.editingId === id) queue.editingId = null;
  if (queue.steeredId === id) queue.steeredId = null;
  return queue.items.splice(index, 1)[0];
}

export function steerQueuedPrompt(queue, id) {
  if (!queue.items.some((item) => item.id === id)) return false;
  queue.steeredId = id;
  return true;
}

export function takeNextQueuedPrompt(queue) {
  const steered = queue.items.find((item) => item.id === queue.steeredId);
  const candidate = steered ?? queue.items[0];
  if (!candidate || candidate.id === queue.editingId) return null;
  queue.steeredId = null;
  return removeQueuedPrompt(queue, candidate.id);
}
