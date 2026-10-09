//! Dials one multiaddr and prints how far the connection gets (TLS, noise, muxer, identify).
//!
//! cargo run --release -p waku-light --example probe -- /dns4/relay-a.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x

use std::time::Duration;

use waku_light::{Config, LightNode};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "waku_light=debug".into()))
        .init();
    let addr = std::env::args().nth(1).expect("multiaddr with /p2p/<peer id>");
    let node = LightNode::start(Config::new(vec![addr.parse().expect("multiaddr")], 5, 1)).expect("start");
    node.subscribe(["/railgun/v2/0-11155111-fees/json"]);
    let ok = node.wait_subscribed(Duration::from_secs(20)).await;
    println!("subscribed={ok} {:?}", node.status());
}
