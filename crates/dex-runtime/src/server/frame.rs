//! Length-prefixed frame reading and writing over a byte stream.
//!
//! The IPC path is binary, not JSON: a `u32` little-endian length followed by a
//! `postcard` payload. The prefix is what lets a reader distinguish a frame from
//! a partial one, so a frame split across TCP segments is reassembled rather
//! than mis-parsed.

use std::io;
use std::path::Path;

use dex_protocol::{MAX_FRAME_BYTES, ServerResponse, decode_payload, encode_frame, split_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Reads request frames from a stream.
pub struct FrameReader<R> {
    inner: R,
    buffer: Vec<u8>,
}

/// What one read produced.
#[derive(Debug)]
pub enum Frame<T> {
    /// A complete value.
    Complete(T),
    /// The stream ended cleanly between frames.
    Closed,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            // Sized for a typical frame so the common path does not reallocate.
            buffer: Vec::with_capacity(8 * 1024),
        }
    }

    /// Read the next response, or `Closed` at end of stream.
    ///
    /// A convenience over [`FrameReader::next`] for the common case, so a
    /// caller holding this type does not have to spell the type parameter.
    pub async fn next_frame(&mut self) -> Result<Frame<ServerResponse>, io::Error> {
        self.next().await
    }

    /// Read the next frame, or `Closed` at end of stream.
    pub async fn next<T: serde::de::DeserializeOwned>(&mut self) -> Result<Frame<T>, io::Error> {
        loop {
            match split_frame(&self.buffer) {
                Ok(Some((header, payload))) => {
                    let value = decode_payload::<T>(payload).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                    })?;
                    // Consume exactly this frame, leaving any that followed it
                    // in the buffer for the next call.
                    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]])
                        as usize;
                    self.buffer.drain(..4 + len);
                    return Ok(Frame::Complete(value));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            }

            // No complete frame yet. Guard against a hostile length prefix
            // making this allocate without bound.
            if self.buffer.len() > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame exceeded the size limit",
                ));
            }

            let mut chunk = [0u8; 16 * 1024];
            let read = self.inner.read(&mut chunk).await?;
            if read == 0 {
                return Ok(Frame::Closed);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// Writes response frames to a stream.
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub async fn send(&mut self, response: &ServerResponse) -> io::Result<()> {
        let framed = encode_frame(response)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.inner.write_all(&framed).await?;
        self.inner.flush().await
    }

    /// Write bytes that are already framed.
    pub async fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes).await?;
        self.inner.flush().await
    }

    /// Close the write side, ending the connection from this end.
    pub async fn close(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}

/// Read one frame from an already-buffered slice. Used by tests.
pub fn read_frame(bytes: &[u8]) -> Result<Option<ServerResponse>, io::Error> {
    let Some((_, payload)) =
        split_frame(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
    else {
        return Ok(None);
    };
    decode_payload::<ServerResponse>(payload)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Build the frame bytes for a response, for tests and for the CLI.
pub fn frame_bytes(response: &ServerResponse) -> io::Result<Vec<u8>> {
    encode_frame(response).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Confirm a socket path is usable before binding.
///
/// `sun_path` is a fixed-size buffer, so an over-long path fails at bind with an
/// opaque error. Checking here turns that into a clear message.
pub fn check_socket_path(path: &Path) -> io::Result<()> {
    let text = path.display().to_string();
    // 108 on Linux, including the NUL terminator.
    if text.len() > 100 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path is {} bytes; a Unix socket path must be under 100",
                text.len()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_protocol::{Ack, Event, EventFrame, RequestFrame, ServerResponse, SessionId};

    fn responses() -> Vec<ServerResponse> {
        vec![
            ServerResponse::ack(dex_protocol::RequestId(1), Ack::Ok),
            ServerResponse::ack(
                dex_protocol::RequestId(2),
                Ack::CreateSession {
                    session_id: SessionId::new(),
                    status: dex_protocol::SessionStatus::Created,
                    model: "m".into(),
                },
            ),
            ServerResponse::ack(dex_protocol::RequestId(3), Ack::Accepted),
            ServerResponse::Event(EventFrame::new(
                SessionId::new(),
                42,
                Event::ModelDelta {
                    text: "a somewhat longer chunk of streamed text".into(),
                },
            )),
        ]
    }

    #[tokio::test]
    async fn frames_survive_a_write_and_read() {
        // Built once: a fresh `SessionId::new()` per call would not match what
        // was written.
        let expected = responses();

        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let mut writer = FrameWriter::new(&mut client);
        for response in &expected {
            writer.send(response).await.expect("send");
        }
        writer.shutdown().await.expect("shutdown");

        let mut reader = FrameReader::new(&mut server);
        for expected in expected {
            match reader.next::<ServerResponse>().await.expect("read") {
                Frame::Complete(got) => assert_eq!(got, expected),
                Frame::Closed => panic!("closed early"),
            }
        }
        assert!(matches!(
            reader.next::<ServerResponse>().await.expect("read"),
            Frame::Closed
        ));
    }

    #[tokio::test]
    async fn a_frame_split_across_writes_is_reassembled() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let framed = frame_bytes(&ServerResponse::ack(
            dex_protocol::RequestId(9),
            Ack::Accepted,
        ))
        .expect("frame");

        // Write one byte at a time, the worst a socket can do.
        for byte in &framed {
            client.write_all(&[*byte]).await.expect("write");
        }
        client.flush().await.expect("flush");

        let mut reader = FrameReader::new(&mut server);
        match reader.next::<ServerResponse>().await.expect("read") {
            Frame::Complete(got) => assert_eq!(
                got,
                ServerResponse::ack(dex_protocol::RequestId(9), Ack::Accepted)
            ),
            Frame::Closed => panic!("closed early"),
        }
    }

    #[tokio::test]
    async fn two_frames_written_together_are_separated() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let mut bytes = frame_bytes(&ServerResponse::ack(dex_protocol::RequestId(1), Ack::Ok))
            .expect("a");
        bytes.extend(
            frame_bytes(&ServerResponse::ack(dex_protocol::RequestId(2), Ack::Accepted))
                .expect("b"),
        );
        client.write_all(&bytes).await.expect("write");

        let mut reader = FrameReader::new(&mut server);
        for expected in [
            ServerResponse::ack(dex_protocol::RequestId(1), Ack::Ok),
            ServerResponse::ack(dex_protocol::RequestId(2), Ack::Accepted),
        ] {
            match reader.next::<ServerResponse>().await.expect("read") {
                Frame::Complete(got) => assert_eq!(got, expected),
                Frame::Closed => panic!("closed early"),
            }
        }
    }

    #[tokio::test]
    async fn a_hostile_length_prefix_is_rejected_without_allocating() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        client
            .write_all(&u32::MAX.to_le_bytes())
            .await
            .expect("write");

        let mut reader = FrameReader::new(&mut server);
        let err = reader
            .next::<ServerResponse>()
            .await
            .expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn garbage_is_rejected_rather_than_guessed_at() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let mut framed = 4u32.to_le_bytes().to_vec();
        framed.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        client.write_all(&framed).await.expect("write");

        let mut reader = FrameReader::new(&mut server);
        assert!(reader.next::<ServerResponse>().await.is_err());
    }

    #[test]
    fn an_over_long_socket_path_is_refused_with_a_clear_reason() {
        let long = Path::new("/tmp").join("x".repeat(200));
        let err = check_socket_path(&long).expect_err("must refuse");
        assert!(err.to_string().contains("under 100"), "got {err}");

        let ok = Path::new("/tmp/dex.sock");
        assert!(check_socket_path(ok).is_ok());
    }

    #[test]
    fn a_frame_helper_round_trips() {
        let response = ServerResponse::ack(dex_protocol::RequestId(1), Ack::Ok);
        let bytes = frame_bytes(&response).expect("frame");
        assert_eq!(read_frame(&bytes).expect("read").expect("some"), response);
        assert!(read_frame(&bytes[..2]).expect("read").is_none());
    }

    #[tokio::test]
    async fn a_request_frame_is_readable_too() {
        // The runtime receives requests, so the reader is generic over the type.
        let frame = RequestFrame::new(
            dex_protocol::RequestId(1),
            dex_protocol::ClientRequest::ListCapabilities,
        );
        let bytes = dex_protocol::encode_frame(&frame).expect("frame");

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move {
            client.write_all(&bytes).await.expect("write");
            client.flush().await.expect("flush");
            drop(client);
            let mut reader = FrameReader::new(server);
            reader.next::<RequestFrame>().await
        });
        match handle.await.expect("join").expect("read") {
            Frame::Complete(got) => assert_eq!(got, frame),
            Frame::Closed => panic!("closed early"),
        }
    }
}