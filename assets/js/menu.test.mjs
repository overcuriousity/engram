import test from "node:test";
import assert from "node:assert/strict";
import { typeahead } from "./menu.js";

const opts = ["Default", "Compact", "Dark", "Light"];

test("type-ahead finds the next match after the current one and wraps", () => {
  assert.equal(typeahead(opts, "d", 0), 2);
  assert.equal(typeahead(opts, "d", 2), 0);
  assert.equal(typeahead(opts, "co", -1), 1);
  assert.equal(typeahead(opts, "x", 0), -1);
});
