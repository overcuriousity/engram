# System One fixtures

Shared by the JS (`web/js/systemone.test.mjs`: `invalid-*`, the runs of `request-*` and the expected outcomes of `response-*`) and Rust tests. `request-*` and `response-*` bodies come from the real endpoint (`scripts/probe.sh`); `response-*` error cases and `invalid-*` are hand-made.

The endpoint returns the `probabilities` of a choice sorted by name, not in request order (so `"1"`, `"10"`, `"2"`); `expected.probabilities` is in option order, and for a score in level order.

Copied from magicshell's `fixtures/systemone/` (without `invalid-*`, whose caps differ). engram reads only `run`, `model`, `expected_json`, `body` and `expected`.
