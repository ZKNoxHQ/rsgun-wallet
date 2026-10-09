//! Framing of every Waku req/resp protocol: one unsigned-varint length prefix, then the protobuf
//! (`writeLp` / `readLp` in nim-libp2p and it-length-prefixed in js-libp2p).

use std::io;

use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use prost::Message;

/// Upper bound on one frame. nwaku caps a Waku message at 150 KiB and an RPC at a few MiB.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

pub async fn write_lp<W: AsyncWrite + Unpin, M: Message>(io: &mut W, msg: &M) -> io::Result<()> {
    let body = msg.encode_to_vec();
    let mut frame = Vec::with_capacity(body.len() + 10);
    prost::encoding::encode_varint(body.len() as u64, &mut frame);
    frame.extend_from_slice(&body);
    io.write_all(&frame).await?;
    io.flush().await
}

pub async fn read_lp<R: AsyncRead + Unpin, M: Message + Default>(io: &mut R) -> io::Result<M> {
    let len = read_uvarint(io).await?;
    if len > MAX_FRAME as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes")));
    }
    let mut buf = vec![0u8; len as usize];
    io.read_exact(&mut buf).await?;
    M::decode(buf.as_slice()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn read_uvarint<R: AsyncRead + Unpin>(io: &mut R) -> io::Result<u64> {
    let mut value = 0u64;
    for i in 0..10 {
        let mut byte = [0u8; 1];
        io.read_exact(&mut byte).await?;
        value |= u64::from(byte[0] & 0x7f) << (7 * i);
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "varint too long"))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::proto::{WakuMessage, WakuMetadataRequest};

    #[tokio::test]
    async fn roundtrip() {
        let msg = WakuMessage {
            payload: vec![7; 300],
            content_topic: "/railgun/v2/0-1-fees/json".into(),
            version: Some(0),
            timestamp: Some(1_758_000_000_000_000_000),
            ..Default::default()
        };
        let mut buf = Vec::new();
        write_lp(&mut buf, &msg).await.unwrap();
        // 300+ byte body: two-byte length prefix.
        assert!(buf[0] & 0x80 != 0 && buf[1] & 0x80 == 0);
        let back: WakuMessage = read_lp(&mut buf.as_slice()).await.unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn metadata_shards_unpacked() {
        let req = WakuMetadataRequest { cluster_id: Some(5), shards: vec![1] };
        // field 1 varint 5, field 2 varint 1 (unpacked: key 0x10, not 0x12 len-delimited)
        assert_eq!(req.encode_to_vec(), vec![0x08, 5, 0x10, 1]);
    }

    #[test]
    fn timestamp_is_zigzag() {
        let m = WakuMessage { timestamp: Some(1), ..Default::default() };
        // key (10<<3)|0 = 0x50, zigzag(1) = 2
        assert_eq!(m.encode_to_vec(), vec![0x50, 2]);
    }
}
