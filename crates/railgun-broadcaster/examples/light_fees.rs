//! `BroadcasterClient` on the native light node, against the Railgun fleet. Passive: listens to
//! fee announcements, authenticates them and prints the offers. Publishes nothing.
//!
//! cargo run --release -p railgun-broadcaster --features light-node --example light_fees -- [chain id, default 11155111] [seconds, default 90] [trusted | open, default trusted]

use std::{sync::Arc, time::Duration};

use railgun_broadcaster::{BroadcasterClient, LightNodeTransport, RAILWAY_TRUSTED_FEE_SIGNERS};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "waku_light=info,railgun_broadcaster=info".into()))
        .init();
    let mut args = std::env::args().skip(1);
    let chain: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(11_155_111);
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(90);
    let trusted = args.next().as_deref() != Some("open");

    let transport = Arc::new(LightNodeTransport::for_chain(chain));
    let client = if trusted {
        let signers: Vec<String> = RAILWAY_TRUSTED_FEE_SIGNERS.iter().map(|s| s.to_string()).collect();
        BroadcasterClient::with_trusted_signers(transport.clone(), chain, &signers).expect("signers")
    } else {
        BroadcasterClient::new(transport.clone(), chain)
    };

    // Same loop shape as the wallet's fee monitor: subscribe until it holds, then pump.
    let start = std::time::Instant::now();
    let mut subscribed = false;
    let mut accepted_total = 0usize;
    while start.elapsed() < Duration::from_secs(secs) {
        let result = async {
            if !subscribed {
                client.subscribe().await?;
            }
            client.pump().await
        }
        .await;
        match result {
            Ok(n) => {
                subscribed = true;
                accepted_total += n;
            }
            Err(e) => {
                subscribed = false;
                println!("[{:>3}s] {e}", start.elapsed().as_secs());
            }
        }
        if subscribed && start.elapsed().as_secs() % 15 < 3 {
            println!(
                "[{:>3}s] peers {:?}, {accepted_total} announcement(s) accepted, {} trusted signer(s), authorized {:?}",
                start.elapsed().as_secs(),
                client.peer_count().await,
                client.trusted_signer_count(),
                client.authorized_fees()
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let quotes = client.all_quotes();
    println!("\n{} live offer(s) on chain {chain}:", quotes.len());
    for q in &quotes {
        println!(
            "  {}…{} {:<12} token {} rate {} wallets {} reliability {} v{}",
            &q.railgun_address[..12],
            &q.railgun_address[q.railgun_address.len() - 4..],
            q.identifier.as_deref().unwrap_or("-"),
            q.token,
            q.fee_per_unit_gas,
            q.available_wallets,
            q.reliability,
            q.version
        );
    }
    println!("status {:?}", transport.status());
}
