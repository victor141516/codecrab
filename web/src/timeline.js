function eventSequence(event) {
  return event.type === "message"
    ? event.message.sequence
    : event.activity.sequence;
}

export function sortChronologically(events) {
  if (
    events.length < 2 ||
    !events.every((event) => Number.isSafeInteger(eventSequence(event)))
  ) {
    return events;
  }
  return [...events].sort(
    (left, right) => eventSequence(left) - eventSequence(right)
  );
}
