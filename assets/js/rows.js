export function isLongPress(downMs, upMs, movedPx) {
  return upMs - downMs >= 500 && movedPx < 10;
}

function rows(tbody) {
  return [...tbody.querySelectorAll("tr[data-oc-row]")];
}

function select(tr, focus = true) {
  const tbody = tr.closest("[data-oc-rows]");
  for (const r of tbody.querySelectorAll("tr.is-selected")) {
    r.classList.remove("is-selected");
    r.setAttribute("aria-selected", "false");
  }
  tr.classList.add("is-selected");
  tr.setAttribute("aria-selected", "true");
  if (focus) tr.focus({ preventScroll: true });
  tr.scrollIntoView({ block: "nearest" });
}

function notice(text) {
  const box = document.getElementById("oc-notices");
  if (!box) return;
  const n = document.createElement("div");
  n.className = "oc-notice oc-notice--danger";
  n.setAttribute("role", "status");
  n.dataset.ocAutohide = "6000";
  const t = document.createElement("span");
  t.className = "oc-notice__text";
  t.textContent = text;
  n.append(t);
  box.prepend(n);
}

let menu = null;

function closeMenu() {
  menu?.remove();
  menu = null;
}

function openMenu(tr, x, y) {
  closeMenu();
  const actions = tr.querySelectorAll(".oc-row-actions button");
  if (!actions.length) return;
  menu = document.createElement("ul");
  menu.className = "oc-menu";
  menu.setAttribute("role", "menu");
  // Items forward to the row's own buttons, so relative htmx targets like "closest tr" still resolve.
  for (const a of actions) {
    const li = document.createElement("li");
    const item = document.createElement("button");
    item.type = "button";
    item.setAttribute("role", "menuitem");
    const icon = a.querySelector("svg");
    if (icon) item.append(icon.cloneNode(true));
    const label = document.createElement("span");
    label.textContent = a.getAttribute("aria-label");
    item.append(label);
    item.addEventListener("click", () => {
      closeMenu();
      a.click();
    });
    li.append(item);
    menu.append(li);
  }
  document.body.append(menu);
  const coarse = window.matchMedia("(pointer: coarse)").matches;
  menu.style.setProperty("position", "fixed");
  if (coarse) {
    menu.style.setProperty("inset", "auto 0 0 0");
  } else {
    menu.style.setProperty("left", `${Math.min(x, window.innerWidth - menu.offsetWidth - 8)}px`);
    menu.style.setProperty("top", `${Math.min(y, window.innerHeight - menu.offsetHeight - 8)}px`);
  }
  menu.querySelector("button")?.focus();
}

if (typeof document !== "undefined") {
  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    const t = e.target;
    if (["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName) || t.isContentEditable) return;
    const tbody = document.querySelector("[data-oc-rows]");
    if (!tbody) return;
    const all = rows(tbody);
    const current = tbody.querySelector("tr.is-selected");
    const i = current ? all.indexOf(current) : -1;
    if (e.key === "j" || e.key === "ArrowDown") {
      e.preventDefault();
      if (all[i + 1]) select(all[i + 1]);
    } else if (e.key === "k" || e.key === "ArrowUp") {
      e.preventDefault();
      if (all[Math.max(i - 1, 0)]) select(all[Math.max(i - 1, 0)]);
    } else if (e.key === "Enter" && current) {
      e.preventDefault();
      (current.querySelector("[data-oc-open]") ?? current).click();
    }
  });

  document.addEventListener("click", (e) => {
    if (menu && !menu.contains(e.target)) closeMenu();
    const tr = e.target.closest?.("tr[data-oc-row]");
    if (tr && !e.target.closest("button, a, input")) {
      select(tr, false);
      tr.querySelector("[data-oc-open]")?.click();
    }
  });

  document.addEventListener("contextmenu", (e) => {
    const tr = e.target.closest?.("tr[data-oc-row]");
    if (!tr) return;
    e.preventDefault();
    select(tr, false);
    openMenu(tr, e.clientX, e.clientY);
  });

  let press = null;
  document.addEventListener("pointerdown", (e) => {
    const tr = e.target.closest?.("tr[data-oc-row]");
    if (!tr || e.pointerType !== "touch") return;
    press = { tr, t: Date.now(), x: e.clientX, y: e.clientY, timer: setTimeout(() => openMenu(tr, e.clientX, e.clientY), 500) };
  });
  const cancel = (e) => {
    if (!press) return;
    const moved = Math.hypot(e.clientX - press.x, e.clientY - press.y);
    if (!isLongPress(press.t, Date.now(), moved)) clearTimeout(press.timer);
    press = null;
  };
  document.addEventListener("pointerup", cancel);
  document.addEventListener("pointercancel", cancel);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeMenu();
  });

  document.addEventListener("htmx:beforeRequest", (e) => {
    const elt = e.detail.elt;
    const sel = elt.dataset?.ocOptimistic;
    if (!sel) return;
    for (const target of document.querySelectorAll(sel)) {
      target.dataset.ocPrevious = target.dataset.state ?? "";
      target.dataset.state = elt.dataset.ocOptimisticState ?? "";
    }
  });
  document.addEventListener("htmx:responseError", (e) => {
    const sel = e.detail.elt.dataset?.ocOptimistic;
    if (!sel) return;
    for (const target of document.querySelectorAll(sel)) target.dataset.state = target.dataset.ocPrevious ?? "";
    notice("Could not save. Try again.");
  });

  document.addEventListener("htmx:afterSettle", (e) => {
    const show = document.getElementById("oc-ingest-show");
    const wrap = document.querySelector(".oc-table-wrap");
    if (show && wrap && wrap.scrollTop === 0 && e.target.id === "oc-ingest-notice") show.click();
  });
}
