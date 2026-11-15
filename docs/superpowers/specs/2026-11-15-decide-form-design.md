# The Decide form: one click, plain fields

Owner's decisions, 2026-11-15. A small change to the web form only; the API, CLI and MCP are
untouched.

## Why

Three things in the form under the search box got in the way:

- Decide had to be pressed twice: once to open the "Answer" row, then again, back up at the
  button, after the row was filled in.
- Each option carried its own "describe" fold-out, which read as clutter and hid the field.
- "scale" took one named level per field, lowest first. Typing 1 and 10 gave two levels called
  "1" and "10", not a range from 1 to 10, and nothing said so.

## Approaches

- **Flow.** (a) Decide runs at once as yes/no and a toggle beside it opens the answer shape, whose
  row ends in its own Decide button — chosen: the common case is one click and the custom case
  never sends the pointer back up. (b) Keep the two-step opening and add a button to the row:
  yes/no still takes two clicks. (c) Keep the row always open: no opening step, a line of height
  on every search.
- **Descriptions.** (a) One option per line, name then an always-visible "what it means
  (optional)" — chosen. (b) One switch that reveals every description. (c) Names only.
- **Scale.** (a) "rating": from 1 to N with optional labels for the two ends — chosen, it is what
  the fields were taken for. (b) Named levels with help text and a preset. (c) Both. (d) Remove.

## Design

- **Decide** runs the decision with the shape that is showing. With the row closed that is
  yes/no, so Decide is one click, like Ask. `Alt`+`Enter` in the box does the same.
- **A toggle beside Decide** (one control group with it; `aria-expanded`, `aria-controls`) opens
  and closes the "Answer" row. Closing it returns the shape to yes/no for the next run; what was
  typed is kept for when it is opened again. `Escape` in the row closes it and returns to the box.
- **The row** offers `yes / no`, `options`, `rating`. It ends in its own **Decide** button, which
  is the same act as the one above (same disabled states), and `Enter` in any of its fields runs
  it. The row says so next to the button.
- **options**: one line per option — the name (40 characters), then "what it means (optional)".
  Two lines to start, "Add an option" up to eight. No `<details>`.
- **rating**: "Rating from 1 to N", N from 2 to 10, 5 to start, and two optional fields, "1
  means" and "N means". It is sent as the existing `score` type: the levels `1`…`N` in order,
  the lowest and the highest written `1 (label)` and `N (label)` when a label is given. The
  answer's bars carry those names as any level's.
- The problem line (`#decide-problem`) keeps its wording for the cases that remain; the level
  count can no longer be wrong.
- Phone (390 px): every field and button is at least 44 px high; an option's two fields stack.

## Constraints

The dev manifest's rules for UI hold (`manifest/10`–`40`): tokens only, no inline styles or
scripts, symbol + colour for status, one primary per view (Decide never takes the primary
style), touch targets. The page works without the row's script only as far as it did before.

## Not in scope

More than one question per decision in the web form; named levels (the API keeps them).

## Left to the builder

The toggle's icon and tooltip; the exact wording of the Enter hint; how the two Decide buttons
share their disabled state.
