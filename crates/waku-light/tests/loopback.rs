#![cfg(not(target_arch = "wasm32"))]
//! A fake Waku service node (filter + light push + metadata) on a local `/ws` listener, driven
//! by the real `LightNode` over websocket + noise + yamux.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, Transport,
    core::{muxing::StreamMuxerBox, upgrade::Version},
    identify, identity::Keypair, multiaddr::Protocol, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, websocket, yamux,
};
use waku_light::{
    Config, LightNode,
    proto::{
        FilterSubscribeRequest, FilterSubscribeResponse, LightpushRequest, LightpushResponse,
        MessagePush, PushResponse, PushRpc, WakuMessage, WakuMetadataRequest, WakuMetadataResponse,
    },
    protocols,
};

#[derive(NetworkBehaviour)]
struct MockBehaviour {
    identify: identify::Behaviour,
    stream: libp2p_stream::Behaviour,
}

async fn write_lp<M: prost::Message>(s: &mut libp2p::Stream, m: &M) {
    let body = m.encode_to_vec();
    let mut f = Vec::new();
    prost::encoding::encode_varint(body.len() as u64, &mut f);
    f.extend(body);
    s.write_all(&f).await.unwrap();
    s.flush().await.unwrap();
}

async fn read_lp<M: prost::Message + Default>(s: &mut libp2p::Stream) -> M {
    let mut len = 0u64;
    for i in 0.. {
        let mut b = [0u8];
        s.read_exact(&mut b).await.unwrap();
        len |= u64::from(b[0] & 0x7f) << (7 * i);
        if b[0] & 0x80 == 0 {
            break;
        }
    }
    let mut buf = vec![0; len as usize];
    s.read_exact(&mut buf).await.unwrap();
    M::decode(buf.as_slice()).unwrap()
}

#[derive(Default)]
struct Seen {
    subscriber: Option<PeerId>,
    topics: Vec<String>,
    pubsub: Option<String>,
    pings: usize,
    pushed: Vec<(String, WakuMessage)>,
    client_cluster: Option<u32>,
}

struct Mock {
    addr: Multiaddr,
    control: libp2p_stream::Control,
    seen: Arc<Mutex<Seen>>,
}

async fn mock(v3: bool) -> Mock {
    let key = Keypair::generate_secp256k1(); // like the Railgun fleet (16Uiu2…)
    let peer_id = key.public().to_peer_id();
    let transport = websocket::Config::new(tcp::tokio::Transport::new(tcp::Config::default()))
        .upgrade(Version::V1Lazy)
        .authenticate(noise::Config::new(&key).unwrap())
        .multiplex(yamux::Config::default())
        .map(|(p, m), _| (p, StreamMuxerBox::new(m)))
        .boxed();
    let behaviour = MockBehaviour {
        identify: identify::Behaviour::new(identify::Config::new("/ipfs/id/1.0.0".into(), key.public())),
        stream: libp2p_stream::Behaviour::new(),
    };
    let mut control = behaviour.stream.new_control();
    let mut swarm = Swarm::new(
        transport,
        behaviour,
        peer_id,
        libp2p::swarm::Config::with_tokio_executor().with_idle_connection_timeout(Duration::from_secs(60)),
    );
    swarm.listen_on("/ip4/127.0.0.1/tcp/0/ws".parse().unwrap()).unwrap();
    let addr = loop {
        if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
            break address.with(Protocol::P2p(peer_id));
        }
    };
    let seen = Arc::new(Mutex::new(Seen::default()));

    let mut subs = control.accept(StreamProtocol::new(protocols::FILTER_SUBSCRIBE)).unwrap();
    let s2 = seen.clone();
    tokio::spawn(async move {
        while let Some((peer, mut st)) = subs.next().await {
            let req: FilterSubscribeRequest = read_lp(&mut st).await;
            {
                let mut s = s2.lock().unwrap();
                match req.filter_subscribe_type {
                    1 => {
                        s.subscriber = Some(peer);
                        s.topics.extend(req.content_topics);
                        s.pubsub = req.pubsub_topic;
                    }
                    0 => s.pings += 1,
                    _ => {}
                }
            }
            let resp = FilterSubscribeResponse { request_id: req.request_id, status_code: 200, status_desc: None };
            write_lp(&mut st, &resp).await;
        }
    });

    if v3 {
        let mut lp = control.accept(StreamProtocol::new(protocols::LIGHTPUSH_V3)).unwrap();
        let s3 = seen.clone();
        tokio::spawn(async move {
            while let Some((_, mut st)) = lp.next().await {
                let req: LightpushRequest = read_lp(&mut st).await;
                s3.lock().unwrap().pushed.push((req.pubsub_topic.unwrap(), req.message.unwrap()));
                let resp = LightpushResponse { request_id: req.request_id, status_code: 200, status_desc: None, relay_peer_count: Some(4) };
                write_lp(&mut st, &resp).await;
            }
        });
    } else {
        let mut lp = control.accept(StreamProtocol::new(protocols::LIGHTPUSH_V2)).unwrap();
        let s3 = seen.clone();
        tokio::spawn(async move {
            while let Some((_, mut st)) = lp.next().await {
                let rpc: PushRpc = read_lp(&mut st).await;
                let r = rpc.request.unwrap();
                s3.lock().unwrap().pushed.push((r.pubsub_topic, r.message.unwrap()));
                let resp = PushRpc { request_id: rpc.request_id, request: None, response: Some(PushResponse { is_success: true, info: None }) };
                write_lp(&mut st, &resp).await;
            }
        });
    }

    let mut md = control.accept(StreamProtocol::new(protocols::METADATA)).unwrap();
    tokio::spawn(async move {
        while let Some((_, mut st)) = md.next().await {
            let _req: WakuMetadataRequest = read_lp(&mut st).await;
            write_lp(&mut st, &WakuMetadataResponse { cluster_id: Some(5), shards: vec![1] }).await;
        }
    });

    tokio::spawn(async move {
        loop {
            swarm.select_next_some().await;
        }
    });
    Mock { addr, control, seen }
}

impl Mock {
    async fn push(&self, peer: PeerId, msg: WakuMessage) {
        let mut c = self.control.clone();
        let mut st = c.open_stream(peer, StreamProtocol::new(protocols::FILTER_PUSH)).await.unwrap();
        write_lp(&mut st, &MessagePush { waku_message: Some(msg), pubsub_topic: Some("/waku/2/rs/5/1".into()) }).await;
        st.close().await.unwrap();
    }

    async fn query_metadata(&self, peer: PeerId) -> Option<u32> {
        let mut c = self.control.clone();
        let mut st = c.open_stream(peer, StreamProtocol::new(protocols::METADATA)).await.unwrap();
        write_lp(&mut st, &WakuMetadataRequest { cluster_id: Some(5), shards: vec![1] }).await;
        let resp: WakuMetadataResponse = read_lp(&mut st).await;
        resp.cluster_id
    }
}

fn config(addr: Multiaddr) -> Config {
    let mut c = Config::new(vec![addr], 5, 1);
    c.filter_ping_interval = Duration::from_secs(1);
    c
}

async fn roundtrip(v3: bool) {
    let _ = tracing_subscriber::fmt().with_env_filter("waku_light=debug").try_init();
    let mock = mock(v3).await;
    let node = LightNode::start(config(mock.addr.clone())).unwrap();
    node.subscribe(["/railgun/v2/0-11155111-fees/json", "/railgun/v2/0-11155111-transact-response/json"]);
    assert!(node.wait_subscribed(Duration::from_secs(15)).await, "{:?}", node.status());

    let client = mock.seen.lock().unwrap().subscriber.unwrap();
    assert_eq!(client, node.local_peer_id());
    {
        let s = mock.seen.lock().unwrap();
        assert_eq!(s.pubsub.as_deref(), Some("/waku/2/rs/5/1"));
        assert_eq!(s.topics.len(), 2);
    }
    let st = node.status();
    assert_eq!((st.connected_peers, st.service_peers, st.filter_subscriptions), (1, 1, 1));
    assert_eq!(st.last_error, None, "cleared by the successful subscription");

    // Same message twice (two service nodes would do that), one on a topic we did not ask for.
    let fee = WakuMessage {
        payload: b"{\"data\":\"00\"}".to_vec(),
        content_topic: "/railgun/v2/0-11155111-fees/json".into(),
        timestamp: Some(1_758_000_000_123_000_000),
        ..Default::default()
    };
    mock.push(client, fee.clone()).await;
    mock.push(client, fee.clone()).await;
    mock.push(client, WakuMessage { content_topic: "/other/1/x/proto".into(), ..fee.clone() }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let got = node.drain();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].payload, fee.payload);
    assert_eq!(got[0].timestamp_ns, Some(1_758_000_000_123_000_000));

    let report = node.publish("/railgun/v2/0-11155111-transact/json", b"sealed").await.unwrap();
    assert_eq!(report.accepted, 1);
    {
        let s = mock.seen.lock().unwrap();
        let (pubsub, m) = &s.pushed[0];
        assert_eq!(pubsub, "/waku/2/rs/5/1");
        assert_eq!(m.payload, b"sealed");
        assert!(m.timestamp.unwrap() > 1_700_000_000_000_000_000);
    }

    assert_eq!(mock.query_metadata(client).await, Some(5));
    mock.seen.lock().unwrap().client_cluster = Some(5);

    // Subscriber pings keep the subscription alive.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(mock.seen.lock().unwrap().pings >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn filter_and_lightpush_v3() {
    roundtrip(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lightpush_v2_fallback() {
    roundtrip(false).await;
}

/// On plain ws nothing but the pin authenticates the peer: a different identity is refused, and
/// the rotated-identity tolerance (wss only) does not apply.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_pin_on_ws_is_refused() {
    let mock = mock(true).await;
    let mut addr = mock.addr.clone();
    addr.pop();
    let other = Keypair::generate_secp256k1().public().to_peer_id();
    let node = LightNode::start(config(addr.with(Protocol::P2p(other)))).unwrap();
    node.subscribe(["/railgun/v2/0-11155111-fees/json"]);
    assert!(!node.wait_subscribed(Duration::from_secs(4)).await);
    let st = node.status();
    assert_eq!(st.connected_peers, 0);
    assert!(st.last_error.unwrap_or_default().contains("Unexpected peer ID"));
}
