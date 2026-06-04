# 项目说明:团队 AI 活动监视器(Bongcat 版)

> 这是一份交接文档,目标读者是 Claude Code。请基于以下背景和决策,帮我把现有工具扩展成一个团队版的「Bongcat AI 活动状态面板」。

---

## 1. 要做什么(目标)

我已经有一个本地的「AI 用量监视器」(原生桌面悬浮窗),目前能显示自己的 Claude / Codex 用量:剩余额度百分比、token 消耗等数据。

现在要把它**扩展成团队版**:让一个 2-3 人的小团队能**互相看到彼此的 AI 是不是正在干活**,并用 **Bongo Cat(拍桌子的卡通猫)动画**来可视化——某人的 AI 正在跑任务时,他那只猫就「啪啪」拍手;闲着时猫就打盹。本质上是一个开发工具版的 Rich Presence(类似 Steam/Discord「好友正在玩什么」的状态广播)。

---

## 2. 现状(已有的东西)

- 已有一个原生桌面监视器,技术栈是原生桌面方向(Tauri / C# / Swift 等,以 Tauri 为主假设)。
- 它已经在读取 Claude / Codex 的 token 消耗和剩余额度数据。**这一点很重要:活动检测可以直接复用这个现成的数据源。**
- 已经做了一个 UI 原型 `bongcat-panel.html`(见第 7 节),包含 CSS 画的猫和忙/闲两种动画,可作为前端基础。

---

## 3. 环境约束(关键前提)

- 团队成员**全部在中国大陆**。
- 平时**在同一个办公网 / 局域网**里。
- 结论:**不需要上云,也不要用 Supabase 等境外服务**(跨境连接有延迟和稳定性问题)。直接在局域网内解决,跨境烦恼归零。

---

## 4. 整体架构(三层)

```
[本地活动检测] → [局域网内状态同步] → [各自的悬浮窗里画猫]
     ↑复用现有                              ↑原型已就绪
     token 监控
```

### 4.1 活动检测:怎么判断「AI 在干活」

最省事的方式:**复用现有的 token 监控,比较两次采样之间 token 数有没有增长**。

- token 在涨 = 正在干活(busy = true)
- 一段时间不变 = 闲着(busy = false)

不需要额外去 hook 进程或拦截 API。(如需更精确,后续可加:监听 CLI 进程输出 / CPU 占用,但第一版不做。)

### 4.2 状态同步:推荐 UDP 局域网广播(首选)

因为在同一局域网,**连中心服务器都不需要**。每台机器:往局域网广播自己的状态 + 同时监听别人的状态。优点:无单点、零配置、即插即用,任何一台机器关机都不影响其他人。

### 4.3 可视化

悬浮窗里给每个队友一只猫,根据收到的 `busy` 状态决定播放拍手动画还是打盹。UI 见 `bongcat-panel.html`。

---

## 5. 推荐方案 + 备选

### 首选:UDP 局域网广播

数据结构(每条状态消息):

```json
{ "user": "我", "busy": true, "task": "claude code · 重构支付模块", "tokens": 5.85 }
```

Tauri 场景下,广播/监听逻辑放在 Rust 后端,前端负责画猫。

**广播自己的状态(每隔几秒一次,数据来自现有 token 监控):**

```rust
use std::net::UdpSocket;

fn broadcast_status(busy: bool, task: &str, tokens: f64) -> std::io::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    let payload = serde_json::json!({
        "user": "我", "busy": busy, "task": task, "tokens": tokens
    }).to_string();
    socket.send_to(payload.as_bytes(), "255.255.255.255:48888")?; // 往全网段广播
    Ok(())
}
```

**监听别人的状态(后台线程,收到就转发给前端):**

```rust
use tauri::Manager;

fn start_listener(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let socket = UdpSocket::bind("0.0.0.0:48888").expect("绑定端口失败");
        let mut buf = [0u8; 1024];
        loop {
            if let Ok((len, _)) = socket.recv_from(&mut buf) {
                if let Ok(text) = std::str::from_utf8(&buf[..len]) {
                    let _ = app.emit_all("peer-status", text.to_string());
                }
            }
        }
    });
}
```

**前端收到后更新对应那只猫:**

```javascript
import { listen } from '@tauri-apps/api/event'

listen('peer-status', e => {
  const m = JSON.parse(e.payload)
  updateCat(m)   // 找到 m.user 那张卡,根据 m.busy 加/去掉 busy class
})
```

### 备选:内网 WebSocket 中心节点

如果公司网络屏蔽了 UDP 广播,或做了无线/有线隔离(client isolation),则退而求其次:指定一台常开机器跑一个十几行的 WebSocket 小服务,其他人连它的**内网 IP**(`ws://192.168.x.x:端口`)。仍然不出局域网、不碰云。

---

## 6. UI 原型说明(bongcat-panel.html)

已有的原型是一个深色悬浮窗面板,特点:

- 每个队友一张卡片:头像名字 + 一只猫 + 状态文字(在跑什么 / 闲置多久) + token 数。
- 猫和动画**全部用 CSS 实现**,无图片依赖:
  - `.card.busy` → 两只爪子交替拍打(bongo 动画)+ 冒音符。
  - `.card.idle` → 猫变暗、轻微呼吸、冒「z」打盹。
- 状态切换只需给卡片加/去掉 `busy` class,动画自动响应。
- 这套 UI 可以直接塞进 Tauri 的前端层。

---

## 7. 给你的任务清单(建议顺序)

1. **先验证网络**:写一个最小的 UDP 一发一收测试,在两台机器上跑,**确认能收到包**。这一步决定走 UDP 还是 WebSocket。
2. 把 `bongcat-panel.html` 的 UI 集成进现有监视器的前端(替换/扩展现有界面)。
3. 实现活动检测:在现有 token 监控逻辑里加「token 增量 → busy 布尔值」的判断。
4. 实现 UDP 广播 + 监听(第 5 节代码),把本地状态广播出去、把收到的 peer 状态 emit 给前端。
5. 前端 `listen('peer-status')`,维护一个 peer 列表并渲染对应的猫。
6. 实现**离线判断**:给每个 peer 记「最后收到时间」,超过约 15 秒没消息就标灰或移除(这样谁关了程序面板能反映出来)。

---

## 8. 注意事项 / 坑

- **端口统一**:示例用 48888,所有成员保持一致即可(挑一个不冲突的)。
- **UDP 广播可能被屏蔽**:部分企业网络禁用广播或隔离客户端,务必先做第 1 步测试。不通就切到 WebSocket 备选方案。
- **离线检测必须做**:否则关掉程序的人会一直挂在面板上。
- **第一版别过度设计**:活动检测先用最简单的「token 增量」,不要一上来就 hook 进程。
- 用户名 `"我"` 是占位,实际应做成可配置(每台机器填自己的名字)。
