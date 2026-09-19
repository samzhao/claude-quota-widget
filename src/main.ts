import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type UsageWindow = {
  key: string;
  label: string;
  utilization: number;
  resets_at: string | null;
  severity: string | null;
};

type AccountUsage = {
  id: string;
  label: string;
  plan: string | null;
  read_only: boolean;
  status: "ok" | "needs_login" | "rate_limited" | "error";
  message: string | null;
  windows: UsageWindow[];
  as_of_ms: number | null;
  next_check_ms: number | null;
};

type View = "grid" | "cards";
type Theme = "warm" | "instrument";
type AppSettings = { pinned: boolean; show_dock_icon: boolean };
type Level = "normal" | "warning" | "critical";

const GAUGE_CELLS = 10;

// The chip answers one question: is this account's login still working?
const STATUS_TEXT: Record<AccountUsage["status"], string> = {
  ok: "connected",
  needs_login: "needs login",
  rate_limited: "throttled",
  error: "error",
};
const STATUS_HELP: Record<AccountUsage["status"], string> = {
  ok: "Login is valid and usage numbers are live.",
  needs_login: "The stored login no longer works, so usage can't be fetched.",
  rate_limited: "Anthropic is throttling usage checks. Numbers may be stale.",
  error: "The last usage check failed. Numbers may be stale.",
};

const $ = <T extends HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const accountsEl = $("#accounts");
const updatedEl = $("#updated");
const refreshBtn = $<HTMLButtonElement>("#refresh");
const gridBtn = $<HTMLButtonElement>("#view-grid");
const cardsBtn = $<HTMLButtonElement>("#view-cards");
const addForm = $<HTMLFormElement>("#add-form");
const emailInput = $<HTMLInputElement>("#email");
const emailToggle = $<HTMLButtonElement>("#email-toggle");
const addBtn = $<HTMLButtonElement>("#add");
const cancelBtn = $<HTMLButtonElement>("#cancel");
const hideUnusedInput = $<HTMLInputElement>("#hide-unused");
const themeSelect = $<HTMLSelectElement>("#theme");
const compactInput = $<HTMLInputElement>("#compact");
const compactWrap = $("#compact-wrap");
const pinBtn = $<HTMLButtonElement>("#pin");
const dockInput = $<HTMLInputElement>("#dock-icon");
const noticeEl = $("#notice");

// Per-viewer conveniences only; the app works the same if storage is unavailable.
function loadSetting(key: string, fallback: string): string {
  try {
    return localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}
function saveSetting(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* ignore */
  }
}

// "detail" is what the cards view was called in earlier builds.
let view: View = ["cards", "detail"].includes(loadSetting("view", "grid")) ? "cards" : "grid";
let compactCards = loadSetting("compactCards", "0") === "1";
let hideUnused = loadSetting("hideUnused", "1") === "1";
let theme: Theme = loadSetting("theme", "warm") === "instrument" ? "instrument" : "warm";
let lastAccounts: AccountUsage[] = [];

function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

function ago(ms: number, short = false): string {
  const minutes = Math.max(0, Math.floor((Date.now() - ms) / 60000));
  if (minutes < 1) return short ? "now" : "just now";
  if (minutes < 60) return short ? `${minutes}m ago` : `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  return short ? `${hours}h ${minutes % 60}m ago` : `${hours} h ${minutes % 60} min ago`;
}

/** The last check failed but the login itself is fine. */
function updateFailed(a: AccountUsage): boolean {
  return a.status === "rate_limited" || a.status === "error";
}

function failureReason(a: AccountUsage): string {
  return a.status === "rate_limited" ? "API throttling" : (a.message ?? "request failed");
}

function failureDetail(a: AccountUsage): string {
  const parts = [
    a.status === "rate_limited"
      ? "Anthropic is throttling usage checks for this account."
      : `The last usage check failed: ${a.message ?? "unknown reason"}.`,
  ];
  if (a.as_of_ms) parts.push(`Showing the reading from ${clock(a.as_of_ms)}.`);
  if (a.next_check_ms) parts.push(`Next try ${clock(a.next_check_ms)}.`);
  return parts.join(" ");
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

/**
 * Extra limits the endpoint lists but this plan never uses: 0% with no reset
 * clock. The core limits are never hidden, because an idle 5-hour window
 * looks exactly the same (0%, no reset) until the next message starts it.
 */
function isCoreLimit(w: UsageWindow): boolean {
  return w.key === "session" || w.key === "weekly_all" || w.key.startsWith("weekly_scoped:");
}
function isUnused(w: UsageWindow): boolean {
  return !isCoreLimit(w) && w.utilization === 0 && !w.resets_at;
}

function timeLeft(iso: string | null): string | null {
  if (!iso) return null;
  const ms = new Date(iso).getTime() - Date.now();
  if (Number.isNaN(ms)) return null;
  if (ms <= 0) return "now";
  const minutes = Math.round(ms / 60000);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ${minutes % 60}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

function levelOf(w: UsageWindow): Level {
  if (w.severity === "normal" || w.severity === "warning" || w.severity === "critical") {
    return w.severity;
  }
  if (w.utilization >= 90) return "critical";
  if (w.utilization >= 70) return "warning";
  return "normal";
}

/** Ten-cell fuel gauge. Partial cells round up so 4% still lights one cell. */
function renderGauge(w: UsageWindow): HTMLElement {
  const pct = Math.max(0, Math.min(100, w.utilization));
  const lit = pct === 0 ? 0 : Math.max(1, Math.round((pct / 100) * GAUGE_CELLS));
  const gauge = el("div", `gauge ${levelOf(w)}`);
  gauge.setAttribute("role", "meter");
  gauge.setAttribute("aria-valuemin", "0");
  gauge.setAttribute("aria-valuemax", "100");
  gauge.setAttribute("aria-valuenow", String(Math.round(pct)));
  gauge.setAttribute("aria-label", `${w.label} used`);
  gauge.style.setProperty("--pct", `${pct}%`);
  for (let i = 0; i < GAUGE_CELLS; i++) {
    gauge.append(el("i", i < lit ? "lit" : undefined));
  }
  return gauge;
}

function renderReading(w: UsageWindow, resetPrefix: string): HTMLElement {
  const reading = el("div", "reading");
  reading.append(el("span", `pct ${levelOf(w)}`, `${Math.round(w.utilization)}%`));
  const left = timeLeft(w.resets_at);
  const reset = el("span", "reset", left ? `${resetPrefix}${left}` : "idle");
  reset.title = w.resets_at
    ? `Resets ${new Date(w.resets_at).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })}`
    : "This limit's clock starts with the next message.";
  reading.append(reset);
  return reading;
}

function statusDot(a: AccountUsage): HTMLElement {
  const dot = el("span", `dot ${a.status}`);
  dot.title = updateFailed(a)
    ? failureDetail(a)
    : `${STATUS_TEXT[a.status]}: ${STATUS_HELP[a.status]}`;
  dot.setAttribute("role", "img");
  dot.setAttribute("aria-label", STATUS_TEXT[a.status]);
  return dot;
}

/* ---------- Grid view: accounts down, limits across ---------- */

function gridColumns(accounts: AccountUsage[]): { key: string; label: string }[] {
  const columns = new Map<string, string>();
  for (const a of accounts) {
    for (const w of a.windows) {
      if (hideUnused && isUnused(w)) continue;
      if (!columns.has(w.key)) columns.set(w.key, w.label);
    }
  }
  // Accounts that are all idle would otherwise produce an empty board.
  if (columns.size === 0) {
    for (const a of accounts) for (const w of a.windows) columns.set(w.key, w.label);
  }
  return [...columns].map(([key, label]) => ({ key, label }));
}

/** Space is tight in the grid, so show "sam" for sam@example.com unless that is ambiguous. */
function shortLabels(accounts: AccountUsage[]): Map<string, string> {
  const local = (label: string) => (label.includes("@") ? label.slice(0, label.indexOf("@")) : label);
  const counts = new Map<string, number>();
  for (const a of accounts) counts.set(local(a.label), (counts.get(local(a.label)) ?? 0) + 1);
  return new Map(
    accounts.map((a) => [a.id, counts.get(local(a.label)) === 1 ? local(a.label) : a.label]),
  );
}

function renderGrid(accounts: AccountUsage[]): HTMLElement {
  const columns = gridColumns(accounts);
  const names = shortLabels(accounts);
  const board = el("div", "board");
  board.style.setProperty("--limit-columns", String(Math.max(1, columns.length)));
  board.setAttribute("role", "table");

  const headRow = el("div", "board-row board-head");
  headRow.setAttribute("role", "row");
  const corner = el("span", "board-account", "Account");
  corner.setAttribute("role", "columnheader");
  headRow.append(corner);
  for (const column of columns) {
    const th = el("span", "board-cell", column.label);
    th.setAttribute("role", "columnheader");
    headRow.append(th);
  }
  board.append(headRow);

  for (const a of accounts) {
    const row = el("div", "board-row");
    row.setAttribute("role", "row");

    const name = el("div", "board-account");
    name.setAttribute("role", "rowheader");
    const label = el("span", "account-label", names.get(a.id) ?? a.label);
    label.title = [a.label, a.plan, a.read_only ? "read-only" : null].filter(Boolean).join(", ");
    const who = el("div", "board-who");
    who.append(label);
    if (updateFailed(a) || a.as_of_ms) {
      const age = a.as_of_ms ? ago(a.as_of_ms, true) : "no reading";
      const sub = el(
        "span",
        updateFailed(a) ? `board-age ${a.status}` : "board-age",
        updateFailed(a) ? `${a.status === "rate_limited" ? "throttled" : "failed"}, ${age}` : age,
      );
      sub.title = updateFailed(a) ? failureDetail(a) : `Read at ${clock(a.as_of_ms!)}`;
      who.append(sub);
    }
    name.append(statusDot(a), who);
    row.append(name);

    if (a.status !== "ok" && a.windows.length === 0) {
      const problem = el("div", `board-problem ${a.status}`, a.message ?? STATUS_TEXT[a.status]);
      problem.setAttribute("role", "cell");
      row.append(problem);
    } else {
      for (const column of columns) {
        const cell = el("div", "board-cell");
        cell.setAttribute("role", "cell");
        const w = a.windows.find((candidate) => candidate.key === column.key);
        if (w) {
          cell.append(renderGauge(w), renderReading(w, ""));
        } else {
          cell.append(el("span", "reset", "no limit"));
        }
        row.append(cell);
      }
    }
    board.append(row);
  }
  return board;
}

/* ---------- Cards view: one block per account ---------- */

function renderRemove(a: AccountUsage): HTMLElement {
  const remove = el("button", "link danger", "Remove account");
  remove.type = "button";
  // Two clicks instead of a confirm() dialog, which blocks the webview.
  remove.addEventListener("click", async () => {
    if (remove.dataset.armed !== "1") {
      remove.dataset.armed = "1";
      remove.textContent = "Click again to remove";
      setTimeout(() => {
        remove.dataset.armed = "";
        remove.textContent = "Remove account";
      }, 3000);
      return;
    }
    try {
      await invoke("remove_account", { id: a.id });
      noticeEl.textContent = `Removed ${a.label}.`;
    } catch (error) {
      noticeEl.textContent = String(error);
    }
    void refresh();
  });
  return remove;
}

function renderCard(a: AccountUsage): HTMLElement {
  const block = el("section", compactCards ? "account compact" : "account");

  const head = el("div", "account-head");
  const title = el("div", "account-title");
  title.append(el("span", "account-label", a.label));
  if (a.plan) title.append(el("span", "tag", a.plan));
  if (a.read_only) {
    const tag = el("span", "tag", "read-only");
    tag.title = "This is Claude Code's own login on this Mac. The app only reads it.";
    title.append(tag);
  }
  // The chip is about the login. A failed check with a working login still
  // reads "connected"; the failure gets its own badge below.
  const loginStatus = updateFailed(a) ? "ok" : a.status;
  const chip = el("span", `chip ${loginStatus}`, STATUS_TEXT[loginStatus]);
  chip.title = STATUS_HELP[loginStatus];
  head.append(title, chip);
  block.append(head);

  const meta = el("div", "account-meta");
  if (a.as_of_ms) {
    const updated = el("span", "updated-ago", `Updated ${ago(a.as_of_ms)}`);
    updated.title = `Read at ${clock(a.as_of_ms)}`;
    meta.append(updated);
  }
  if (updateFailed(a)) {
    const badge = el("span", `badge ${a.status}`, `Update failed: ${failureReason(a)}`);
    badge.title = failureDetail(a);
    meta.append(badge);
  }
  if (compactCards && !a.read_only) {
    const remove = renderRemove(a);
    remove.classList.add("meta-action");
    meta.append(remove);
  }
  if (meta.childElementCount > 0) block.append(meta);

  if (a.message && !updateFailed(a)) block.append(el("p", "message", a.message));

  const shown = a.windows.filter((w) => !(hideUnused && isUnused(w)));
  if (compactCards) {
    // Same three-across layout as the grid, inside the card.
    const strip = el("div", "limit-strip");
    strip.style.setProperty("--limit-columns", String(Math.max(1, shown.length)));
    for (const w of shown) {
      const cell = el("div", "limit-cell");
      cell.append(el("span", "limit-label", w.label), renderGauge(w), renderReading(w, ""));
      strip.append(cell);
    }
    if (shown.length > 0) block.append(strip);
  } else {
    for (const w of shown) {
      const row = el("div", "limit");
      row.append(el("span", "limit-label", w.label), renderGauge(w), renderReading(w, "resets in "));
      block.append(row);
    }
  }
  const hidden = a.windows.length - shown.length;
  if (hidden > 0) {
    block.append(el("p", "message", `${hidden} unused limit${hidden === 1 ? "" : "s"} hidden.`));
  } else if (a.status === "ok" && a.windows.length === 0) {
    block.append(el("p", "message", "No limits reported for this account."));
  }

  if (!a.read_only && !compactCards) block.append(renderRemove(a));
  return block;
}

/* ---------- Wiring ---------- */

function applyTheme() {
  document.documentElement.dataset.theme = theme;
  themeSelect.value = theme;
}

function render() {
  gridBtn.setAttribute("aria-pressed", String(view === "grid"));
  cardsBtn.setAttribute("aria-pressed", String(view === "cards"));
  hideUnusedInput.checked = hideUnused;
  compactInput.checked = compactCards;
  compactWrap.hidden = view !== "cards";
  if (lastAccounts.length === 0) return;
  accountsEl.replaceChildren(
    ...(view === "grid" ? [renderGrid(lastAccounts)] : lastAccounts.map(renderCard)),
  );
  // Repainted every minute along with the rest, so "X min ago" stays true.
  updatedEl.textContent = describeFreshness(lastAccounts);
}

function describeFreshness(accounts: AccountUsage[]): string {
  const readings = accounts.map((a) => a.as_of_ms).filter((ms): ms is number => ms !== null);
  if (readings.length === 0) return "";
  const oldest = Math.min(...readings);
  const parts = [`Oldest reading ${ago(oldest)}`];
  const next = accounts.map((a) => a.next_check_ms).filter((ms): ms is number => ms !== null);
  if (next.length > 0) parts.push(`next check ${clock(Math.min(...next))}`);
  return parts.join(", ");
}

async function refresh(force = false) {
  refreshBtn.disabled = true;
  try {
    lastAccounts = await invoke<AccountUsage[]>("list_usage", { force });
    render();
    updatedEl.title = "Readings are cached. Anthropic throttles this endpoint, so checks are spaced out.";
  } catch (error) {
    accountsEl.replaceChildren(el("p", "message", `Could not load usage: ${String(error)}`));
  } finally {
    refreshBtn.disabled = false;
  }
}

function applyAppSettings(settings: AppSettings) {
  pinBtn.setAttribute("aria-pressed", String(settings.pinned));
  pinBtn.title = settings.pinned
    ? "Pinned: stays on top, on every Space. Click to unpin."
    : "Pin on top of other windows, on every Space";
  dockInput.checked = settings.show_dock_icon;
}

function setView(next: View) {
  view = next;
  saveSetting("view", next);
  render();
}

function setLoggingIn(active: boolean) {
  addBtn.disabled = active;
  emailInput.disabled = active;
  emailToggle.disabled = active;
  cancelBtn.hidden = !active;
}

addForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  setLoggingIn(true);
  noticeEl.textContent = "Finish signing in in your browser. Waiting up to 3 minutes.";
  try {
    const hint = emailInput.hidden ? "" : emailInput.value.trim();
    const label = await invoke<string>("add_account", { emailHint: hint || null });
    noticeEl.textContent = `Added ${label}.`;
    emailInput.value = "";
    await refresh();
  } catch (error) {
    noticeEl.textContent = String(error);
  } finally {
    setLoggingIn(false);
  }
});

emailToggle.addEventListener("click", () => {
  emailInput.hidden = !emailInput.hidden;
  emailToggle.textContent = emailInput.hidden ? "Pre-fill an email" : "Skip the email";
  if (!emailInput.hidden) emailInput.focus();
});

hideUnusedInput.addEventListener("change", () => {
  hideUnused = hideUnusedInput.checked;
  saveSetting("hideUnused", hideUnused ? "1" : "0");
  render();
});

gridBtn.addEventListener("click", () => setView("grid"));
cardsBtn.addEventListener("click", () => setView("cards"));
compactInput.addEventListener("change", () => {
  compactCards = compactInput.checked;
  saveSetting("compactCards", compactCards ? "1" : "0");
  render();
});
cancelBtn.addEventListener("click", () => void invoke("cancel_login"));
refreshBtn.addEventListener("click", () => void refresh(true));
themeSelect.addEventListener("change", () => {
  theme = themeSelect.value === "instrument" ? "instrument" : "warm";
  saveSetting("theme", theme);
  applyTheme();
});
pinBtn.addEventListener("click", async () => {
  const pinned = pinBtn.getAttribute("aria-pressed") !== "true";
  applyAppSettings(await invoke<AppSettings>("set_pinned", { pinned }));
});
dockInput.addEventListener("change", async () => {
  applyAppSettings(await invoke<AppSettings>("set_show_dock_icon", { show: dockInput.checked }));
});

// The Rust side checks usage in the background (page timers stall while the
// window is hidden) and pushes every new set of readings here.
void listen<AccountUsage[]>("usage-updated", (event) => {
  lastAccounts = event.payload;
  render();
}).catch(() => {});
// Frameless window: no close button, so Esc hides it (the menubar item toggles it back).
window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") void invoke("hide_window");
});
$<HTMLButtonElement>("#quit").addEventListener("click", () => void invoke("quit_app"));
void invoke<AppSettings | null>("get_settings")
  .then((settings) => settings && applyAppSettings(settings))
  .catch(() => {});
// Countdown text goes stale between polls; repaint it without refetching.
setInterval(render, 60 * 1000);

applyTheme();
render();
void refresh();
