# LLMMonitor

一个常驻 Windows 桌面的**置顶悬浮小窗**，实时显示 **Claude Code** 和 **Codex** 的额度用量 + **AI 完工/卡住/出错提醒**——一眼看出还能不能继续猛写代码，以及哪个 agent 该回去看了。

灵感来自把 M5Stack StopWatch 改成「AI 编程额度实体表」的硬件项目，这是它的软件版。

```
┌────────────────────────────┐
│ ✷ Claude            ⟳ 22:33 │  标题 + 同步时间（点 ⟳ 立即刷新）
│  73%███████░░░ 剩余          │  5h 剩余% + 进度条
│  5h 重置 18:10              │
│  7d 87%   6/10 17:00        │  7天剩余% + 重置
│  今日 $1008 · 4.68亿 tok     │  各自的今日 token / 等价花费
│ ◎ Codex                     │
│  98%█████████░ 剩余          │
│  5h 重置 12:42              │
│  7d 95%   6/11 02:10        │
│  今日 $.. · 0.71亿 tok       │
├────────────────────────────┤
│ ▌✷ implement-toast   完成    │  ← 信号隧道：每个会话一条
│ ▌◎ SanZhanAssistant  完成    │     绿=完成 / 黄=等你 / 红=出错
└────────────────────────────┘
```

## 两块能力

### 1. 额度监视
- **5h / 7天 额度% + 重置时间**：调厂商内部 usage 接口（Claude `api/oauth/usage`、Codex `backend-api/codex/usage`），慢轮询 ~100s。
- **今日 token / API 等价花费**：本地解析会话日志（Claude `~/.claude/projects`、Codex `~/.codex/sessions`），各算各的，快刷新 ~20s。

### 2. 信号隧道（完工/卡住/出错提醒）
卡片下方一条竖向状态条，每个近期会话一条：

| 灯 | 含义 | 日志信号 | 消失条件 |
|---|---|---|---|
| 🟢 完成 | 干完一轮、等你下一步 | Claude `stop_reason=end_turn` / Codex `task_complete` | 点击 / 会话又继续 / 超时 |
| 🟡 等你 | 卡住等批准·输入 | Claude `tool_use` 卡住超阈值 | 同上 |
| 🔴 出错 | 会话中断 / 限流 | Claude `isApiErrorMessage` / Codex `turn_aborted` | 同上 |

- 状态突变时弹 **Windows 通知**（进通知中心，人不在也不漏）。
- chip 名字用 Claude 的 **aiTitle**（AI 生成的会话标题，如 `implement-toast-notifications`），比文件夹名好认；Codex 用仓库名。
- 点击某条 → 淡出消失；最多叠 5 条。秒级检测，纯读日志，不监听键盘/屏幕。

## 隐私

使用本机已有的登录态（`~/.claude/.credentials.json`、`~/.codex/auth.json`），**纯本地运行**，仅用你自己的 token 直连 Anthropic / OpenAI（与官方 CLI 行为一致），**不监听键盘/截屏/监控窗口**，**不经过任何第三方服务器**。

## 行为

- 总是置顶、无边框、透明圆角卡片、可拖拽移动；从任务栏隐藏。
- **系统托盘图标**：左键显隐，右键菜单（显示/隐藏、立即刷新、退出）；点 ✕ 收起到托盘而非退出。
- 未检测到某工具 → 隐藏该区；token 失效 → 提示重新登录但仍显示本地用量；接口失败 → 保留上次值并标「同步失败」。

## 开发 / 运行

```bash
npm install
npm run tauri dev      # 开发模式，热重载
npm run tauri build    # 打包成 Windows 安装包（.msi + .exe）
```

安装包产物在 `src-tauri/target/release/bundle/`（`msi/` 与 `nsis/`）。装到任何 Windows 电脑上，会自动读取该机器自己的 `~/.claude`、`~/.codex`。

## 说明与限制

- 「今日花费」是 **API 等价成本**（按公开单价估算 token 价值），订阅用户并不实际支付，仅作用量参考。
- 两家 usage 接口均为**非公开接口**，未来可能变动，代码已做容错降级。
- v1 未做 OAuth token 主动刷新：Codex token 约 1 小时过期，期间额度栏可能显示「需重新登录」（用一下 codex CLI 自动续期）。
- 信号灯超时兜底默认 30 分钟（计划做成可配置）。

## 待办（下一期）

- **团队版**：局域网 UDP 广播彼此的 AI 活动状态 + Bongo Cat 动画面板（活动检测引擎复用本期的 `activity` 模块）。详见 `docs/团队版-Bongcat-实施计划.md`。
- 信号灯各项阈值/超时做成设置项；`extra_usage` 额外额度展示；开机自启。

## 技术栈

Tauri v2（Rust 后端 + Vanilla TS 前端）。后端按模块拆分：`auth`（凭证）/ `quota`（额度接口）/ `usage`（本地用量）/ `activity`（信号隧道：完工/卡住/出错检测）/ `collector`（采集循环）/ `state`（共享状态）。
