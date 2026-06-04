# LLMMonitor

一个常驻 Windows 桌面的**置顶悬浮小窗**，实时显示 **Claude Code** 和 **Codex** 的额度用量——一眼看出现在还能不能继续猛写代码。

灵感来自把 M5Stack StopWatch 改成「AI 编程额度实体表」的硬件项目，这是它的软件版。

```
┌────────────────────────┐
│ ✷ Claude        ⟳ 22:33 │   标题 + 同步时间
│  73%███████░░░ 剩余      │   5h 剩余% + 进度条
│  5h 重置 18:10          │
│  7d 87%   6/10 17:00    │   7天剩余% + 重置
├────────────────────────┤
│ ◎ Codex                 │
│  98%█████████░ 剩余      │
│  5h 重置 12:42          │
│  7d 95%   6/11 02:10    │
├────────────────────────┤
│ 今日 $1008 · 4.68亿 tok  │   全局汇总
└────────────────────────┘
```

## 数据来源

| 数据 | Claude | Codex |
|---|---|---|
| 5h / 7天 额度% + 重置时间 | `GET api.anthropic.com/api/oauth/usage` | `GET chatgpt.com/backend-api/codex/usage`（兼容本地缓存 `usage-limits.json`） |
| 今日 token / API 等价花费 | 本地扫描 `~/.claude/projects/**/*.jsonl` × 模型单价 | （v1 暂未统计） |

- **额度数据**慢轮询（~100s），**本地用量**快刷新（~20s）。
- 使用本机已有的登录态（`~/.claude/.credentials.json`、`~/.codex/auth.json`），**纯本地运行**，仅用你自己的 token 直连 Anthropic / OpenAI（与官方 CLI 行为一致），**不经过任何第三方服务器**。

## 行为

- 总是置顶、无边框、透明圆角卡片、可拖拽移动。
- 悬停卡片右上角出现关闭按钮。
- 未检测到某工具 → 隐藏该区；token 失效 → 提示重新登录但仍显示本地用量；接口失败 → 保留上次值并标「同步失败」。

## 开发 / 运行

```bash
npm install
npm run tauri dev      # 开发模式，热重载
npm run tauri build    # 打包成 Windows 安装包（.msi + .exe）
```

安装包产物在 `src-tauri/target/release/bundle/`（`msi/` 与 `nsis/`）。装到任何 Windows 电脑上，会自动读取该机器自己的 `~/.claude`、`~/.codex`。

## 说明与限制

- 「今日花费」是 **API 等价成本**（按公开单价估算 token 价值），订阅用户并不会实际支付该金额，仅作用量参考。
- 两家 usage 接口均为**非公开接口**，未来可能变动，代码已做容错降级。
- Claude 的 `User-Agent: claude-code/*` 头是必须的，缺它会撞激进限流。
- v1 未做 OAuth token 主动刷新：Codex token 约 1 小时过期，期间会显示「需重新登录」（用一下 codex CLI 会自动续期）。后续可补刷新逻辑。
- 待办（v2）：Codex 今日 token 统计、`extra_usage` 额外额度展示、开机自启 / 托盘图标 / 透明度调节。

## 技术栈

Tauri v2（Rust 后端 + Vanilla TS 前端）。后端按模块拆分：`auth`（凭证）/ `quota`（额度接口）/ `usage`（本地用量）/ `collector`（采集循环）/ `state`（共享状态）。
