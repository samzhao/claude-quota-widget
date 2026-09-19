import { invoke } from "@tauri-apps/api/core";

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

type View = "grid" | "detail";
type Theme = "warm" | "instrument";
type Level = "normal" | "warning" | "critical";

// The Rust cache decides whether a check really hits the network (usage
// endpoint budget is tight), so this timer only has to be "often enough".
const POLL_MS = 5 * 60 * 1000;
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
const detailBtn = $<HTMLButtonElement>("#view-detail");
const addForm = $<HTMLFormElement>("#add-form");
const emailInput = $<HTMLInputElement>("#email");
const emailToggle = $<HTMLButtonElement>("#email-toggle");
const addBtn = $<HTMLButtonElement>("#add");
const cancelBtn = $<HTMLButtonElement>("#cancel");
const hideUnusedInput = $<HTMLInputElement>("#hide-unused");
const themeSelect = $<HTMLSelectElement>("#theme");
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

let view: View = loadSetting("view", "grid") === "detail" ? "detail" : "grid";
let hideUnused = loadSetting("hideUnused", "1") === "1";
let theme: Theme = loadSetting("theme", "warm") === "instrument" ? "instrument" : "warm";
let lastAccounts: AccountUsage[] = [];

function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
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

/** A limit nobody has touched yet: 0% and no reset clock running. */
function isUnused(w: UsageWindow): boolean {
  return w.utilization === 0 && !w.resets_at;
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
  const since = a.status !== "ok" && a.as_of_ms ? ` Reading from ${clock(a.as_of_ms)}.` : "";
  dot.title = `${STATUS_TEXT[a.status]}: ${STATUS_HELP[a.status]}${since}`;
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

function renderGrid(accounts: AccountUsage[]): HTMLElement {
  const columns = gridColumns(accounts);
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
    const label = el("span", "account-label", a.label);
    label.title = [a.label, a.plan, a.read_only ? "read-only" : null].filter(Boolean).join(", ");
    name.append(statusDot(a), label);
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

/* ---------- Detail view: one block per account ---------- */

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

function renderDetail(a: AccountUsage): HTMLElement {
  const block = el("section", "account");

  const head = el("div", "account-head");
  const title = el("div", "account-title");
  title.append(el("span", "account-label", a.label));
  if (a.plan) title.append(el("span", "tag", a.plan));
  if (a.read_only) {
    const tag = el("span", "tag", "read-only");
    tag.title = "This is Claude Code's own login on this Mac. The app only reads it.";
    title.append(tag);
  }
  const chip = el("span", `chip ${a.status}`, STATUS_TEXT[a.status]);
  chip.title = STATUS_HELP[a.status];
  head.append(title, chip);
  block.append(head);

  if (a.message) {
    const stale = a.status !== "ok" && a.windows.length > 0;
    const since = stale && a.as_of_ms ? ` Showing the reading from ${clock(a.as_of_ms)}.` : "";
    block.append(el("p", "message", `${a.message}${since}`));
  }

  const shown = a.windows.filter((w) => !(hideUnused && isUnused(w)));
  for (const w of shown) {
    const row = el("div", "limit");
    row.append(el("span", "limit-label", w.label), renderGauge(w), renderReading(w, "resets in "));
    block.append(row);
  }
  const hidden = a.windows.length - shown.length;
  if (hidden > 0) {
    block.append(el("p", "message", `${hidden} unused limit${hidden === 1 ? "" : "s"} hidden.`));
  } else if (a.status === "ok" && a.windows.length === 0) {
    block.append(el("p", "message", "No limits reported for this account."));
  }

  if (!a.read_only) block.append(renderRemove(a));
  return block;
}

/* ---------- Wiring ---------- */

function applyTheme() {
  document.documentElement.dataset.theme = theme;
  themeSelect.value = theme;
}

function render() {
  gridBtn.setAttribute("aria-pressed", String(view === "grid"));
  detailBtn.setAttribute("aria-pressed", String(view === "detail"));
  hideUnusedInput.checked = hideUnused;
  if (lastAccounts.length === 0) return;
  accountsEl.replaceChildren(
    ...(view === "grid" ? [renderGrid(lastAccounts)] : lastAccounts.map(renderDetail)),
  );
}

function describeFreshness(accounts: AccountUsage[]): string {
  const readings = accounts.map((a) => a.as_of_ms).filter((ms): ms is number => ms !== null);
  if (readings.length === 0) return "";
  const parts = [`As of ${clock(Math.min(...readings))}`];
  const next = accounts.map((a) => a.next_check_ms).filter((ms): ms is number => ms !== null);
  if (next.length > 0) parts.push(`next check ${clock(Math.min(...next))}`);
  return parts.join(", ");
}

async function refresh(force = false) {
  refreshBtn.disabled = true;
  try {
    lastAccounts = await invoke<AccountUsage[]>("list_usage", { force });
    render();
    updatedEl.textContent = describeFreshness(lastAccounts);
    updatedEl.title = "Readings are cached. Anthropic throttles this endpoint, so checks are spaced out.";
  } catch (error) {
    accountsEl.replaceChildren(el("p", "message", `Could not load usage: ${String(error)}`));
  } finally {
    refreshBtn.disabled = false;
  }
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
detailBtn.addEventListener("click", () => setView("detail"));
cancelBtn.addEventListener("click", () => void invoke("cancel_login"));
refreshBtn.addEventListener("click", () => void refresh(true));
themeSelect.addEventListener("change", () => {
  theme = themeSelect.value === "instrument" ? "instrument" : "warm";
  saveSetting("theme", theme);
  applyTheme();
});
setInterval(() => void refresh(), POLL_MS);
// Countdown text goes stale between polls; repaint it without refetching.
setInterval(render, 60 * 1000);

applyTheme();
render();
void refresh();
