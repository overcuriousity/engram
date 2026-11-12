import test from "node:test";
import assert from "node:assert/strict";
import { matchesShortcut, shouldIgnore } from "./keys.js";

test("shortcuts never fire while typing in a field", () => {
  for (const tagName of ["INPUT", "TEXTAREA", "SELECT"]) assert.equal(shouldIgnore({ tagName }), true);
  assert.equal(shouldIgnore({ tagName: "DIV", isContentEditable: true }), true);
  assert.equal(shouldIgnore({ tagName: "BUTTON" }), false);
  assert.equal(shouldIgnore(null), false);
});

test("letter shortcuts match without Shift, symbols match exactly", () => {
  assert.equal(matchesShortcut("F", "f"), true);
  assert.equal(matchesShortcut("F", "F"), true);
  assert.equal(matchesShortcut("\\", "\\"), true);
  assert.equal(matchesShortcut("F", "g"), false);
  assert.equal(matchesShortcut("/", "?"), false);
});
