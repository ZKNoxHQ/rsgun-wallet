//! Light push check against the Railgun fleet, without touching any broadcaster: subscribe to a
//! content topic of our own, publish timestamped messages on it, and wait for each to come back
//! through filter (published by one fleet node, relayed, pushed back by the others).
//!
//! cargo run --release -p waku-light --example echo -- [rounds, default 3]

use std::time::{Duration, Instant};

use waku_light::{Config, LightNode};

const FLEET: [&str; 3] = [
    "/dns4/relay-a.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x",
    "/dns4/relay-b.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmPtEAoPPok7VLrpNNC6t92ZQFqLndHvkdx6Fk3CxA4MaG",
    "/dns4/client-edge.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmQdCGG5qREQCq96kucmpUVupmvLwrTRjMazPAaMTNP97A",
];

/// Outside every Railgun topic; nothing listens to it but us.
const ECHO_TOPIC: &str = "/zknox-waku-light/1/echo/json";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "waku_light=info".into()))
        .init();
    let rounds: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(3);

    let bootstrap = FLEET.iter().map(|a| a.parse().unwrap()).collect();
    let node = LightNode::start(Config::new(bootstrap, 5, 1)).expect("start");
    node.subscribe([ECHO_TOPIC]);
    let t0 = Instant::now();
    let subscribed = node.wait_subscribed(Duration::from_secs(60)).await;
    println!("subscribed={subscribed} after {:.1}s, {:?}", t0.elapsed().as_secs_f32(), node.status());
    if !subscribed {
        return;
    }
    // Let the other fleet nodes' subscriptions land too, so the echo has a path back.
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("status before publishing: {:?}", node.status());

    let tag = node.local_peer_id().to_string();
    let tag = &tag[tag.len() - 8..];
    let (mut pushed, mut echoed, mut rtts) = (0, 0, Vec::new());
    for round in 1..=rounds {
        let nonce = format!("{tag}-{round}-{}", t0.elapsed().as_millis());
        let payload = format!("{{\"echo\":\"{nonce}\"}}");
        let sent = Instant::now();
        match node.publish(ECHO_TOPIC, payload.as_bytes()).await {
            Ok(report) => {
                pushed += 1;
                println!(
                    "[{round}] light push accepted by {} peer(s) in {} ms: {:?}{}",
                    report.accepted,
                    sent.elapsed().as_millis(),
                    report.accepted_via,
                    if report.failures.is_empty() { String::new() } else { format!(", refused: {:?}", report.failures) }
                );
            }
            Err(e) => {
                println!("[{round}] light push failed: {e}");
                continue;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut back = false;
        while Instant::now() < deadline && !back {
            for m in node.drain() {
                if m.content_topic == ECHO_TOPIC && m.payload == payload.as_bytes() {
                    let rtt = sent.elapsed().as_millis();
                    println!("[{round}] echo received after {rtt} ms");
                    rtts.push(rtt);
                    back = true;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if back {
            echoed += 1;
        } else {
            println!("[{round}] no echo within 20 s");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    rtts.sort_unstable();
    println!(
        "\n{pushed}/{rounds} light push accepted, {echoed}/{rounds} echoed{}; {:?}",
        rtts.get(rtts.len() / 2).map(|m| format!(", median round trip {m} ms")).unwrap_or_default(),
        node.status()
    );
}
