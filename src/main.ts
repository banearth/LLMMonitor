import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

// ===== 与 Rust 端 state.rs 对齐的类型 =====
interface Win {
  remaining: number; // 0..100
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
}
interface AppState {
  claude: Provider;
  codex: Provider;
  lastSync: string | null;
}

// ===== 格式化工具 =====
function fmtPct(n: number): string {
  return Math.round(n).toString();
}

function fmtTokens(n: number | null): string {
  if (n == null) return "--";
  if (n >= 1e8) return (n / 1e8).toFixed(2) + "亿";
  if (n >= 1e4) return (n / 1e4).toFixed(1) + "万";
  return n.toString();
}

function fmtCost(c: number | null): string {
  if (c == null) return "$--";
  return "$" + (c >= 10 ? c.toFixed(0) : c.toFixed(2));
}

function pad(n: number): string {
  return n.toString().padStart(2, "0");
}

/** 5h 重置：始终 "重置 HH:MM" */
function fmtResetShort(iso: string | null): string {
  if (!iso) return "重置 --:--";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "重置 --:--";
  return `重置 ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 7d 重置：今天显示 HH:MM，跨天显示 M/D HH:MM */
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
  // 安全兜底：最多转 5 秒，避免刷新失败时一直转。
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

function $(id: string): HTMLElement | null {
  return document.getElementById(id);
}

function setText(id: string, text: string) {
  const el = $(id);
  if (el) el.textContent = text;
}

// ===== 渲染单个厂商区块 =====
function renderProvider(key: "claude" | "codex", p: Provider) {
  const zone = $(`zone-${key}`);
  if (!zone) return;

  // 未检测到该工具 → 隐藏整区
  if (!p.present) {
    zone.classList.add("hidden");
    return;
  }
  zone.classList.remove("hidden");
  zone.classList.toggle("stale", p.stale);

  const note = $(`${key}-note`);

  // 未登录 → 提示，% 置 --
  if (!p.loggedIn) {
    setText(`${key}-pct`, "--");
    const bar = $(`${key}-bar`);
    if (bar) bar.style.width = "0%";
    setText(`${key}-5h`, "重置 --:--");
    setText(`${key}-7d`, "-- · --");
    if (note) note.textContent = p.error ?? "未登录";
    return;
  }

  if (note) note.textContent = p.stale ? p.error ?? "同步失败" : "";

  // 5h 作为主显示
  const five = p.fiveHour;
  if (five) {
    setText(`${key}-pct`, fmtPct(five.remaining));
    const bar = $(`${key}-bar`);
    if (bar) {
      bar.style.width = `${Math.max(2, Math.min(100, five.remaining))}%`;
      bar.className = `fill ${barClass(five.remaining)}`;
    }
    setText(`${key}-5h`, fmtResetShort(five.resetsAt));
  }

  // 7d
  const seven = p.sevenDay;
  if (seven) {
    setText(`${key}-7d`, `${fmtPct(seven.remaining)}%　${fmtResetDate(seven.resetsAt)}`);
  }
}

// ===== 渲染整体 =====
function render(state: AppState) {
  renderProvider("claude", state.claude);
  renderProvider("codex", state.codex);

  setText("sync-time", fmtSync(state.lastSync));
  const sync = $("sync");
  if (sync) sync.classList.toggle("warn", state.claude.stale || state.codex.stale);

  // 每家各自的今日 token / API 等价花费（本地日志统计），放在各自区内。
  setTodayLine("claude-today", state.claude);
  setTodayLine("codex-today", state.codex);

  // 数据已更新 → 停止刷新转圈。
  stopSpin();
}

function setTodayLine(id: string, p: Provider) {
  const el = $(id);
  if (!el) return;
  el.innerHTML = `<span class="amt">${fmtCost(p.todayCost)}</span> · <span class="amt">${fmtTokens(p.todayTokens)}</span> tok`;
}

// ===== 启动 =====
window.addEventListener("DOMContentLoaded", async () => {
  $("close")?.addEventListener("click", (e) => {
    e.stopPropagation();
    getCurrentWindow().close();
  });

  // ⟳ 立即刷新：阻止拖拽、转圈、通知后台马上同步一次。
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

  // 首帧兜底：主动拉一次
  try {
    const s = await invoke<AppState>("get_state");
    render(s);
  } catch (_) {
    /* 后台线程稍后会推送 */
  }
});
