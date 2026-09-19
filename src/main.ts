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
  tag: string | null;
  hidden: boolean;
  machines: MachineBadge[];
  plan: string | null;
  read_only: boolean;
  status: "ok" | "needs_login" | "rate_limited" | "error";
  message: string | null;
  windows: UsageWindow[];
  as_of_ms: number | null;
  next_check_ms: number | null;
};

type MachineBadge = { machine: string; profile: string; running: number };
type MachineStatus = {
  name: string;
  ssh: string;
  sightings: { email: string; profile: string; running: number }[];
  problem: string | null;
  checked_at_ms: number | null;
};

type View = "grid" | "cards";
type Theme = "warm" | "instrument";
type AppSettings = {
  pinned: boolean;
  show_dock_icon: boolean;
  width: number;
  manual_height: number | null;
};
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
const fitBtn = $<HTMLButtonElement>("#fit");
const sortSelect = $<HTMLSelectElement>("#sort");
const sortDirBtn = $<HTMLButtonElement>("#sort-dir");
const machinesToggle = $<HTMLButtonElement>("#machines-toggle");
const machinesPanel = $("#machines-panel");
const machinesList = $("#machines-list");
const machineForm = $<HTMLFormElement>("#machine-form");
const machineName = $<HTMLInputElement>("#machine-name");
const machineSsh = $<HTMLInputElement>("#machine-ssh");
const machineNotice = $("#machine-notice");
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

/* ---------- Sorting and hiding ---------- */

// A sort is a chain of steps, applied in order; later steps break ties.
// Step keys: "usable" (the combined default below), "added", "room", "name",
// or a limit's key (session, weekly_all, weekly_scoped:…).
type SortStep = { key: string; dir: "asc" | "desc" };
const DEFAULT_SORT: SortStep[] = [{ key: "usable", dir: "asc" }];
const FULL = 100;

function loadSortChain(): SortStep[] {
  try {
    const parsed: unknown = JSON.parse(loadSetting("sortChain", ""));
    if (Array.isArray(parsed) && parsed.length > 0) {
      return parsed
        .filter((step): step is SortStep => typeof step?.key === "string")
        .map((step) => ({ key: step.key, dir: step.dir === "desc" ? "desc" : "asc" }));
    }
  } catch {
    /* fall through to the default */
  }
  return DEFAULT_SORT;
}
let sortChain: SortStep[] = loadSortChain();

function limitValue(a: AccountUsage, key: string): number | null {
  return a.windows.find((w) => w.key === key)?.utilization ?? null;
}

/**
 * "Usable first": can I use this account right now?
 * An account with a full 5-hour or weekly limit is blocked outright; one with
 * only a model limit full still works for other models, so it ranks between.
 * Within each group the order is 5-hour, then all models, then each model
 * limit, least used first.
 */
function blockedRank(a: AccountUsage): number {
  const full = (key: string) => (limitValue(a, key) ?? 0) >= FULL;
  if (full("session") || full("weekly_all")) return 2;
  return a.windows.some((w) => w.utilization >= FULL) ? 1 : 0;
}

function expandChain(columns: { key: string }[]): SortStep[] {
  return sortChain.flatMap((step): SortStep[] =>
    step.key === "usable"
      ? [{ key: "blocked", dir: step.dir }, ...columns.map((c) => ({ key: c.key, dir: step.dir }))]
      : [step],
  );
}

function stepValue(a: AccountUsage, key: string): number | string | null {
  if (key === "name") return a.label.toLowerCase();
  if (a.windows.length === 0) return null;
  if (key === "blocked") return blockedRank(a);
  if (key === "room") return Math.max(...a.windows.map((w) => w.utilization));
  return limitValue(a, key);
}

/** Accounts with nothing to compare (no reading, or no such limit) always sink to the bottom. */
function sortAccounts(list: AccountUsage[], columns: { key: string }[]): AccountUsage[] {
  const steps = expandChain(columns);
  const position = new Map(list.map((a, index) => [a.id, index]));
  return [...list].sort((x, y) => {
    for (const step of steps) {
      if (step.key === "added") {
        const diff = position.get(x.id)! - position.get(y.id)!;
        if (diff !== 0) return step.dir === "asc" ? diff : -diff;
        continue;
      }
      const a = stepValue(x, step.key);
      const b = stepValue(y, step.key);
      if (a === null || b === null) {
        if (a !== b) return a === null ? 1 : -1;
        continue;
      }
      const diff =
        typeof a === "string" || typeof b === "string"
          ? String(a).localeCompare(String(b))
          : a - b;
      if (diff !== 0) return step.dir === "asc" ? diff : -diff;
    }
    return position.get(x.id)! - position.get(y.id)!;
  });
}

function setSortChain(chain: SortStep[]) {
  sortChain = chain.length > 0 ? chain : DEFAULT_SORT;
  saveSetting("sortChain", JSON.stringify(sortChain));
  render();
}

/**
 * Grid headers. Click: sort by that column alone (low to high, then high to
 * low, then back to the default). Shift-click: add it as a further tie-breaker,
 * or flip it if it is already in the chain.
 */
function onHeaderClick(key: string, extend: boolean) {
  const at = sortChain.findIndex((step) => step.key === key);
  if (extend) {
    const base = sortChain.filter((step) => step.key !== "usable" && step.key !== "added");
    if (at >= 0 && base.length === sortChain.length) {
      setSortChain(sortChain.map((step, i) => (i === at ? { key, dir: step.dir === "asc" ? "desc" : "asc" } : step)));
    } else {
      setSortChain([...base, { key, dir: "asc" }]);
    }
    return;
  }
  const alone = sortChain.length === 1 && at === 0;
  if (!alone) setSortChain([{ key, dir: "asc" }]);
  else if (sortChain[0].dir === "asc") setSortChain([{ key, dir: "desc" }]);
  else setSortChain(DEFAULT_SORT);
}

/** Where a column sits in the effective order, for the header's marker. */
function headerMark(key: string, columns: { key: string }[]): { order: number; dir: "asc" | "desc" } | null {
  const steps = expandChain(columns).filter((step) => step.key !== "blocked");
  const index = steps.findIndex((step) => step.key === key);
  return index < 0 ? null : { order: steps.length > 1 ? index + 1 : 0, dir: steps[index].dir };
}

function syncSortControls(columns: { key: string; label: string }[]) {
  const labelOf = (key: string) =>
    ({ usable: "Usable first", added: "Order added", room: "Most room left", name: "Name" })[key] ??
    columns.find((c) => c.key === key)?.label ??
    key;
  const options: [string, string][] = [
    ["usable", "Usable first"],
    ["room", "Most room left"],
    ...columns.map((c): [string, string] => [c.key, c.label]),
    ["name", "Name"],
    ["added", "Order added"],
  ];
  const custom = sortChain.length > 1;
  if (custom) options.unshift(["custom", sortChain.map((step) => labelOf(step.key)).join(", then ")]);
  sortSelect.replaceChildren(
    ...options.map(([key, label]) => {
      const option = el("option", undefined, label);
      option.value = key;
      return option;
    }),
  );
  sortSelect.value = custom ? "custom" : sortChain[0].key;
  sortSelect.title =
    sortChain[0].key === "usable"
      ? "Accounts you can use right now come first: 5-hour, then all models, then each model limit, least used first. Full accounts sink."
      : "Shift-click grid headers to sort by several columns";
  const dir = sortChain[0].dir;
  sortDirBtn.textContent = dir === "asc" ? "↑" : "↓";
  sortDirBtn.title = dir === "asc" ? "Least used first. Click to reverse." : "Most used first. Click to reverse.";
}

async function setHidden(a: AccountUsage, hidden: boolean) {
  try {
    await invoke("set_hidden", { id: a.id, hidden });
    noticeEl.textContent = hidden ? `Hid ${a.label}. It stays signed in; find it under Hidden below.` : "";
  } catch (error) {
    noticeEl.textContent = String(error);
  }
  void refresh();
}

function renderHiddenList(hidden: AccountUsage[]): HTMLElement {
  const box = el("div", "hidden-list");
  box.append(el("span", "hidden-title", `Hidden (${hidden.length})`));
  for (const a of hidden) {
    const item = el("span", "hidden-item");
    item.append(el("span", "hidden-name", a.label));
    const show = el("button", "link", "Show");
    show.type = "button";
    show.addEventListener("click", () => void setHidden(a, false));
    item.append(show);
    box.append(item);
  }
  box.title = "Hidden accounts stay signed in, but are not checked, shown or recommended in the menubar.";
  return box;
}

/* ---------- Labels: free text per account, edited in place ---------- */

const LABEL_MAX_CHARS = 32;
// Background pushes re-render the list; that must not yank an input mid-typing.
let editingLabel = false;

function editLabel(anchor: HTMLElement, a: AccountUsage) {
  editingLabel = true;
  const input = el("input", "label-input");
  input.type = "text";
  input.maxLength = LABEL_MAX_CHARS;
  input.value = a.tag ?? "";
  input.placeholder = "label";
  input.spellcheck = false;
  input.setAttribute("aria-label", `Label for ${a.label}`);

  let finished = false;
  const finish = async (save: boolean) => {
    if (finished) return;
    finished = true;
    editingLabel = false;
    if (save && input.value.trim() !== (a.tag ?? "")) {
      try {
        await invoke("set_label", { id: a.id, text: input.value });
      } catch (error) {
        noticeEl.textContent = String(error);
      }
    }
    render();
  };
  input.addEventListener("keydown", (event) => {
    // Esc here cancels the edit; it must not reach the window-level "hide" handler.
    event.stopPropagation();
    if (event.key === "Enter") void finish(true);
    else if (event.key === "Escape") void finish(false);
  });
  input.addEventListener("blur", () => void finish(true));

  anchor.replaceWith(input);
  input.focus();
  input.select();
}

/** The label chip. `offerAdd` shows a quiet "+ label" when there is none yet. */
function renderLabel(a: AccountUsage, offerAdd: boolean): HTMLElement | null {
  if (!a.tag && !offerAdd) return null;
  const chip = el("button", a.tag ? "label-chip" : "label-chip empty", a.tag ?? "+ label");
  chip.type = "button";
  chip.title = a.tag
    ? "Click to edit. Clear the text to remove the label."
    : "Add a label, for example which machine uses this account";
  chip.addEventListener("click", () => editLabel(chip, a));
  return chip;
}

/* ---------- Machines: where else each account is signed in ---------- */

let machines: MachineStatus[] = [];

/** A filled dot means Claude sessions are running there right now. */
function renderMachineBadges(a: AccountUsage): HTMLElement[] {
  return (a.machines ?? []).map((m) => {
    // The default profile is just the machine's name; any other profile adds its
    // folder name, so "studio" and "studio/2" can sit on different accounts.
    const profileName = m.profile === "~/.claude" ? "" : `/${m.profile.split("/").pop()}`;
    const badge = el(
      "span",
      m.running > 0 ? "machine-badge running" : "machine-badge",
      `${m.machine}${profileName}`,
    );
    const sessions =
      m.running > 0
        ? `${m.running} Claude session${m.running === 1 ? "" : "s"} running now`
        : "signed in, nothing running";
    badge.title = `${m.machine}: ${sessions} (${m.profile})`;
    return badge;
  });
}

function renderMachines() {
  machinesToggle.textContent = machines.length > 0 ? `Machines (${machines.length})` : "Machines";
  const known = new Set(lastAccounts.map((a) => a.label.toLowerCase()));
  machinesList.replaceChildren(
    ...machines.map((m) => {
      const row = el("div", "machine-row");
      const head = el("div", "machine-head");
      head.append(el("span", "machine-name", m.name), el("span", "muted", m.ssh));
      const remove = el("button", "link danger", "Remove");
      remove.type = "button";
      remove.addEventListener("click", async () => {
        machines = await invoke<MachineStatus[]>("remove_machine", { name: m.name });
        renderMachines();
      });
      head.append(remove);
      row.append(head);

      let detail: string;
      let problem = false;
      if (m.problem) {
        detail = m.problem;
        problem = true;
      } else if (m.checked_at_ms === null) {
        detail = "Checking…";
      } else if (m.sightings.length === 0) {
        detail = `No signed-in Claude profile found. Checked ${ago(m.checked_at_ms)}.`;
      } else {
        const parts = m.sightings.map((sighting) => {
          const where = sighting.profile === "~/.claude" ? "" : ` (${sighting.profile})`;
          const tracked = known.has(sighting.email) ? "" : ", not in this list";
          const running = sighting.running > 0 ? `, ${sighting.running} running` : "";
          return `${sighting.email}${where}${running}${tracked}`;
        });
        detail = `${parts.join("; ")}. Checked ${ago(m.checked_at_ms)}.`;
      }
      row.append(el("p", problem ? "machine-detail problem" : "machine-detail", detail));
      return row;
    }),
  );
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
  const header = (key: string, label: string, className: string) => {
    const th = el("button", `${className} sort-header`, label);
    th.type = "button";
    th.setAttribute("role", "columnheader");
    const mark = headerMark(key, columns);
    if (mark) {
      th.setAttribute("aria-sort", mark.dir === "asc" ? "ascending" : "descending");
      const arrow = mark.dir === "asc" ? "↑" : "↓";
      th.append(el("span", "sort-arrow", mark.order > 0 ? `${mark.order}${arrow}` : arrow));
    }
    th.title = `Sort by ${label.toLowerCase()}. Shift-click to add it as a tie-breaker.`;
    th.addEventListener("click", (event) => onHeaderClick(key, event.shiftKey));
    return th;
  };
  headRow.append(header("name", "Account", "board-account"));
  for (const column of columns) headRow.append(header(column.key, column.label, "board-cell"));
  board.append(headRow);

  for (const a of accounts) {
    const row = el("div", "board-row");
    row.setAttribute("role", "row");

    const name = el("div", "board-account");
    name.setAttribute("role", "rowheader");
    const label = el("span", "account-label", names.get(a.id) ?? a.label);
    label.title = [a.label, a.plan, a.read_only ? "read-only" : null].filter(Boolean).join(", ");
    const who = el("div", "board-who");
    // Name and label share the top line; the reading age keeps its own line.
    const nameLine = el("div", "board-nameline");
    nameLine.append(label);
    const gridChip = renderLabel(a, false);
    if (gridChip) nameLine.append(gridChip);
    who.append(nameLine);
    const under = el("div", "board-under");
    under.append(...renderMachineBadges(a));
    if (updateFailed(a) || a.as_of_ms) {
      const age = a.as_of_ms ? ago(a.as_of_ms, true) : "no reading";
      const sub = el(
        "span",
        updateFailed(a) ? `board-age ${a.status}` : "board-age",
        updateFailed(a) ? `${a.status === "rate_limited" ? "throttled" : "failed"}, ${age}` : age,
      );
      sub.title = updateFailed(a) ? failureDetail(a) : `Read at ${clock(a.as_of_ms!)}`;
      under.append(sub);
    }
    if (under.childElementCount > 0) who.append(under);
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
  // Label and machines live here rather than beside the name, which needs the room.
  const cardChip = renderLabel(a, true);
  if (cardChip) meta.append(cardChip);
  meta.append(...renderMachineBadges(a));
  if (updateFailed(a)) {
    const badge = el("span", `badge ${a.status}`, `Update failed: ${failureReason(a)}`);
    badge.title = failureDetail(a);
    meta.append(badge);
  }
  const hide = el("button", "link", "Hide");
  hide.type = "button";
  hide.title = "Hide this account. It stays signed in and can be shown again from the Hidden list.";
  hide.addEventListener("click", () => void setHidden(a, true));
  const actions = el("span", "meta-action");
  actions.append(hide);
  if (compactCards && !a.read_only) actions.append(renderRemove(a));
  meta.append(actions);
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
  if (lastAccounts.length === 0 || editingLabel) return;
  const hiddenAccounts = lastAccounts.filter((a) => a.hidden);
  const shown = lastAccounts.filter((a) => !a.hidden);
  const sortColumns = gridColumns(shown);
  syncSortControls(sortColumns);
  const sorted = sortAccounts(shown, sortColumns);
  const parts: HTMLElement[] =
    sorted.length === 0
      ? [el("p", "message", "Every account is hidden.")]
      : view === "grid"
        ? [renderGrid(sorted)]
        : sorted.map(renderCard);
  if (hiddenAccounts.length > 0) parts.push(renderHiddenList(hiddenAccounts));
  accountsEl.replaceChildren(...parts);
  renderMachines();
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
  fitBtn.hidden = settings.manual_height === null;
}

/**
 * The window's height follows the content. Report the natural height whenever
 * it changes (accounts added, view or theme switched, width reflow); the Rust
 * side caps it to the screen and ignores it after a manual resize.
 */
function watchContentHeight() {
  const app = $("#app");
  const content = $("#content");
  let last = 0;
  const report = () => {
    const style = getComputedStyle(app);
    const chrome =
      parseFloat(style.paddingTop) +
      parseFloat(style.paddingBottom) +
      parseFloat(style.borderTopWidth) +
      parseFloat(style.borderBottomWidth);
    const height = Math.ceil(content.getBoundingClientRect().height + chrome);
    if (height === last) return;
    last = height;
    void invoke("content_height", { height }).catch(() => {});
  };
  new ResizeObserver(report).observe(content);
  report();
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
sortSelect.addEventListener("change", () => {
  if (sortSelect.value !== "custom") setSortChain([{ key: sortSelect.value, dir: sortChain[0].dir }]);
});
sortDirBtn.addEventListener("click", () =>
  setSortChain(sortChain.map((step) => ({ key: step.key, dir: step.dir === "asc" ? "desc" : "asc" }))),
);
machinesToggle.addEventListener("click", () => {
  machinesPanel.hidden = !machinesPanel.hidden;
  saveSetting("machinesOpen", machinesPanel.hidden ? "0" : "1");
});
machineForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  machineNotice.textContent = "";
  try {
    machines = await invoke<MachineStatus[]>("add_machine", {
      name: machineName.value,
      ssh: machineSsh.value,
    });
    machineName.value = "";
    machineSsh.value = "";
    renderMachines();
  } catch (error) {
    machineNotice.textContent = String(error);
  }
});
void listen<MachineStatus[]>("machines-updated", (event) => {
  machines = event.payload;
  renderMachines();
}).catch(() => {});
void invoke<MachineStatus[] | null>("list_machines")
  .then((list) => {
    machines = list ?? [];
    renderMachines();
  })
  .catch(() => {});
machinesPanel.hidden = loadSetting("machinesOpen", "0") !== "1";

fitBtn.addEventListener("click", async () => {
  applyAppSettings(await invoke<AppSettings>("fit_to_content"));
});
void listen<AppSettings>("settings-changed", (event) => applyAppSettings(event.payload)).catch(
  () => {},
);
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
watchContentHeight();
void refresh();
