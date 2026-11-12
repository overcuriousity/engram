import test from "node:test";
import assert from "node:assert/strict";
import { confirmEnabled } from "./dialog.js";

test("the destructive button enables only on the exact name", () => {
  assert.equal(confirmEnabled("ACME breach", "ACME breach"), true);
  assert.equal(confirmEnabled("acme breach", "ACME breach"), false);
  assert.equal(confirmEnabled("ACME breach ", "ACME breach"), false);
  assert.equal(confirmEnabled("", ""), false);
});
