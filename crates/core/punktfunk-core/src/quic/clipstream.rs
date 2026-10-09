//! Per-transfer clipboard fetch streams (`design/clipboard-and-file-transfer.md`).
//!
//! Bulk clipboard / file bytes never ride the control stream or datagrams (lossy,
//! single-packet). The requester opens a fresh QUIC bi-stream of type transfer and writes a
//! [`ClipFetch`] frame; the holder replies with a [`ClipFetchHdr`] frame then raw chunks until
//! FIN. One transfer per stream: flow control, `RESET_STREAM` cancel, no head-of-line blocking
//! against control or other transfers.
//!
//! Transport only — no clipboard state. Host and client-core share these helpers;
//! each side runs its own accept loop because they own the connection differently.

use super::v2::{io as v2io, msg::V2Message, registry};
use super::{ClipFetch, ClipFetchHdr};

/// Stream-reset / stop code for a cancelled fetch. Distinct from connection close
/// codes (`0x51`/`0x52` quit/exit, `0x42` reject, `0x60`–`0x67` pairing) so a
/// captured code is unambiguous even though QUIC already namespaces them.
pub const CLIP_CANCELLED_CODE: u32 = 0x70;

/// 64 KiB write size; matches the control-frame bound.
pub const CLIP_CHUNK: usize = 64 * 1024;

pub fn cancelled_code() -> quinn::VarInt {
    quinn::VarInt::from_u32(CLIP_CANCELLED_CODE)
}

/// Send is for `reset`/`finish`; recv sits at [`ClipFetchHdr`] ([`read_fetch_hdr`]).
pub async fn open_fetch(
    conn: &quinn::Connection,
    req: &ClipFetch,
) -> std::io::Result<(quinn::SendStream, quinn::RecvStream)> {
    let (mut send, recv) = conn.open_bi().await.map_err(std::io::Error::other)?;
    // -1 yields to the control stream (default 0). A large paste must not
    // head-of-line-block input/audio/control on this connection.
    let _ = send.set_priority(-1);
    // quinn: the opener must write before the peer's `accept_bi()` can return.
    v2io::write_stream_type(&mut send, registry::STREAM_TRANSFER).await?;
    v2io::write_frame(&mut send, &req.encode_v2()).await?;
    Ok((send, recv))
}

/// A stream of another type is an error; the caller `stop`s the stream.
pub async fn read_stream_header(recv: &mut quinn::RecvStream) -> std::io::Result<()> {
    match v2io::read_stream_type(recv).await? {
        registry::STREAM_TRANSFER => Ok(()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a transfer stream",
        )),
    }
}

pub async fn read_fetch(recv: &mut quinn::RecvStream) -> std::io::Result<ClipFetch> {
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad ClipFetch");
    let (ty, body) = v2io::read_one_frame(recv).await?;
    super::v2::msg::decode::<ClipFetch>(ty, &body).map_err(|_| bad())
}

/// Header must precede any data chunks.
pub async fn write_fetch_hdr(
    send: &mut quinn::SendStream,
    hdr: &ClipFetchHdr,
) -> std::io::Result<()> {
    v2io::write_frame(send, &hdr.encode_v2()).await
}

/// Call only after [`super::CLIP_FETCH_OK`]. FIN so [`read_data`] terminates.
pub async fn write_data(send: &mut quinn::SendStream, data: &[u8]) -> std::io::Result<()> {
    for chunk in data.chunks(CLIP_CHUNK) {
        send.write_all(chunk).await.map_err(std::io::Error::other)?;
    }
    send.finish().map_err(std::io::Error::other)?;
    Ok(())
}

/// One frame and no further: the data follows it on the stream.
pub async fn read_fetch_hdr(recv: &mut quinn::RecvStream) -> std::io::Result<ClipFetchHdr> {
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad ClipFetchHdr");
    let (ty, body) = v2io::read_one_frame(recv).await?;
    super::v2::msg::decode::<ClipFetchHdr>(ty, &body).map_err(|_| bad())
}

/// `max_bytes` is the requester size cap. A breach errors; the caller resets the stream.
pub async fn read_data(recv: &mut quinn::RecvStream, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    recv.read_to_end(max_bytes)
        .await
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use crate::quic::clipstream;
    use crate::quic::test_util::connect_pair;
    use crate::quic::*;

    #[tokio::test]
    async fn fetch_text_transfers_then_cancel_resets() {
        let (_server_ep, _client_ep, host_conn, client_conn) = connect_pair().await;
        fetch_then_cancel(host_conn, client_conn).await;
    }

    async fn fetch_then_cancel(host_conn: quinn::Connection, client_conn: quinn::Connection) {
        let payload = b"hello clipboard \xf0\x9f\x93\x8b".to_vec();
        let holder_payload = payload.clone();

        let holder = tokio::spawn(async move {
            let (mut send, mut recv) = host_conn.accept_bi().await.expect("accept fetch #1");
            clipstream::read_stream_header(&mut recv)
                .await
                .expect("stream header #1");
            let req = clipstream::read_fetch(&mut recv)
                .await
                .expect("fetch req #1");
            assert_eq!(req.seq, 1);
            assert_eq!(req.file_index, CLIP_FILE_INDEX_NONE);
            assert_eq!(req.mime, "text/plain;charset=utf-8");
            clipstream::write_fetch_hdr(
                &mut send,
                &ClipFetchHdr {
                    status: CLIP_FETCH_OK,
                    total_size: holder_payload.len() as u64,
                },
            )
            .await
            .expect("write hdr #1");
            clipstream::write_data(&mut send, &holder_payload)
                .await
                .expect("write data #1");

            let (mut send2, mut recv2) = host_conn.accept_bi().await.expect("accept fetch #2");
            clipstream::read_stream_header(&mut recv2)
                .await
                .expect("stream header #2");
            let _ = clipstream::read_fetch(&mut recv2)
                .await
                .expect("fetch req #2");
            send2.reset(clipstream::cancelled_code()).unwrap();

            host_conn // drop would close the pair before the requester finishes
        });

        let req = ClipFetch {
            seq: 1,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: "text/plain;charset=utf-8".into(),
        };
        let (_send, mut recv) = clipstream::open_fetch(&client_conn, &req)
            .await
            .expect("open fetch #1");
        let hdr = clipstream::read_fetch_hdr(&mut recv)
            .await
            .expect("read hdr #1");
        assert_eq!(hdr.status, CLIP_FETCH_OK);
        assert_eq!(hdr.total_size as usize, payload.len());
        let got = clipstream::read_data(&mut recv, 8 << 20)
            .await
            .expect("read data #1");
        assert_eq!(got, payload);

        let req2 = ClipFetch {
            seq: 2,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: "text/plain;charset=utf-8".into(),
        };
        let (_send2, mut recv2) = clipstream::open_fetch(&client_conn, &req2)
            .await
            .expect("open fetch #2");
        assert!(
            clipstream::read_fetch_hdr(&mut recv2).await.is_err(),
            "a cancelled fetch must surface as an error, not a hang"
        );

        let _host_conn = holder.await.unwrap();
    }

    #[tokio::test]
    async fn read_data_enforces_size_cap() {
        let (_server_ep, _client_ep, host_conn, client_conn) = connect_pair().await;

        let big = vec![0xABu8; 200_000]; // above CLIP_CHUNK and the 64 KiB cap below
        let holder_payload = big.clone();
        let holder = tokio::spawn(async move {
            let (mut send, mut recv) = host_conn.accept_bi().await.expect("accept");
            clipstream::read_stream_header(&mut recv).await.unwrap();
            let _ = clipstream::read_fetch(&mut recv).await.unwrap();
            clipstream::write_fetch_hdr(
                &mut send,
                &ClipFetchHdr {
                    status: CLIP_FETCH_OK,
                    total_size: holder_payload.len() as u64,
                },
            )
            .await
            .unwrap();
            let _ = clipstream::write_data(&mut send, &holder_payload).await;
            host_conn
        });

        let req = ClipFetch {
            seq: 1,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: "application/octet-stream".into(),
        };
        let (_send, mut recv) = clipstream::open_fetch(&client_conn, &req).await.unwrap();
        assert_eq!(
            clipstream::read_fetch_hdr(&mut recv).await.unwrap().status,
            CLIP_FETCH_OK
        );
        assert!(clipstream::read_data(&mut recv, 64 * 1024).await.is_err());

        let _host_conn = holder.await.unwrap();
    }
}
