export const LIMITS = { options: [2, 8], levels: [2, 10] };

function options(row) {
  return row.options
    .map((o) => ({ name: o.name.trim(), description: o.description.trim() }))
    .filter((o) => o.name);
}

// Held to its bounds rather than refused; a field that does not read as a number is the starting five.
export function ratingTop(value) {
  const n = Number.parseInt(String(value).trim(), 10);
  if (Number.isNaN(n)) return 5;
  return Math.min(Math.max(n, LIMITS.levels[0]), LIMITS.levels[1]);
}

// A rating is a score whose levels are 1 to N; only the two ends can carry a name.
function levels(row) {
  const n = ratingTop(row.top);
  const ends = [row.low.trim(), row.high.trim()];
  return Array.from({ length: n }, (_, i) => {
    const label = i === 0 ? ends[0] : i === n - 1 ? ends[1] : "";
    return label ? `${i + 1} (${label})` : String(i + 1);
  });
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
  }
  return "";
}

export function progressText(read, of, bearing) {
  if (read < of) return `Reading ${read} of ${of}`;
  if (read === of && bearing > 0) return `Weighing ${bearing} together`;
  return "";
}

if (typeof document !== "undefined") {
  globalThis.engramDecide = { bodyText, problem, progressText, ratingTop, LIMITS };
}
