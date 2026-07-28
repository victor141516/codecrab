import assert from "node:assert/strict";
import test from "node:test";

import { isScrolledToBottom } from "./scroll.js";

test("scroll tracking pauses above the bottom", () => {
  assert.equal(
    isScrolledToBottom({
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: 300
    }),
    false
  );
});

test("scroll tracking resumes at the bottom with a small pixel tolerance", () => {
  assert.equal(
    isScrolledToBottom({
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: 597
    }),
    true
  );
});
