export async function consumeNdjson(response, onMessage) {
  if (!response.body) {
    throw new Error("The browser did not expose the session stream");
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
      if (line.trim()) await onMessage(JSON.parse(line));
    }
    if (done) break;
  }
  if (buffer.trim()) await onMessage(JSON.parse(buffer));
}

export function acceptsRevision(current, incoming) {
  return Number.isFinite(incoming) && incoming >= current;
}

export function liveTurnState(lifecycle) {
  return {
    sending: lifecycle === "running" || lifecycle === "stopping",
    cancelling: lifecycle === "stopping"
  };
}

export function localStreamEndState(completed, lifecycle) {
  const live = liveTurnState(lifecycle);
  if (!completed && live.sending) {
    return { ...live, keepFollowing: true };
  }
  return { sending: false, cancelling: false, keepFollowing: false };
}

export function mergeCatalog(current, message, currentRevision) {
  if (!acceptsRevision(currentRevision, message?.revision)) {
    return { state: current, revision: currentRevision, applied: false };
  }
  return {
    state: {
      ...current,
      live_revision: message.revision,
      projects: message.projects ?? current?.projects ?? [],
      workers: message.workers ?? current?.workers ?? []
    },
    revision: message.revision,
    applied: true
  };
}
