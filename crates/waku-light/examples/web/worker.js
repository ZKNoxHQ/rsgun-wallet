// Dedicated Web Worker hosting the waku-light node (same wasm as index.html). This is the form a
// web wallet takes: the node lives in the worker, the page only renders what it posts.
import init, { WebNode } from "./pkg/web_fleet.js";

const ECHO = "/zknox-waku-light/1/echo/json";
const ready = init();
let node = null, started = 0, firstAt = null, total = 0, timer = null;
let lastPoll = 0, maxGap = 0;
const pending = new Map();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const post = (type, data = {}) => postMessage({ type, ...data });

function decodeFee(bytes) {
  try {
    const outer = JSON.parse(new TextDecoder().decode(bytes));
    const hex = outer.data.startsWith("0x") ? outer.data.slice(2) : outer.data;
    const raw = new Uint8Array(hex.match(/../g).map((h) => parseInt(h, 16)));
    return JSON.parse(new TextDecoder().decode(raw));
  } catch { return null; }
}

function poll() {
  const now = Date.now();
  if (lastPoll) maxGap = Math.max(maxGap, now - lastPoll);
  lastPoll = now;
  if (!node) return;
  const fees = [];
  for (const m of node.drain()) {
    if (m.contentTopic === ECHO) {
      const text = new TextDecoder().decode(m.payload);
      const p = pending.get(text);
      if (p) { post("echo", { line: `[${p.round}] echo received after ${Date.now() - p.sentAt} ms` }); pending.delete(text); }
      continue;
    }
    total += 1;
    if (firstAt === null) firstAt = (now - started) / 1000;
    const d = decodeFee(m.payload);
    if (d && d.railgunAddress) fees.push({
      address: d.railgunAddress, identifier: d.identifier, version: d.version, wallets: d.availableWallets,
      reliability: d.reliability, tokens: Object.keys(d.fees || {}).length, expires: d.feeExpiration,
    });
  }
  const s = node.status();
  post("status", {
    peerId: node.peerId(), topic: node.topic(), total, firstAt, fees, maxGap,
    uptime: Math.round((now - started) / 1000),
    status: { connectedPeers: s.connectedPeers, servicePeers: s.servicePeers, filterSubscriptions: s.filterSubscriptions, lastError: s.lastError },
  });
}

async function echo() {
  if (!node) return;
  pending.clear();
  node.subscribe(ECHO);
  post("echo", { line: `subscribing to ${ECHO}, publishing in 6 s` });
  await sleep(6000);
  for (let round = 1; round <= 3 && node; round++) {
    const text = JSON.stringify({ echo: `${node.peerId().slice(-8)}-${round}-${Date.now()}` });
    pending.set(text, { round, sentAt: Date.now() });
    try {
      const r = await node.publish(ECHO, new TextEncoder().encode(text));
      post("echo", { line: `[${round}] light push accepted by ${r.accepted} peer(s): ${r.via.join(", ")}` + (r.failures.length ? `; refused: ${r.failures.join("; ")}` : "") });
    } catch (e) {
      pending.delete(text);
      post("echo", { line: `[${round}] light push failed: ${e}` });
    }
    await sleep(8000);
  }
  for (const p of pending.values()) post("echo", { line: `[${p.round}] no echo within 8 s` });
  pending.clear();
  post("echo", { line: "echo test done", done: true });
}

onmessage = async (e) => {
  await ready;
  const { cmd } = e.data;
  if (cmd === "start") {
    if (node) node.free();
    node = new WebNode(e.data.chainId);
    started = Date.now(); total = 0; firstAt = null; maxGap = 0; lastPoll = 0;
    if (!timer) timer = setInterval(poll, 1000);
  } else if (cmd === "stop") {
    if (node) { node.free(); node = null; }
    post("stopped");
  } else if (cmd === "echo") {
    echo();
  }
};
post("loaded");
