import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

document.addEventListener("contextmenu", (e) => e.preventDefault());

interface Settings {
  waitingThresholdSecs: number;
  runningGraceSecs: number;
  toastDone: boolean;
  toastWaiting: boolean;
  toastError: boolean;
  opacity: number;
  idleAfterSecs: number;
  autoStart: boolean;
}

function $(id: string): HTMLInputElement {
  return document.getElementById(id) as HTMLInputElement;
}

let saveTimer: number | undefined;

function collect(): Settings {
  return {
    waitingThresholdSecs: Math.max(10, parseInt($("waiting").value) || 180),
    runningGraceSecs: Math.max(5, parseInt($("grace").value) || 60),
    toastDone: $("td").checked,
    toastWaiting: $("tw").checked,
    toastError: $("te").checked,
    opacity: parseFloat($("op").value) || 1,
    idleAfterSecs: Math.max(60, parseInt($("idle").value) || 300),
    autoStart: $("auto").checked,
  };
}

async function persist() {
  await invoke("save_settings", { new: collect() }).catch(() => {});
  const el = document.getElementById("saved")!;
  el.textContent = "已保存";
  el.classList.add("show");
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = window.setTimeout(() => el.classList.remove("show"), 1200);
}

function bind() {
  // 改即存（防抖）
  const debounced = () => {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = window.setTimeout(persist, 250);
  };
  for (const id of ["waiting", "grace", "idle", "op"]) {
    $(id).addEventListener("input", () => {
      if (id === "op") $("opv").textContent = Math.round(parseFloat($("op").value) * 100) + "%";
      debounced();
    });
  }
  for (const id of ["td", "tw", "te", "auto"]) {
    $(id).addEventListener("change", persist);
  }
  document.getElementById("close")!.addEventListener("click", () => getCurrentWindow().hide());
}

window.addEventListener("DOMContentLoaded", async () => {
  bind();
  try {
    const s = await invoke<Settings>("get_settings");
    $("waiting").value = String(s.waitingThresholdSecs);
    $("grace").value = String(s.runningGraceSecs);
    $("td").checked = s.toastDone;
    $("tw").checked = s.toastWaiting;
    $("te").checked = s.toastError;
    $("op").value = String(s.opacity);
    document.getElementById("opv")!.textContent = Math.round(s.opacity * 100) + "%";
    $("idle").value = String(s.idleAfterSecs);
    $("auto").checked = s.autoStart;
  } catch (_) {
    /* 用默认值 */
  }
});
