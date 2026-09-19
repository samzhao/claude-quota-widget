import { invoke } from "@tauri-apps/api/core";

type UsageWindow = {
  key: string;
  label: string;
  utilization: number;
  resets_at: string | null;
};

type AccountUsage = {
  id: string;
  label: string;
  plan: string | null;
  read_only: boolean;
  status: "ok" | "needs_login" | "rate_limited" | "error";
  message: string | null;
  windows: UsageWindow[];
  fetched_at_ms: number;
};

const POLL_MS = 5 * 60 * 1000;
const STATUS_TEXT: Record<AccountUsage["status"], string> = {
  ok: "ok",
  needs_login: "needs login",
  rate_limited: "throttled",
  error: "error",
};

const accountsEl = document.querySelector<HTMLElement>("#accounts")!;
const updatedEl = document.querySelector<HTMLElement>("#updated")!;
const refreshBtn = document.querySelector<HTMLButtonElement>("#refresh")!;
const addForm = document.querySelector<HTMLFormElement>("#add-form")!;
const emailInput = document.querySelector<HTMLInputElement>("#email")!;
const addBtn = document.querySelector<HTMLButtonElement>("#add")!;
const cancelBtn = document.querySelector<HTMLButtonElement>("#cancel")!;
const noticeEl = document.querySelector<HTMLElement>("#notice")!;

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

function resetsIn(iso: string | null): string {
  if (!iso) return "not started";
  const ms = new Date(iso).getTime() - Date.now();
  if (Number.isNaN(ms)) return "";
  if (ms <= 0) return "resetting";
  const minutes = Math.round(ms / 60000);
  if (minutes < 60) return `resets in ${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `resets in ${hours}h ${minutes % 60}m`;
  return `resets in ${Math.floor(hours / 24)}d ${hours % 24}h`;
}

function level(utilization: number): string {
  if (utilization >= 90) return "critical";
  if (utilization >= 70) return "warn";
  return "fine";
}

function renderWindow(w: UsageWindow): HTMLElement {
  const row = el("div", "window");
  const pct = Math.max(0, Math.min(100, w.utilization));

  const head = el("div", "window-head");
  head.append(el("span", "window-label", w.label));
  head.append(el("span", "window-pct", `${Math.round(w.utilization)}%`));

  const track = el("div", "track");
  track.setAttribute("role", "progressbar");
  track.setAttribute("aria-valuemin", "0");
  track.setAttribute("aria-valuemax", "100");
  track.setAttribute("aria-valuenow", String(Math.round(pct)));
  track.setAttribute("aria-label", `${w.label} usage`);
  const fill = el("div", `fill ${level(pct)}`);
  fill.style.width = `${pct}%`;
  track.append(fill);

  row.append(head, track, el("div", "muted small", resetsIn(w.resets_at)));
  return row;
}

function renderAccount(a: AccountUsage): HTMLElement {
  const card = el("section", "account");

  const head = el("div", "account-head");
  const title = el("div", "account-title");
  title.append(el("span", "account-label", a.label));
  if (a.plan) title.append(el("span", "tag", a.plan));
  if (a.read_only) title.append(el("span", "tag", "read-only"));
  head.append(title, el("span", `chip ${a.status}`, STATUS_TEXT[a.status]));
  card.append(head);

  if (a.message) card.append(el("p", "message", a.message));
  for (const w of a.windows) card.append(renderWindow(w));
  if (a.status === "ok" && a.windows.length === 0) {
    card.append(el("p", "message", "No usage windows reported."));
  }

  if (!a.read_only) {
    const remove = el("button", "link", "Remove");
    remove.type = "button";
    // Two clicks instead of a confirm() dialog, which blocks the webview.
    remove.addEventListener("click", async () => {
      if (remove.dataset.armed !== "1") {
        remove.dataset.armed = "1";
        remove.textContent = "Click again to remove";
        setTimeout(() => {
          remove.dataset.armed = "";
          remove.textContent = "Remove";
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
    card.append(remove);
  }
  return card;
}

async function refresh() {
  refreshBtn.disabled = true;
  try {
    const accounts = await invoke<AccountUsage[]>("list_usage");
    accountsEl.replaceChildren(...accounts.map(renderAccount));
    updatedEl.textContent = `updated ${new Date().toLocaleTimeString([], {
      hour: "numeric",
      minute: "2-digit",
    })}`;
  } catch (error) {
    accountsEl.replaceChildren(el("p", "message", `Could not load usage: ${String(error)}`));
  } finally {
    refreshBtn.disabled = false;
  }
}

function setLoggingIn(active: boolean) {
  addBtn.disabled = active;
  emailInput.disabled = active;
  cancelBtn.hidden = !active;
}

addForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  setLoggingIn(true);
  noticeEl.textContent = "Finish the login in your browser. Waiting up to 3 minutes.";
  try {
    const label = await invoke<string>("add_account", {
      emailHint: emailInput.value.trim() || null,
    });
    noticeEl.textContent = `Added ${label}.`;
    emailInput.value = "";
    await refresh();
  } catch (error) {
    noticeEl.textContent = String(error);
  } finally {
    setLoggingIn(false);
  }
});

cancelBtn.addEventListener("click", () => void invoke("cancel_login"));
refreshBtn.addEventListener("click", () => void refresh());
setInterval(() => void refresh(), POLL_MS);
void refresh();
