export function formatEventTimestamp(value) {
  if (!value) return undefined;
  const timestamp = new Date(value);
  if (Number.isNaN(timestamp.getTime())) return undefined;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "long"
  }).format(timestamp);
}

export function activityEventTimestamp(activity) {
  if (!activity) return undefined;
  return activity.status === "running"
    ? activity.started_at
    : activity.completed_at ?? activity.started_at;
}
