import test from "node:test";
import assert from "node:assert/strict";
import { decideBody, bodyText, problem, progressText } from "./decide.js";

const opt = (name, description = "") => ({ name, description });

test("yes or no sends the question alone", () => {
  assert.deepEqual(decideBody(" Keep it? ", { kind: "noul", options: [], levels: [] }), { q: "Keep it?" });
});

test("options are sent in the order they were typed", () => {
  const row = { kind: "choice", options: [opt("switch", "Move"), opt(" keep ")], levels: [] };
  assert.ok(bodyText("Which?", row).includes('"criteria":{"switch":"Move","keep":""}'));
});

test("blank option rows are dropped", () => {
  const row = { kind: "choice", options: [opt("a"), opt("  ", " "), opt("b")], levels: [] };
  const b = decideBody("Q", row);
  assert.deepEqual(Object.keys(b.questions.verdict.criteria), ["a", "b"]);
});

test("a scale sends its levels lowest first", () => {
  const row = { kind: "score", options: [], levels: ["can wait", " this month", "now", ""] };
  const v = decideBody("Q", row).questions.verdict;
  assert.equal(v.type, "score");
  assert.deepEqual(v.criteria, ["can wait", "this month", "now"]);
});

test("a row that cannot be sent says why in one line", () => {
  const many = (n) => Array.from({ length: n }, (_, i) => opt(`o${i}`));
  const lv = (n) => Array.from({ length: n }, (_, i) => `l${i}`);
  const c = (options) => ({ kind: "choice", options, levels: [] });
  const s = (levels) => ({ kind: "score", options: [], levels });
  assert.equal(problem("", c(many(2))), "Type a question first");
  assert.equal(problem("Q", c(many(1))), "Name at least two options");
  assert.equal(problem("Q", c([opt("a"), opt("a ")])), "Use each option once");
  assert.equal(problem("Q", c(many(9))), "At most eight options");
  assert.equal(problem("Q", s(lv(1))), "Name at least two levels");
  assert.equal(problem("Q", s(lv(11))), "At most ten levels");
  assert.equal(problem("Q", c(many(2))), "");
  assert.equal(problem("Q", { kind: "noul", options: [], levels: [] }), "");
});

test("integer-like option names keep their order", () => {
  const row = { kind: "choice", options: [opt("10"), opt("2"), opt("1")], levels: [] };
  assert.ok(bodyText("Q", row).includes('"criteria":{"10":"","2":"","1":""}'));
});

test("progress counts reads, then weighs", () => {
  assert.equal(progressText(3, 8, 2), "Reading 3 of 8");
  assert.equal(progressText(8, 8, 6), "Weighing 6 together");
  assert.equal(progressText(8, 8, 0), "");
});
