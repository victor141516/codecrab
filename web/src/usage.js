export function formatUsageReset(
  timestamp,
  locale = undefined,
  timeZone = undefined,
  detailed = false
) {
  if (!Number.isFinite(timestamp)) return "Unknown";
  const options = detailed
    ? {
        weekday: "short",
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        hourCycle: "h23",
        timeZoneName: "short"
      }
    : {
        weekday: "short",
        hour: "2-digit",
        minute: "2-digit",
        hourCycle: "h23"
      };
  if (timeZone) options.timeZone = timeZone;
  return new Intl.DateTimeFormat(locale, options).format(
    new Date(timestamp * 1000)
  );
}

export function formatUsageDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return "Unknown window";
  if (seconds % 604800 === 0) {
    const weeks = seconds / 604800;
    return `${weeks} week${weeks === 1 ? "" : "s"}`;
  }
  if (seconds % 86400 === 0) {
    const days = seconds / 86400;
    return `${days} day${days === 1 ? "" : "s"}`;
  }
  if (seconds % 3600 === 0) {
    const hours = seconds / 3600;
    return `${hours} hour${hours === 1 ? "" : "s"}`;
  }
  return `${seconds} seconds`;
}

export function usageIndicator(
  usage,
  locale = undefined,
  timeZone = undefined
) {
  if (!usage?.available) return "";
  const window = usage.snapshot?.windows?.[0];
  if (!window) return "Usage unavailable";
  return `${window.remaining_percent}% remaining · Resets ${formatUsageReset(
    window.resets_at,
    locale,
    timeZone
  )}${usage.stale ? " · stale" : ""}`;
}

export function canResetUsage(usage) {
  return Boolean(usage?.available && usage.can_reset);
}

export function resetOutcomeMessage(result) {
  switch (result?.outcome) {
    case "reset": {
      const count = result.windows_reset ?? 0;
      return `Usage reset completed. ${count} window${count === 1 ? "" : "s"} reset.`;
    }
    case "nothing_to_reset":
      return "No current usage window was eligible for reset.";
    case "no_credit":
      return "No reset credits are available.";
    case "already_redeemed":
      return "This reset request was already redeemed.";
    default:
      return "Reset request completed.";
  }
}
