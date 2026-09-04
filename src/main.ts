import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

const WIN_W = 190;
const MAX_VISIBLE = 5;

document.addEventListener("contextmenu", (e) => e.preventDefault());

interface Win {
  remaining: number;
  resetsAt: string | null;
}

interface Provider {
  present: boolean;
  loggedIn: boolean;
  fiveHour: Win | null;
  sevenDay: Win | null;
  todayTokens: number | null;
  todayCost: number | null;
  stale: boolean;
  error: string | null;
  blocked: boolean;
  plan: string | null;
}

interface AppState {
  claude: Provider;
  codex: Provider;
  lastSync: string | null;
}

interface MonitorSettings {
  enableClaude: boolean;
  enableCodex: boolean;
  opacity: number;
}

interface Chip {
  id: string;
  tool: string;
  project: string;
  folder: string;
  state: string;
  since: string;
  trigger: string;
}

function $(id: string): HTMLElement | null {
  return document.getElementById(id);
}

function setText(id: string, text: string) {
  const el = $(id);
  if (el) el.textContent = text;
}

function fmtPct(n: number): string {
  return Math.round(n).toString();
}

function fmtTokens(n: number | null): string {
  if (n == null) return "--";
  if (n >= 1e8) return (n / 1e8).toFixed(2) + "e8";
  if (n >= 1e4) return (n / 1e4).toFixed(1) + "w";
  return n.toString();
}

function fmtCost(c: number | null): string {
  if (c == null) return "$--";
  return "$" + (c >= 10 ? c.toFixed(0) : c.toFixed(2));
}

function pad(n: number): string {
  return n.toString().padStart(2, "0");
}

function fmtResetShort(iso: string | null): string {
  if (!iso) return "reset --:--";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "reset --:--";
  return `reset ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function fmtResetDate(iso: string | null): string {
  if (!iso) return "--";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "--";
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return sameDay ? hm : `${d.getMonth() + 1}/${d.getDate()} ${hm}`;
}

function fmtSync(iso: string | null): string {
  if (!iso) return "--:--";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "--:--";
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

let spinTimer: number | undefined;

function startSpin() {
  $("sync-ico")?.classList.add("spinning");
  if (spinTimer) clearTimeout(spinTimer);
  spinTimer = window.setTimeout(stopSpin, 5000);
}

function stopSpin() {
  $("sync-ico")?.classList.remove("spinning");
  if (spinTimer) {
    clearTimeout(spinTimer);
    spinTimer = undefined;
  }
}

function barClass(remaining: number): string {
  if (remaining < 15) return "danger";
  if (remaining < 40) return "warn";
  return "ok";
}

function planLabel(plan: string | null): string {
  const base = "额度已用尽";
  if (!plan) return base;
  const m: Record<string, string> = {
    free: "免费档",
    plus: "Plus",
    pro: "Pro",
    team: "Team",
  };
  return `${m[plan] ?? plan} · ${base}`;
}

function applyOpacity(o: number) {
  const v = Math.max(0.3, Math.min(1, o || 1));
  const root = document.getElementById("root");
  if (root) root.style.opacity = String(v);
}

let monitorSettings: MonitorSettings = {
  enableClaude: true,
  enableCodex: true,
  opacity: 1,
};
let latestState: AppState | null = null;

function applySettings(settings: MonitorSettings) {
  monitorSettings = settings;
  applyOpacity(settings.opacity);
  if (latestState) render(latestState);
}

function renderProvider(key: "claude" | "codex", p: Provider, enabled: boolean) {
  const zone = $(`zone-${key}`);
  if (!zone) return;

  if (!enabled || !p.present) {
    zone.classList.add("hidden");
    return;
  }

  zone.classList.remove("hidden");
  zone.classList.toggle("stale", p.stale);

  const note = $(`${key}-note`);
  if (!p.loggedIn) {
    setText(`${key}-pct`, "--");
    const bar = $(`${key}-bar`);
    if (bar) bar.style.width = "0%";
    setText(`${key}-5h`, "reset --:--");
    setText(`${key}-7d`, "-- - --");
    if (note) note.textContent = p.error ?? "not logged in";
    return;
  }

  // 接口明确「不可用」（额度耗尽/未订阅）：显示明确状态，而不是一个乐观百分比。
  if (p.blocked) {
    setText(`${key}-pct`, "0");
    const bar = $(`${key}-bar`);
    if (bar) {
      bar.style.width = "2%";
      bar.className = "fill danger";
    }
    const five = p.fiveHour;
    const seven = p.sevenDay;
    const limiting = five ?? seven;
    setText(`${key}-5h`, five ? fmtResetShort(five.resetsAt) : "reset --:--");
    setText(`${key}-7d`, "额度耗尽");
    if (note) {
      note.textContent =
        planLabel(p.plan) +
        (limiting?.resetsAt ? ` · ${fmtResetDate(limiting.resetsAt)} 恢复` : "");
    }
    return;
  }

  if (note) {
    note.textContent = p.stale ? p.error ?? "sync failed" : p.plan === "free" ? "免费档" : "";
  }

  // free 档的额度窗口实际是月度滚动，不是 5 小时；标签跟着套餐类型走，
  // 避免继续显示 "5h" 误导用户以为是付费档的 5 小时窗口。
  if (key === "codex") {
    const label = $(`${key}-5h-label`);
    if (label) label.textContent = p.plan === "free" ? "本月" : "5h";
  }

  // 每次都完整覆盖所有字段：数据缺失时回退占位符，避免上一次渲染（如 blocked 的
  // “额度耗尽”）的残影和新值并存造成自相矛盾的显示。
  const five = p.fiveHour;
  const seven = p.sevenDay;
  // 某些套餐当前只返回 7 天窗口；顶部摘要展示任一可用主窗口，具体类型仍由下方
  // 5h / 7d 行准确标注。
  const summary = five ?? seven;
  const bar = $(`${key}-bar`);
  if (summary) {
    setText(`${key}-pct`, fmtPct(summary.remaining));
    if (bar) {
      bar.style.width = `${Math.max(2, Math.min(100, summary.remaining))}%`;
      bar.className = `fill ${barClass(summary.remaining)}`;
    }
  } else {
    setText(`${key}-pct`, "--");
    if (bar) {
      bar.style.width = "0%";
      bar.className = "fill";
    }
  }

  // free 档窗口是月度滚动，只显示时:分会把"一个月后"误显示成"今天"——带上日期。
  setText(
    `${key}-5h`,
    five
      ? p.plan === "free"
        ? `reset ${fmtResetDate(five.resetsAt)}`
        : fmtResetShort(five.resetsAt)
      : "reset --:--",
  );

  setText(
    `${key}-7d`,
    seven ? `${fmtPct(seven.remaining)}% - ${fmtResetDate(seven.resetsAt)}` : "-- - --",
  );
}

function render(state: AppState) {
  latestState = state;
  const claudeVisible = monitorSettings.enableClaude && state.claude.present;
  const codexVisible = monitorSettings.enableCodex && state.codex.present;

  renderProvider("claude", state.claude, monitorSettings.enableClaude);
  renderProvider("codex", state.codex, monitorSettings.enableCodex);

  setText("sync-time", fmtSync(state.lastSync));
  const sync = $("sync");
  if (sync) {
    sync.classList.toggle(
      "warn",
      (claudeVisible && state.claude.stale) || (codexVisible && state.codex.stale),
    );
  }

  setTodayLine("claude-today", state.claude);
  setTodayLine("codex-today", state.codex);

  const divider = document.querySelector(".divider") as HTMLElement | null;
  if (divider) {
    divider.style.display = claudeVisible && codexVisible ? "" : "none";
  }

  resizeWindow();
  stopSpin();
}

function setTodayLine(id: string, p: Provider) {
  const el = $(id);
  if (!el) return;
  el.innerHTML = `<span class="amt">${fmtCost(p.todayCost)}</span> - <span class="amt">${fmtTokens(p.todayTokens)}</span> tok`;
}

function stateLabel(s: string): string {
  if (s === "done") return "done";
  if (s === "waiting") return "waiting";
  return "error";
}

function resizeWindow() {
  requestAnimationFrame(() => {
    const root = document.getElementById("root");
    if (!root) return;
    const h = Math.ceil(root.getBoundingClientRect().height);
    getCurrentWindow()
      .setSize(new LogicalSize(WIN_W, h))
      .catch(() => {});
  });
}

function attachClick(el: HTMLElement, c: Chip) {
  el.addEventListener("click", (e) => {
    e.stopPropagation();
    el.classList.add("fading");
    window.setTimeout(() => {
      invoke("dismiss_chip", { id: c.id, trigger: c.trigger }).catch(() => {});
    }, 140);
  });
}

function renderChips(chips: Chip[]) {
  const stack = $("stack");
  if (!stack) return;
  const root = $("root");
  stack.innerHTML = "";

  if (!chips.length) {
    stack.classList.add("empty");
    root?.classList.remove("has-bars");
    resizeWindow();
    return;
  }

  stack.classList.remove("empty");
  root?.classList.add("has-bars");

  for (const c of chips.slice(0, MAX_VISIBLE)) {
    const el = document.createElement("div");
    el.className = `sigbar ${c.state}`;
    const toolName = c.tool === "claude" ? "Claude" : "Codex";
    const parts = [toolName];
    if (c.folder && c.folder !== c.project) parts.push(c.folder);
    parts.push(c.project, stateLabel(c.state));
    el.title = parts.join(" - ");

    const ico = document.createElement("span");
    ico.className = `tico ${c.tool}`;
    ico.textContent = c.tool === "claude" ? "C" : "X";

    const txt = document.createElement("span");
    txt.className = "ptxt";
    txt.textContent = c.project;

    const st = document.createElement("span");
    st.className = "st";
    st.textContent = stateLabel(c.state);

    el.appendChild(ico);
    el.appendChild(txt);
    el.appendChild(st);
    attachClick(el, c);
    stack.appendChild(el);
  }

  resizeWindow();
}

window.addEventListener("DOMContentLoaded", async () => {
  $("close")?.addEventListener("click", (e) => {
    e.stopPropagation();
    getCurrentWindow().close();
  });

  const syncEl = $("sync");
  syncEl?.addEventListener("mousedown", (e) => e.stopPropagation());
  syncEl?.addEventListener("click", async (e) => {
    e.stopPropagation();
    startSpin();
    try {
      await invoke("refresh_now");
    } catch (_) {
      stopSpin();
    }
  });

  await listen<AppState>("state-updated", (e) => render(e.payload));
  await listen<Chip[]>("chips-updated", (e) => renderChips(e.payload));
  await listen<number>("apply-opacity", (e) => applyOpacity(e.payload));
  await listen<MonitorSettings>("settings-updated", (e) => applySettings(e.payload));

  try {
    const s = await invoke<MonitorSettings>("get_settings");
    applySettings(s);
  } catch (_) {
    /* keep default opacity */
  }

  try {
    const s = await invoke<AppState>("get_state");
    render(s);
  } catch (_) {
    /* collector will emit later */
  }

  try {
    const chips = await invoke<Chip[]>("get_chips");
    renderChips(chips);
  } catch (_) {
    /* collector will emit later */
  }
});
