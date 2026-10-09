//! Passive check against the Railgun fleet: connect, subscribe to the fees topic of a chain, count
//! what arrives. Publishes nothing.
//!
//! cargo run --release -p waku-light --example fleet -- [chain id, default 11155111] [seconds, default 60]

use std::time::{Duration, Instant};

use waku_light::{Config, LightNode};

const FLEET: [&str; 3] = [
    "/dns4/relay-a.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x",
    "/dns4/relay-b.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmPtEAoPPok7VLrpNNC6t92ZQFqLndHvkdx6Fk3CxA4MaG",
    "/dns4/client-edge.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmQdCGG5qREQCq96kucmpUVupmvLwrTRjMazPAaMTNP97A",
];

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "waku_light=info".into()))
        .init();
    let mut args = std::env::args().skip(1);
    let chain: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(11_155_111);
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);

    let bootstrap = FLEET.iter().map(|a| a.parse().unwrap()).collect();
    let node = LightNode::start(Config::new(bootstrap, 5, 1)).expect("start");
    let topic = format!("/railgun/v2/0-{chain}-fees/json");
    node.subscribe([topic.clone()]);

    let t0 = Instant::now();
    let subscribed = node.wait_subscribed(Duration::from_secs(60)).await;
    println!("subscribed={subscribed} after {:.1}s, status {:?}", t0.elapsed().as_secs_f32(), node.status());

    let mut total = 0usize;
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let batch = node.drain();
        total += batch.len();
        for m in &batch {
            let head = String::from_utf8_lossy(&m.payload[..m.payload.len().min(80)]).to_string();
            println!("{} {} bytes  {head}…", m.content_topic, m.payload.len());
        }
        println!("-- {total} fee message(s) so far, {:?}", node.status());
    }
}
