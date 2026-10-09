//! Metadata v1: cluster and shards, exchanged both ways. nwaku queries every new peer and
//! disconnects the ones that fail or sit on another cluster.

use std::time::Duration;

use futures::AsyncWriteExt;
use libp2p::{PeerId, Stream, StreamProtocol};
use libp2p_stream::Control;

use crate::{
    codec::{read_lp, write_lp},
    proto::{WakuMetadataRequest, WakuMetadataResponse},
    protocols::METADATA,
};

pub const PROTOCOL: StreamProtocol = StreamProtocol::new(METADATA);

/// Answers one inbound query with our cluster and shards; returns the remote's cluster.
pub async fn serve(
    mut stream: Stream,
    cluster_id: u32,
    shards: &[u32],
    timeout: Duration,
) -> Result<Option<u32>, String> {
    let exchange = async {
        let req: WakuMetadataRequest = read_lp(&mut stream).await.map_err(|e| e.to_string())?;
        let resp = WakuMetadataResponse { cluster_id: Some(cluster_id), shards: shards.to_vec() };
        write_lp(&mut stream, &resp).await.map_err(|e| e.to_string())?;
        let _ = stream.close().await;
        Ok(req.cluster_id)
    };
    crate::rt::timeout(timeout, exchange)
        .await
        .unwrap_or_else(|_| Err("timed out".into()))
}

/// Asks a peer for its cluster.
pub async fn query(
    control: &mut Control,
    peer: PeerId,
    cluster_id: u32,
    shards: &[u32],
    timeout: Duration,
) -> Result<Option<u32>, String> {
    let exchange = async {
        let mut stream = control.open_stream(peer, PROTOCOL).await.map_err(|e| e.to_string())?;
        let req = WakuMetadataRequest { cluster_id: Some(cluster_id), shards: shards.to_vec() };
        write_lp(&mut stream, &req).await.map_err(|e| e.to_string())?;
        let resp: WakuMetadataResponse = read_lp(&mut stream).await.map_err(|e| e.to_string())?;
        let _ = stream.close().await;
        Ok(resp.cluster_id)
    };
    crate::rt::timeout(timeout, exchange)
        .await
        .unwrap_or_else(|_| Err("timed out".into()))
}
