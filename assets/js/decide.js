export const LIMITS = { options: [2, 8], levels: [2, 10] };

function options(row) {
  return row.options
    .map((o) => ({ name: o.name.trim(), description: o.description.trim() }))
    .filter((o) => o.name || o.description);
}

function levels(row) {
  return row.levels.map((l) => l.trim()).filter(Boolean);
}

export function decideBody(q, row) {
  const body = { q: q.trim() };
  if (row.kind === "choice") {
    const criteria = {};
    for (const o of options(row)) criteria[o.name] = o.description;
    body.questions = { verdict: { type: "choice", criteria } };
  } else if (row.kind === "score") {
    body.questions = { verdict: { type: "score", criteria: levels(row) } };
  }
  return body;
}

// Assembled as text: a JS object would reorder integer-like option names.
export function bodyText(q, row) {
  const head = `"q":${JSON.stringify(q.trim())}`;
  if (row.kind === "choice") {
    const pairs = options(row).map((o) => `${JSON.stringify(o.name)}:${JSON.stringify(o.description)}`);
    return `{${head},"questions":{"verdict":{"type":"choice","criteria":{${pairs.join(",")}}}}}`;
  }
  if (row.kind === "score") {
    return `{${head},"questions":{"verdict":{"type":"score","criteria":${JSON.stringify(levels(row))}}}}`;
  }
  return `{${head}}`;
}

export function problem(q, row) {
  if (!q.trim()) return "Type a question first";
  if (row.kind === "choice") {
    const names = options(row).map((o) => o.name);
    if (names.length < LIMITS.options[0]) return "Name at least two options";
    if (names.length > LIMITS.options[1]) return "At most eight options";
    if (new Set(names).size < names.length) return "Use each option once";
  } else if (row.kind === "score") {
    const n = levels(row).length;
    if (n < LIMITS.levels[0]) return "Name at least two levels";
    if (n > LIMITS.levels[1]) return "At most ten levels";
  }
  return "";
}

export function progressText(read, of, bearing) {
  if (read < of) return `Reading ${read} of ${of}`;
  if (read === of && bearing > 0) return `Weighing ${bearing} together`;
  return "";
}

if (typeof document !== "undefined") {
  globalThis.engramDecide = { decideBody, bodyText, problem, progressText, LIMITS };
}
