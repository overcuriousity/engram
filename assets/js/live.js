// Mirrors the SSE connection state into any element marked data-oc-live, e.g. a status bar entry.
function set(state, text) {
  for (const el of document.querySelectorAll("[data-oc-live]")) {
    el.dataset.state = state;
    const label = el.querySelector("span");
    if (label) label.textContent = text;
  }
}

if (typeof document !== "undefined") {
  document.addEventListener("htmx:sseOpen", () => set("online", "Live"));
  document.addEventListener("htmx:sseError", () => set("offline", "Reconnecting"));
}
