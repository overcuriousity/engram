import test from "node:test";
import assert from "node:assert/strict";
import { remainingAfterPause } from "./notices.js";

test("a paused notice keeps the time it had left", () => {
  assert.equal(remainingAfterPause(6000, 2500), 3500);
  assert.equal(remainingAfterPause(6000, 9000), 0);
});
