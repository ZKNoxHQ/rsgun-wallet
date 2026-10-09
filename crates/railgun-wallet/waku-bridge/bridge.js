// Waku light node for the wallet tab. Bundled with esbuild into static/waku-bundle.js and
// served by the daemon; the page talks to it through window.RailgunWaku.
//
// Mirrors @railgun-community/waku-broadcaster-client-web 9.x on @waku/sdk 0.0.36: light node on
// the Railgun static shard, filter subscriptions to receive, light push to send. Only the
// fleet's fixed wss bootstrap peers are dialled (no DNS discovery, no peer exchange), which
// keeps the page's connect-src to one domain.
import { createLightNode, createEncoder, createDecoder, Protocols, utils } from "@waku/sdk";

let node = null;
let routingInfo = null;
let status = { state: "stopped", peers: 0, error: null };
const subscribed = new Set();

function setStatus(patch, onStatus) {
  status = { ...status, ...patch };
  if (onStatus) onStatus({ ...status });
}

function peerCount() {
  try { return node ? node.libp2p.getConnections().length : 0; } catch { return 0; }
}

async function start(config, onMessage, onStatus) {
  if (node) return;
  const { clusterId, shardId, bootstrapPeers, contentTopics } = config;
  setStatus({ state: "starting", error: null }, onStatus);
  try {
    const networkConfig = { clusterId };
    routingInfo = utils.createRoutingInfo(networkConfig, { shardId });
    node = await createLightNode({
      autoStart: false,
      defaultBootstrap: false,
      bootstrapPeers,
      networkConfig,
    });
    await node.start();
    node.libp2p.addEventListener("peer:connect", () => setStatus({ peers: peerCount() }, onStatus));
    node.libp2p.addEventListener("peer:disconnect", () => setStatus({ peers: peerCount() }, onStatus));

    setStatus({ state: "connecting" }, onStatus);
    await node.waitForPeers([Protocols.Filter, Protocols.LightPush], config.peerTimeoutMs || 60000);
    setStatus({ state: "subscribing", peers: peerCount() }, onStatus);

    for (const contentTopic of contentTopics) {
      const decoder = createDecoder(contentTopic, routingInfo);
      await node.filter.subscribe(decoder, (message) => {
        if (!message || !message.payload) return;
        onMessage({
          contentTopic,
          payload: message.payload,
          timestampNs: message.timestamp ? String(BigInt(message.timestamp.getTime()) * 1000000n) : null,
        });
      });
      subscribed.add(contentTopic);
    }
    setStatus({ state: "connected", peers: peerCount() }, onStatus);
  } catch (err) {
    const message = err && err.message ? err.message : String(err);
    setStatus({ state: "error", error: message, peers: peerCount() }, onStatus);
    await stop();
    throw err;
  }
}

async function send(contentTopic, payload) {
  if (!node || !routingInfo) throw new Error("waku node not started");
  const encoder = createEncoder({ contentTopic, routingInfo });
  const result = await node.lightPush.send(encoder, { payload });
  const accepted = (result.successes || []).length;
  if (accepted === 0) {
    const reasons = (result.failures || []).map((f) => f.error || "unknown").join(", ");
    throw new Error("light push refused by every peer" + (reasons ? ": " + reasons : ""));
  }
  return accepted;
}

async function stop() {
  const n = node;
  node = null; routingInfo = null; subscribed.clear();
  if (n) { try { await n.stop(); } catch {} }
  status = { state: "stopped", peers: 0, error: status.error };
}

window.RailgunWaku = { start, send, stop, status: () => ({ ...status, peers: peerCount() }) };
