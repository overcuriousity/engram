import test from "node:test";
import assert from "node:assert/strict";
import { isLongPress } from "./rows.js";

test("a long press is at least 500 ms with under 10 px of movement", () => {
  assert.equal(isLongPress(0, 500, 3), true);
  assert.equal(isLongPress(0, 499, 0), false);
  assert.equal(isLongPress(0, 900, 12), false);
});
