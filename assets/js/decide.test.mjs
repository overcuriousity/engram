import test from "node:test";
import assert from "node:assert/strict";
import { bodyText, problem, progressText, ratingTop } from "./decide.js";

const opt = (name, description = "") => ({ name, description });
const sent = (q, row) => JSON.parse(bodyText(q, row));
const rating = (top, low = "", high = "") => ({ kind: "score", options: [], top, low, high });

test("yes or no sends the question alone", () => {
  assert.deepEqual(sent(" Keep it? ", { kind: "noul", options: [] }), { q: "Keep it?" });
});

test("options are sent in the order they were typed", () => {
  const row = { kind: "choice", options: [opt("switch", "Move"), opt(" keep ")] };
  assert.ok(bodyText("Which?", row).includes('"criteria":{"switch":"Move","keep":""}'));
});

test("blank option rows are dropped", () => {
  const row = { kind: "choice", options: [opt("a"), opt("  ", " "), opt("b")] };
  const b = sent("Q", row);
  assert.deepEqual(Object.keys(b.questions.verdict.criteria), ["a", "b"]);
});

test("a rating sends one to N, the two ends carrying their labels", () => {
  const body = bodyText("Q", rating("5", " not at all ", "completely"));
  assert.ok(body.includes('"criteria":["1 (not at all)","2","3","4","5 (completely)"]'), body);
  assert.equal(sent("Q", rating("5", "x", "y")).questions.verdict.type, "score");
});

test("a rating without labels is its numbers", () => {
  assert.deepEqual(sent("Q", rating("5")).questions.verdict.criteria, ["1", "2", "3", "4", "5"]);
  assert.deepEqual(sent("Q", rating("3", "  ", "best")).questions.verdict.criteria, ["1", "2", "3 (best)"]);
  assert.deepEqual(sent("Q", rating("2", "no", "yes")).questions.verdict.criteria, ["1 (no)", "2 (yes)"]);
});

test("a rating's top stays between two and ten", () => {
  assert.equal(ratingTop("2"), 2);
  assert.equal(ratingTop("10"), 10);
  assert.equal(ratingTop("1"), 2);
  assert.equal(ratingTop("-4"), 2);
  assert.equal(ratingTop("11"), 10);
  assert.equal(ratingTop("400"), 10);
  assert.equal(ratingTop(" 7 "), 7);
  assert.equal(ratingTop("3.9"), 3);
  assert.equal(ratingTop(""), 5);
  assert.equal(ratingTop("many"), 5);
  assert.equal(sent("Q", rating("0")).questions.verdict.criteria.length, 2);
  assert.equal(sent("Q", rating("99", "", "most")).questions.verdict.criteria[9], "10 (most)");
  assert.equal(sent("Q", rating("")).questions.verdict.criteria.length, 5);
});

test("a row that cannot be sent says why in one line", () => {
  const many = (n) => Array.from({ length: n }, (_, i) => opt(`o${i}`));
  const c = (options) => ({ kind: "choice", options });
  assert.equal(problem("", c(many(2))), "Type a question first");
  assert.equal(problem("Q", c(many(1))), "Name at least two options");
  assert.equal(problem("Q", c([opt("a"), opt("a ")])), "Use each option once");
  assert.equal(problem("Q", c(many(9))), "At most eight options");
  assert.equal(problem(" ", rating("5")), "Type a question first");
  assert.equal(problem("Q", rating("5", "low", "high")), "");
  assert.equal(problem("Q", rating("99")), "");
  assert.equal(problem("Q", rating("")), "");
  assert.equal(problem("Q", c(many(2))), "");
  assert.equal(problem("Q", { kind: "noul", options: [] }), "");
});

test("integer-like option names keep their order", () => {
  const row = { kind: "choice", options: [opt("10"), opt("2"), opt("1")] };
  assert.ok(bodyText("Q", row).includes('"criteria":{"10":"","2":"","1":""}'));
});

test("progress counts reads, then weighs", () => {
  assert.equal(progressText(3, 8, 2), "Reading 3 of 8");
  assert.equal(progressText(8, 8, 6), "Weighing 6 together");
  assert.equal(progressText(8, 8, 0), "Weighing the closest together");
  assert.equal(progressText(0, 0, 0), "");
});

test("an option with no name is no option", () => {
  const row = { kind: "choice", options: [opt("a"), opt("", "only a description")] };
  assert.deepEqual(Object.keys(sent("Q", row).questions.verdict.criteria), ["a"]);
  const three = { kind: "choice", options: [opt("", "x"), opt("", "y"), opt("a")] };
  assert.equal(problem("Q", three), "Name at least two options");
});
