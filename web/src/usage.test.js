import test from "node:test";
import assert from "node:assert/strict";
import {
  canResetUsage,
  formatUsageDuration,
  resetOutcomeMessage,
  usageIndicator
} from "./usage.js";

const currentUsage = {
  available: true,
  stale: false,
  can_reset: true,
  snapshot: {
    windows: [
      {
        remaining_percent: 63,
        resets_at: 1786826526
      }
    ],
    reset_credits: {
      available_count: 2
    }
  }
};

test("usage indicator shows provider-reported remaining quota and reset time", () => {
  const text = usageIndicator(currentUsage, "en-GB", "UTC");
  assert.equal(text, "63% remaining · Resets Sat 20:42");
});

test("manual reset is disabled for stale or creditless usage", () => {
  assert.equal(canResetUsage(currentUsage), true);
  assert.equal(
    canResetUsage({ ...currentUsage, stale: true, can_reset: false }),
    false
  );
  assert.equal(
    canResetUsage({
      ...currentUsage,
      can_reset: false,
      snapshot: {
        ...currentUsage.snapshot,
        reset_credits: { available_count: 0 }
      }
    }),
    false
  );
});

test("provider-defined window lengths are described without hardcoded tiers", () => {
  assert.equal(formatUsageDuration(604800), "1 week");
  assert.equal(formatUsageDuration(1209600), "2 weeks");
  assert.equal(formatUsageDuration(172800), "2 days");
});

test("reset outcomes are rendered without treating no-op results as success", () => {
  assert.equal(
    resetOutcomeMessage({ outcome: "reset", windows_reset: 2 }),
    "Usage reset completed. 2 windows reset."
  );
  assert.equal(
    resetOutcomeMessage({ outcome: "nothing_to_reset", windows_reset: 0 }),
    "No current usage window was eligible for reset."
  );
  assert.equal(
    resetOutcomeMessage({ outcome: "already_redeemed", windows_reset: 0 }),
    "This reset request was already redeemed."
  );
});
