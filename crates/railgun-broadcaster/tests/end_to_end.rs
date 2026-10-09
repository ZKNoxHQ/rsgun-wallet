#![cfg(not(target_arch = "wasm32"))]
//! Wallet and broadcaster talking through the in-memory Waku hub.

use std::{collections::HashMap, sync::Arc, time::Duration};

use railgun_broadcaster::{
    BroadcastRequest, BroadcasterClient, ClientError, NoQuote, PAR_RATE_WRAPPED_BASE_TOKEN,
    mock::{MockAnswer, MockBroadcaster},
    transport::{WakuMessage, WakuTransport, memory::MemoryHub},
    BrowserBridge, RemoteStatus,
};

const CHAIN: u64 = 11_155_111;
const WETH: &str = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14";

fn request(client: &BroadcasterClient) -> BroadcastRequest {
    let quote = client.quotes_for(WETH, &[]).remove(0);
    BroadcastRequest {
        quote,
        to: "0xeCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea".into(),
        calldata: vec![0xd8, 0xae, 0x13, 0x6a, 1, 2, 3],
        min_gas_price: 1_500_000_000,
        use_relay_adapt: false,
        pre_transaction_pois: HashMap::new(),
    }
}

#[tokio::test]
async fn quotes_are_authenticated_and_ranked() {
    let hub = MemoryHub::new();
    let client = BroadcasterClient::new(Arc::new(hub.handle()), CHAIN);
    let cheap = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    let pricey = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 9);

    pricey.announce(&[(WETH, 900)], "p1", 120_000).await;
    cheap.announce(&[(WETH, 400)], "c1", 120_000).await;
    // Signed with a key that is not the address's viewing key.
    cheap
        .announce_as(&[(WETH, 1)], "forged", 120_000, "8.2.0", &[0x55; 32])
        .await;
    // Out of the supported version range.
    pricey
        .announce_as(&[(WETH, 2)], "old", 120_000, "7.0.0", &[9; 32])
        .await;
    // Expires too soon to prove and submit.
    let hasty = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 11);
    hasty.announce(&[(WETH, 3)], "h1", 10_000).await;

    assert_eq!(client.pump().await.unwrap(), 3);
    let quotes = client.quotes_for(&WETH.to_lowercase(), &[]);
    assert_eq!(
        quotes.iter().map(|q| q.fees_id.as_str()).collect::<Vec<_>>(),
        vec!["c1", "p1"]
    );
    assert_eq!(quotes[0].railgun_address, cheap.railgun_address);
}

#[tokio::test]
async fn request_reaches_only_its_broadcaster_and_the_hash_comes_back() {
    let hub = MemoryHub::new();
    let client = Arc::new(BroadcasterClient::new(Arc::new(hub.handle()), CHAIN));
    let chosen = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    let other = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 9);

    chosen.announce(&[(WETH, 400)], "c1", 120_000).await;
    other.announce(&[(WETH, 900)], "o1", 120_000).await;
    client.pump().await.unwrap();

    let sealed = client.seal(request(&client), &mut rand::rng()).unwrap();
    let sender = {
        let client = client.clone();
        tokio::spawn(async move { client.send_with_timeout(&sealed, Duration::from_secs(10)).await })
    };

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(other.serve(|_| MockAnswer::TxHash("0xbad".into())).await.is_empty());
    let served = chosen.serve(|_| MockAnswer::TxHash("0xfeed".into())).await;

    assert_eq!(served.len(), 1);
    assert_eq!(served[0].fees_id, "c1");
    assert_eq!(served[0].min_gas_price, "1500000000");
    assert_eq!(served[0].data, "0xd8ae136a010203");
    assert_eq!(served[0].transact_type, "COMMON");
    assert_eq!(served[0].chain_id, CHAIN);
    assert_eq!(sender.await.unwrap().unwrap(), "0xfeed");
}

#[tokio::test]
async fn refusal_and_timeout_are_distinct() {
    let hub = MemoryHub::new();
    let client = Arc::new(BroadcasterClient::new(Arc::new(hub.handle()), CHAIN));
    let broadcaster = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    broadcaster.announce(&[(WETH, 400)], "c1", 120_000).await;
    client.pump().await.unwrap();

    let sealed = client.seal(request(&client), &mut rand::rng()).unwrap();
    let sender = {
        let client = client.clone();
        tokio::spawn(async move { client.send_with_timeout(&sealed, Duration::from_secs(10)).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    broadcaster
        .serve(|_| MockAnswer::Error("Bad token fee.".into()))
        .await;
    assert!(matches!(
        sender.await.unwrap(),
        Err(ClientError::Refused(reason)) if reason == "Bad token fee."
    ));

    let sealed = client.seal(request(&client), &mut rand::rng()).unwrap();
    assert!(matches!(
        client.send_with_timeout(&sealed, Duration::from_millis(1200)).await,
        Err(ClientError::Timeout(_))
    ));
}

#[tokio::test]
async fn trusted_signer_caps_what_others_can_charge() {
    let hub = MemoryHub::new();
    let authority = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 21);
    // Configured in chain-agnostic form, announced in chain-scoped form: same signer.
    let client = BroadcasterClient::with_trusted_signers(
        Arc::new(hub.handle()),
        CHAIN,
        &[authority.railgun_address_all_chains.clone()],
    )
    .unwrap();
    assert_eq!(client.trusted_signer_count(), 1);

    let fair = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    let cheap = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 9);
    let gouger = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 11);
    let dumper = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 13);

    // Nothing is authorized yet: every offer is refused, however reasonable.
    fair.announce(&[(WETH, 1_000)], "early", 120_000).await;
    client.pump().await.unwrap();
    assert!(client.quotes_for(WETH, &[]).is_empty());
    assert!(client.authorized_fees().is_empty());

    // Authorized rate 1000: band is 900..=1300.
    authority.announce(&[(WETH, 1_000)], "auth", 120_000).await;
    fair.announce(&[(WETH, 1_200)], "fair", 120_000).await;
    cheap.announce(&[(WETH, 900)], "cheap", 120_000).await;
    gouger.announce(&[(WETH, 1_301)], "gouge", 120_000).await;
    dumper.announce(&[(WETH, 899)], "dump", 120_000).await;
    client.pump().await.unwrap();

    assert_eq!(
        client.authorized_fees(),
        vec![(WETH.to_lowercase(), 1_000)]
    );
    let ids: Vec<String> = client
        .quotes_for(WETH, &[])
        .into_iter()
        .map(|q| q.fees_id)
        .collect();
    assert_eq!(ids, vec!["cheap", "auth", "fair"]);

    // The authorized rate drops to 700 (band 630..=910): offers accepted earlier are re-checked.
    authority.announce(&[(WETH, 700)], "auth2", 130_000).await;
    client.pump().await.unwrap();
    let ids: Vec<String> = client
        .quotes_for(WETH, &[])
        .into_iter()
        .map(|q| q.fees_id)
        .collect();
    assert_eq!(ids, vec!["auth2", "cheap"]);
}

#[tokio::test]
async fn several_trusted_signers_are_averaged_and_an_impostor_is_not_trusted() {
    let hub = MemoryHub::new();
    let a = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 21);
    let b = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 23);
    let client = BroadcasterClient::with_trusted_signers(
        Arc::new(hub.handle()),
        CHAIN,
        &[a.railgun_address.clone(), b.railgun_address.clone()],
    )
    .unwrap();

    // Claims a's address but signs with another key: dropped at authentication, moves nothing.
    a.announce_as(&[(WETH, 5)], "forged", 120_000, "8.2.0", &[0x77; 32])
        .await;
    a.announce(&[(WETH, 1_000)], "a", 120_000).await;
    b.announce(&[(WETH, 2_000)], "b", 120_000).await;
    // An announcement about to expire does not count towards the authorized rate.
    b.announce(&[(WETH, 9_000)], "b-late", 5_000).await;
    client.pump().await.unwrap();

    assert_eq!(client.authorized_fees(), vec![(WETH.to_lowercase(), 1_500)]);
    // Band 1350..=1950: a (1000) and b (2000) are themselves outside of it at selection time,
    // exactly as in the reference client.
    assert!(client.quotes_for(WETH, &[]).is_empty());

    let mid = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    mid.announce(&[(WETH, 1_500)], "mid", 120_000).await;
    client.pump().await.unwrap();
    assert_eq!(client.quotes_for(WETH, &[])[0].fees_id, "mid");
}

#[tokio::test]
async fn rate_ceiling_needs_no_trusted_signer() {
    let hub = MemoryHub::new();
    let client = BroadcasterClient::new(Arc::new(hub.handle()), CHAIN);
    let par = PAR_RATE_WRAPPED_BASE_TOKEN;
    let ceiling = Some(par * 3 / 2);

    assert!(matches!(client.best_quote(WETH, &[], ceiling), Err(NoQuote::None)));

    // A lone broadcaster asking 40 times the gas cost: accepted without a ceiling, refused with.
    let gouger = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 11);
    gouger.announce(&[(WETH, par * 40)], "gouge", 120_000).await;
    client.pump().await.unwrap();
    assert_eq!(client.best_quote(WETH, &[], None).unwrap().fees_id, "gouge");
    match client.best_quote(WETH, &[], ceiling) {
        Err(NoQuote::AboveCeiling { cheapest, rejected, .. }) => {
            assert_eq!(cheapest, par * 40);
            assert_eq!(rejected, 1);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // The ceiling is inclusive, and the cheapest offer under it wins.
    let at_limit = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    let fair = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 9);
    at_limit.announce(&[(WETH, par * 3 / 2)], "limit", 120_000).await;
    client.pump().await.unwrap();
    assert_eq!(client.best_quote(WETH, &[], ceiling).unwrap().fees_id, "limit");
    fair.announce(&[(WETH, par * 11 / 10)], "fair", 120_000).await;
    client.pump().await.unwrap();
    assert_eq!(client.best_quote(WETH, &[], ceiling).unwrap().fees_id, "fair");
}

/// The wallet tab, reduced to its role: shuttle messages between the bridge and the network.
async fn tab_round(bridge: &BrowserBridge, network: &dyn WakuTransport, connected: bool) {
    let received: Vec<WakuMessage> = network.poll().await.unwrap();
    let status = RemoteStatus { connected, peers: 3, detail: None };
    for out in bridge.exchange(status, received, Vec::new()) {
        network.publish(&out.content_topic, &out.payload).await.unwrap();
    }
}

#[tokio::test]
async fn browser_bridge_carries_offers_requests_and_answers() {
    let hub = MemoryHub::new();
    let network = hub.handle();
    let bridge = Arc::new(BrowserBridge::new());
    let client = Arc::new(BroadcasterClient::new(bridge.clone(), CHAIN));
    let broadcaster = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);

    // No tab yet, then a tab that is still connecting: unreachable, with the reason.
    let err = client.pump().await.unwrap_err().to_string();
    assert!(err.contains("no wallet tab"), "{err}");
    tab_round(&bridge, &network, false).await;
    let err = client.pump().await.unwrap_err().to_string();
    assert!(err.contains("not connected yet"), "{err}");

    broadcaster.announce(&[(WETH, 400)], "c1", 120_000).await;
    tab_round(&bridge, &network, true).await;
    assert_eq!(client.pump().await.unwrap(), 1);
    assert_eq!(client.peer_count().await, Some(3));

    let sealed = client.seal(request(&client), &mut rand::rng()).unwrap();
    let sender = {
        let client = client.clone();
        tokio::spawn(async move { client.send_with_timeout(&sealed, Duration::from_secs(10)).await })
    };
    let mut served = 0;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tab_round(&bridge, &network, true).await;
        served += broadcaster.serve(|_| MockAnswer::TxHash("0xbeef".into())).await.len();
        if sender.is_finished() {
            break;
        }
    }
    assert!(served >= 1);
    assert_eq!(sender.await.unwrap().unwrap(), "0xbeef");
}

/// Real broadcasters require the active POI list. Regression: the wallet once passed its list
/// keys in a debug form, matched nothing, and reported an empty market.
#[tokio::test]
async fn required_poi_lists_must_be_ours() {
    const LIST: &str = "efc6ddb59c098a13fb2b618fdae94c1c3a807abc8fb1837c93620c9143ee9e88";
    let hub = MemoryHub::new();
    let client = BroadcasterClient::new(Arc::new(hub.handle()), CHAIN);
    let mut broadcaster = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    broadcaster.required_poi_list_keys = vec![LIST.to_string()];
    broadcaster.announce(&[(WETH, 400)], "c1", 120_000).await;
    client.pump().await.unwrap();

    assert_eq!(client.best_quote(WETH, &[LIST.to_string()], None).unwrap().fees_id, "c1");
    match client.best_quote(WETH, &[format!("ListKey({LIST})")], None) {
        Err(NoQuote::PoiListMismatch { offers, required, .. }) => {
            assert_eq!(offers, 1);
            assert_eq!(required, vec![LIST.to_string()]);
        }
        other => panic!("expected PoiListMismatch, got {other:?}"),
    }
    assert!(matches!(
        client.best_quote(WETH, &[], None),
        Err(NoQuote::PoiListMismatch { .. })
    ));
}

#[tokio::test]
async fn selection_spreads_over_near_cheapest_and_skips_the_silent_ones() {
    let hub = MemoryHub::new();
    let client = BroadcasterClient::new(Arc::new(hub.handle()), CHAIN);
    let a = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 7);
    let b = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 9);
    let far = MockBroadcaster::new(Arc::new(hub.handle()), CHAIN, 11);
    a.announce(&[(WETH, 860)], "a", 120_000).await;
    b.announce(&[(WETH, 920)], "b", 120_000).await;
    far.announce(&[(WETH, 1_400)], "far", 120_000).await;
    client.pump().await.unwrap();

    // Within 10% of 860 (946): a and b, never the expensive one, whatever the draw.
    let ids = |pick: usize, exclude: &[String]| {
        client
            .select_quote(WETH, &[], None, 10, exclude, |n| {
                assert!(n <= 2);
                pick
            })
            .unwrap()
            .fees_id
    };
    assert_eq!(ids(0, &[]), "a");
    assert_eq!(ids(1, &[]), "b");
    assert_eq!(ids(99, &[]), "b"); // an out-of-range draw is clamped

    // a just timed out: only b is drawn. With everybody excluded, exclusions are ignored.
    assert_eq!(ids(0, &[a.railgun_address.clone()]), "b");
    let all = vec![a.railgun_address.clone(), b.railgun_address.clone(), far.railgun_address.clone()];
    assert_eq!(ids(0, &all), "a");
}

#[tokio::test]
async fn bridge_reports_what_happened_to_publishes() {
    use railgun_broadcaster::PublishAck;
    let bridge = BrowserBridge::new();
    let up = RemoteStatus { connected: true, peers: 2, detail: None };
    bridge.exchange(up.clone(), Vec::new(), Vec::new());
    bridge.publish("/t", b"one").await.unwrap();
    bridge.publish("/t", b"two").await.unwrap();

    let out = bridge.exchange(up.clone(), Vec::new(), Vec::new());
    assert_eq!(out.iter().map(|o| o.id).collect::<Vec<_>>(), vec![1, 2]);
    bridge.exchange(
        up,
        Vec::new(),
        vec![
            PublishAck { id: 1, peers: 2, error: None },
            PublishAck { id: 2, peers: 0, error: Some("light push refused by every peer".into()) },
        ],
    );
    let stats = bridge.publish_stats();
    assert_eq!((stats.queued, stats.delivered, stats.failed), (2, 1, 1));
    assert_eq!(stats.last_error.as_deref(), Some("light push refused by every peer"));
}
