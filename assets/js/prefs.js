const THEMES = ["system", "light", "dark"];
const THEME_LABEL = { system: "Use the system theme", light: "Switch to light theme", dark: "Switch to dark theme" };

export function nextTheme(current) {
  const i = Math.max(THEMES.indexOf(current), 0);
  return THEMES[(i + 1) % THEMES.length];
}

function apply(name, value) {
  const html = document.documentElement;
  if (name === "theme") {
    if (value === "system") delete html.dataset.theme;
    else html.dataset.theme = value;
  } else if (name === "density") {
    if (value === "compact") html.dataset.density = "compact";
    else delete html.dataset.density;
  }
}

function persist(name, value) {
  if (document.documentElement.dataset.prefs === "user") {
    fetch("/_oc/prefs", { method: "POST", body: new URLSearchParams({ [name]: value }), credentials: "same-origin" });
    return;
  }
  try {
    if (value === "system" || value === "default") localStorage.removeItem(`oc.${name}`);
    else localStorage.setItem(`oc.${name}`, value);
  } catch {
    /* storage blocked: the choice lasts for this page only */
  }
}

function syncButtons(name, value) {
  for (const b of document.querySelectorAll(`[data-oc-pref="${name}"]`)) {
    b.dataset.ocValue = value;
    if (name === "theme") {
      const label = THEME_LABEL[nextTheme(value)];
      b.setAttribute("aria-label", label);
      b.dataset.ocTip = label;
    } else {
      const compact = value === "compact";
      const label = compact ? "Use default text size" : "Use compact text size";
      b.setAttribute("aria-pressed", String(compact));
      b.setAttribute("aria-label", label);
      b.dataset.ocTip = label;
    }
  }
  for (const s of document.querySelectorAll(`[data-oc-pref-select="${name}"]`)) s.value = value;
}

function set(name, value) {
  apply(name, value);
  syncButtons(name, value);
  persist(name, value);
}

if (typeof document !== "undefined") {
  document.addEventListener("click", (e) => {
    const b = e.target.closest?.("[data-oc-pref]");
    if (!b) return;
    const name = b.dataset.ocPref;
    const current = b.dataset.ocValue;
    set(name, name === "theme" ? nextTheme(current) : current === "compact" ? "default" : "compact");
  });
  document.addEventListener("change", (e) => {
    const s = e.target.closest?.("[data-oc-pref-select]");
    if (s) set(s.dataset.ocPrefSelect, s.value);
  });
}
