// 局域网 UDP 广播双机测试。
// 用法：在两台机器上各自运行：  node udp-lan-test.js 你的名字
// 例如 A 机：node udp-lan-test.js Alice    B 机：node udp-lan-test.js Bob
// 若两边都能看到对方的名字 → UDP 广播在你们网络可用，团队面板走 UDP 方案。
// 若只看到自己、看不到对方 → 网络屏蔽了广播/做了客户端隔离 → 改走内网 WebSocket 方案。

const dgram = require("dgram");
const os = require("os");

const PORT = 48888;
const NAME = process.argv[2] || os.hostname();
const INSTANCE = `${NAME}@${Math.random().toString(36).slice(2, 8)}`;

// 找本机 IPv4 与子网广播地址
let localIp = null;
const targets = new Set(["255.255.255.255"]);
for (const n in os.networkInterfaces()) {
  for (const i of os.networkInterfaces()[n]) {
    if (i.family === "IPv4" && !i.internal) {
      localIp = i.address;
      const ip = i.address.split(".").map(Number);
      const mask = i.netmask.split(".").map(Number);
      targets.add(ip.map((o, k) => (o & mask[k]) | (~mask[k] & 255)).join("."));
    }
  }
}

console.log(`[${INSTANCE}] 本机 IP ${localIp}，监听 UDP ${PORT}，广播目标:`, [...targets].join(", "));
console.log("等待对方出现…（Ctrl+C 退出）\n");

const sock = dgram.createSocket({ type: "udp4", reuseAddr: true });
const seen = new Map(); // instance -> last seen ts

sock.on("message", (msg, rinfo) => {
  let m;
  try { m = JSON.parse(msg.toString()); } catch { return; }
  if (!m.instance) return;
  if (m.instance === INSTANCE) return; // 忽略自己
  if (!seen.has(m.instance)) {
    console.log(`✅ 发现同伴: ${m.name}  (${m.instance})  来自 ${rinfo.address}`);
  }
  seen.set(m.instance, Date.now());
});

sock.bind(PORT, () => {
  sock.setBroadcast(true);
  // 每秒广播一次自己
  setInterval(() => {
    const payload = JSON.stringify({ instance: INSTANCE, name: NAME, ts: Date.now() });
    for (const t of targets) sock.send(payload, PORT, t);
  }, 1000);
});

// 每 5 秒汇报一次当前在线同伴 + 清理超 8 秒没消息的
setInterval(() => {
  const now = Date.now();
  for (const [k, v] of seen) if (now - v > 8000) { seen.delete(k); console.log(`⚠️ 同伴离线: ${k}`); }
  const peers = [...seen.keys()];
  console.log(`[心跳] 在线同伴 ${peers.length} 个${peers.length ? ": " + peers.join(", ") : "（暂无，等对方启动）"}`);
}, 5000);
