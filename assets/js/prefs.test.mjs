import test from "node:test";
import assert from "node:assert/strict";
import { nextTheme } from "./prefs.js";

test("the theme cycles system, light, dark, system", () => {
  assert.equal(nextTheme("system"), "light");
  assert.equal(nextTheme("light"), "dark");
  assert.equal(nextTheme("dark"), "system");
  assert.equal(nextTheme("bogus"), "light");
});
