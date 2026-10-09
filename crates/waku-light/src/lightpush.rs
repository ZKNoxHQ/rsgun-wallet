//! Light push, client side: v3 (`LightpushRequest` / status codes) with a v2 fallback (`PushRpc`).

use std::time::Duration;

use futures::AsyncWriteExt;
use libp2p::{PeerId, StreamProtocol};
use libp2p_stream::{Control, OpenStreamError};

use crate::{
    codec::{read_lp, write_lp},
    proto::{LightpushRequest, LightpushResponse, PushRequest, PushRpc, WakuMessage},
    protocols::{LIGHTPUSH_V2, LIGHTPUSH_V3},
};

pub const V2: StreamProtocol = StreamProtocol::new(LIGHTPUSH_V2);
pub const V3: StreamProtocol = StreamProtocol::new(LIGHTPUSH_V3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V2,
    V3,
}

pub enum PushError {
    /// The peer does not speak this version: try the other one.
    Unsupported,
    Failed(String),
}

pub async fn push(
    control: &mut Control,
    peer: PeerId,
    version: Version,
    request_id: String,
    pubsub_topic: &str,
    message: WakuMessage,
    timeout: Duration,
) -> Result<(), PushError> {
    let exchange = async {
        let protocol = match version {
            Version::V2 => V2,
            Version::V3 => V3,
        };
        let mut stream = match control.open_stream(peer, protocol).await {
            Ok(s) => s,
            Err(OpenStreamError::UnsupportedProtocol(_)) => return Err(PushError::Unsupported),
            Err(e) => return Err(PushError::Failed(e.to_string())),
        };
        let io = |e: std::io::Error| PushError::Failed(e.to_string());
        let outcome = match version {
            Version::V3 => {
                let req = LightpushRequest {
                    request_id: request_id.clone(),
                    pubsub_topic: Some(pubsub_topic.to_string()),
                    message: Some(message),
                };
                write_lp(&mut stream, &req).await.map_err(io)?;
                let resp: LightpushResponse = read_lp(&mut stream).await.map_err(io)?;
                match resp.status_code {
                    200 => Ok(()),
                    code => Err(PushError::Failed(format!(
                        "status {code}{}",
                        resp.status_desc.map(|d| format!(" ({d})")).unwrap_or_default()
                    ))),
                }
            }
            Version::V2 => {
                let req = PushRpc {
                    request_id: request_id.clone(),
                    request: Some(PushRequest {
                        pubsub_topic: pubsub_topic.to_string(),
                        message: Some(message),
                    }),
                    response: None,
                };
                write_lp(&mut stream, &req).await.map_err(io)?;
                let rpc: PushRpc = read_lp(&mut stream).await.map_err(io)?;
                match rpc.response {
                    Some(r) if r.is_success => Ok(()),
                    Some(r) => Err(PushError::Failed(
                        r.info.unwrap_or_else(|| "refused without reason".into()),
                    )),
                    None => Err(PushError::Failed("answer without response".into())),
                }
            }
        };
        let _ = stream.close().await;
        outcome
    };
    crate::rt::timeout(timeout, exchange)
        .await
        .unwrap_or_else(|_| Err(PushError::Failed("timed out".into())))
}
