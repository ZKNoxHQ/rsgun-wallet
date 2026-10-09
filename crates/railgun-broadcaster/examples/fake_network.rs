//! A pretend broadcaster network on localhost, for developing a wallet without Waku.
//!
//! Serves the subset of the nwaku REST API that `NwakuRest` uses, backed by an in-memory hub, and
//! runs one mock broadcaster on it. The broadcaster announces a wrapped-token fee every five
//! seconds and answers every request with a made-up transaction hash: nothing reaches a chain,
//! notes are not spent.
//!
//! cargo run -p railgun-broadcaster --example fake_network -- 8645 11155111 0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14 112

use std::{sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use railgun_broadcaster::{
    mock::{MockAnswer, MockBroadcaster},
    transport::{WakuTransport, memory::MemoryHub},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().map(|p| p.parse()).transpose()?.unwrap_or(8645);
    let chain: u64 = args.next().map(|c| c.parse()).transpose()?.unwrap_or(11_155_111);
    let token = args
        .next()
        .unwrap_or_else(|| "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14".into());
    // Rate as a multiple of the gas cost, in hundredths: 112 = 1.12 times. Try 4000 to see a
    // wallet's rate ceiling refuse the offer.
    let rate_hundredths: u128 = args.next().map(|r| r.parse()).transpose()?.unwrap_or(112);
    let rate = railgun_broadcaster::PAR_RATE_WRAPPED_BASE_TOKEN / 100 * rate_hundredths;

    let hub = MemoryHub::new();
    let rest_side: Arc<dyn WakuTransport> = Arc::new(hub.handle());
    let broadcaster = MockBroadcaster::new(Arc::new(hub.handle()), chain, 7);
    println!("fake broadcaster {} on chain {chain}", broadcaster.railgun_address);
    println!("(paste this address as trusted fee signer to try the capped mode: it is then its own authority)");

    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            if tick % 10 == 0 {
                // Valid two minutes.
                broadcaster
                    .announce(&[(&token, rate)], &format!("fake-{tick}"), 120_000)
                    .await;
            }
            for params in broadcaster
                .serve(|_| MockAnswer::TxHash(format!("0x{}", "fa".repeat(32))))
                .await
            {
                println!(
                    "request: feesID {} minGasPrice {} calldata {} bytes, {} POI list(s)",
                    params.fees_id,
                    params.min_gas_price,
                    params.data.len() / 2 - 1,
                    params.pre_transaction_pois.len()
                );
            }
            tick += 1;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    println!("fake nwaku REST on http://127.0.0.1:{port}");
    loop {
        let (mut socket, _) = listener.accept().await?;
        let transport = rest_side.clone();
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head_end, length) = loop {
                let Ok(n) = socket.read(&mut chunk).await else { return };
                if n == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buffer[..pos]).to_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (pos + 4, length);
                }
            };
            while buffer.len() < head_end + length {
                let Ok(n) = socket.read(&mut chunk).await else { return };
                if n == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let mut first = head.lines().next().unwrap_or("").split(' ');
            let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
            let body = &buffer[head_end..];

            let (status, reply): (&str, Value) = match (method, path) {
                ("POST", "/relay/v1/subscriptions") => ("200 OK", json!("OK")),
                ("GET", "/admin/v1/peers") => ("200 OK", json!([{ "multiaddr": "fake" }])),
                ("GET", p) if p.starts_with("/relay/v1/messages/") => {
                    let messages = transport.poll().await.unwrap_or_default();
                    let list: Vec<Value> = messages
                        .iter()
                        .map(|m| {
                            json!({
                                "payload": STANDARD.encode(&m.payload),
                                "contentTopic": m.content_topic,
                                "version": 0,
                                "timestamp": 0
                            })
                        })
                        .collect();
                    ("200 OK", json!(list))
                }
                ("POST", p) if p.starts_with("/relay/v1/messages/") => {
                    match serde_json::from_slice::<Value>(body) {
                        Ok(v) => {
                            let payload = v["payload"].as_str().and_then(|p| STANDARD.decode(p).ok());
                            match (payload, v["contentTopic"].as_str()) {
                                (Some(payload), Some(topic)) => {
                                    let _ = transport.publish(topic, &payload).await;
                                    ("200 OK", json!("OK"))
                                }
                                _ => ("400 Bad Request", json!("bad message")),
                            }
                        }
                        Err(_) => ("400 Bad Request", json!("bad json")),
                    }
                }
                _ => ("404 Not Found", json!("not found")),
            };
            let body = reply.to_string();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}
