//! Filter v2, client side: subscribe / ping / unsubscribe requests, and decoding of pushes.

use std::time::Duration;

use futures::AsyncWriteExt;
use libp2p::{PeerId, Stream, StreamProtocol};
use libp2p_stream::Control;

use crate::{
    codec::{read_lp, write_lp},
    proto::{FilterSubscribeRequest, FilterSubscribeResponse, MessagePush},
    protocols::FILTER_SUBSCRIBE,
};

pub const SUBSCRIBE_PROTOCOL: StreamProtocol = StreamProtocol::new(FILTER_SUBSCRIBE);

/// nwaku refuses more content topics than this in one request.
pub const MAX_CONTENT_TOPICS_PER_REQUEST: usize = 100;

/// One request on a fresh stream. `Ok` only for status 200.
pub async fn request(
    control: &mut Control,
    peer: PeerId,
    req: FilterSubscribeRequest,
    timeout: Duration,
) -> Result<(), String> {
    let exchange = async {
        let mut stream = control
            .open_stream(peer, SUBSCRIBE_PROTOCOL)
            .await
            .map_err(|e| e.to_string())?;
        write_lp(&mut stream, &req).await.map_err(|e| e.to_string())?;
        let resp: FilterSubscribeResponse = read_lp(&mut stream).await.map_err(|e| e.to_string())?;
        let _ = stream.close().await;
        if resp.request_id != req.request_id {
            return Err(format!("response to another request ({})", resp.request_id));
        }
        match resp.status_code {
            200 => Ok(()),
            code => Err(format!(
                "status {code}{}",
                resp.status_desc.map(|d| format!(" ({d})")).unwrap_or_default()
            )),
        }
    };
    crate::rt::timeout(timeout, exchange)
        .await
        .unwrap_or_else(|_| Err("timed out".into()))
}

/// Reads the single push a service node writes on an inbound filter-push stream.
pub async fn read_push(mut stream: Stream, timeout: Duration) -> Result<MessagePush, String> {
    let push = crate::rt::timeout(timeout, read_lp::<_, MessagePush>(&mut stream))
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let _ = stream.close().await;
    Ok(push)
}
