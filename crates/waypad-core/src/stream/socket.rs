//! The screen-stream socket and its envelope framing.
//!
//! This is the exact wire format documented in `docs/PROTOCOL.md` and already
//! implemented by the shipped Android client, so it is deliberately boring and
//! must not drift: a handshake line naming the codec, then repeating
//! `u32_be header_len | u32_be payload_len | header | payload` envelopes.

use anyhow::Context;
use serde_json::json;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::Mutex, time::timeout};
use tracing::debug;

/// How long a JPEG frame may wait for the socket before it is dropped as stale.
const SEND_FRAME_DEADLINE_MS: u64 = 12;
/// H.264 frames are never dropped, so this bound only exists to detach a client
/// that has stopped reading entirely.
const H264_SEND_TIMEOUT_SECS: u64 = 10;

/// The stream socket, shared by the video producer and the optional desktop
/// audio producer.
///
/// Both write whole envelopes under the same lock, so the two producers
/// interleave on the wire without ever cutting each other's frames — which is
/// what lets audio ride the existing connection instead of opening a second one.
/// Audio waits only briefly for the lock and drops its packet otherwise, so it
/// can never delay a video frame.
#[derive(Debug)]
pub struct StreamSocket {
    socket: Mutex<TcpStream>,
    magic_sent: AtomicBool,
}

impl StreamSocket {
    pub fn new(socket: TcpStream) -> Self {
        Self {
            socket: Mutex::new(socket),
            magic_sent: AtomicBool::new(false),
        }
    }

    /// True once the handshake line naming the payload codec has been written.
    /// Nothing may be sent before it: a client cannot frame what it cannot
    /// identify.
    pub fn handshake_sent(&self) -> bool {
        self.magic_sent.load(Ordering::Acquire)
    }

    /// Writes the handshake line, once.
    ///
    /// Sent lazily, only after a producer has actually encoded something,
    /// because a platform may still fall back to a different codec while the
    /// socket is untouched.
    pub async fn send_magic(&self, magic: &[u8]) -> anyhow::Result<()> {
        if self.handshake_sent() {
            return Ok(());
        }
        let mut socket = self.socket.lock().await;
        if self.handshake_sent() {
            return Ok(());
        }
        socket.write_all(magic).await?;
        self.magic_sent.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) async fn write_all(&self, buf: &[u8], deadline: Duration) -> anyhow::Result<()> {
        let mut socket = self.socket.lock().await;
        timeout(deadline, socket.write_all(buf))
            .await
            .context("screen stream client stalled while receiving a frame")??;
        Ok(())
    }

    /// Writes an envelope only if the socket is free, reporting `false` when the
    /// other producer held it for longer than `lock_timeout`.
    ///
    /// Once the lock is taken the envelope is written whole: a partial write
    /// would desynchronise the framing for the rest of the session.
    pub async fn try_write_envelope(
        &self,
        header: &str,
        payload: &[u8],
        lock_timeout: Duration,
        write_timeout: Duration,
    ) -> anyhow::Result<bool> {
        let Ok(mut socket) = timeout(lock_timeout, self.socket.lock()).await else {
            return Ok(false);
        };
        let buf = frame_envelope(header, payload);
        timeout(write_timeout, socket.write_all(&buf))
            .await
            .context("screen stream client stalled while receiving an audio packet")??;
        Ok(true)
    }
}

pub fn frame_envelope(header: &str, payload: &[u8]) -> Vec<u8> {
    let header = header.as_bytes();
    let mut buf = Vec::with_capacity(4 + 4 + header.len() + payload.len());
    buf.extend_from_slice(&(header.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(header);
    buf.extend_from_slice(payload);
    buf
}

/// Encoded size of the picture together with the size of the desktop it came from.
///
/// The two differ whenever the client asks for a smaller stream. Only the encoded
/// size describes the pixels on the wire, but the client maps touches onto desktop
/// coordinates, so it needs the source size too: mapping against the encoded size
/// would confine the pointer to a corner of the screen, silently and with no error.
#[derive(Debug, Clone, Copy)]
pub struct FrameGeometry {
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl FrameGeometry {
    /// Geometry for a picture that was never downscaled.
    pub fn unscaled(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            source_width: width,
            source_height: height,
        }
    }

    fn header_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = serde_json::Map::new();
        fields.insert("width".into(), self.width.into());
        fields.insert("height".into(), self.height.into());
        fields.insert("source_width".into(), self.source_width.into());
        fields.insert("source_height".into(), self.source_height.into());
        fields
    }
}

pub async fn send_h264_frame(
    socket: &StreamSocket,
    seq: u64,
    geometry: FrameGeometry,
    payload: &[u8],
    key_frame: bool,
    config: bool,
) -> anyhow::Result<()> {
    let mut header_object = geometry.header_fields();
    header_object.insert("seq".into(), seq.into());
    header_object.insert("timestamp_ms".into(), json!(now_millis()));
    header_object.insert("codec".into(), "h264".into());
    header_object.insert("key_frame".into(), key_frame.into());
    header_object.insert("config".into(), config.into());
    let header = serde_json::Value::Object(header_object).to_string();
    let buf = frame_envelope(&header, payload);
    // Unlike JPEG, an H.264 frame is referenced by everything that follows it,
    // so partial or dropped writes would corrupt the rest of the session:
    // frames are always written whole and only a wedged client aborts.
    socket
        .write_all(&buf, Duration::from_secs(H264_SEND_TIMEOUT_SECS))
        .await
        .context("screen stream client stalled while receiving an H.264 frame")?;
    Ok(())
}

/// The JPEG path reports the source size as the frame size: the picture may be
/// downscaled on the wire, but the client only ever needs desktop coordinates.
pub fn jpeg_frame_header(seq: u64, width: u32, height: u32) -> String {
    json!({
        "seq": seq,
        "timestamp_ms": now_millis(),
        "codec": "jpeg",
        "width": width,
        "height": height,
        "source_width": width,
        "source_height": height
    })
    .to_string()
}

/// Sends a JPEG frame, dropping it if the socket cannot take it promptly.
///
/// Dropping is safe here and not for H.264: every JPEG is independent, so a
/// lost one costs exactly one stale picture rather than corrupting everything
/// that references it.
pub async fn send_jpeg_frame(
    socket: &StreamSocket,
    seq: u64,
    width: u32,
    height: u32,
    jpeg: &[u8],
) -> anyhow::Result<()> {
    send_jpeg_frame_deadline(socket, seq, width, height, jpeg, SEND_FRAME_DEADLINE_MS).await
}

pub async fn send_jpeg_frame_deadline(
    socket: &StreamSocket,
    seq: u64,
    width: u32,
    height: u32,
    jpeg: &[u8],
    deadline_ms: u64,
) -> anyhow::Result<()> {
    let buf = frame_envelope(&jpeg_frame_header(seq, width, height), jpeg);

    match socket
        .write_all(&buf, Duration::from_millis(deadline_ms))
        .await
    {
        Ok(()) => Ok(()),
        Err(err) if err.downcast_ref::<std::io::Error>().is_some() => Err(err),
        Err(_elapsed) => {
            debug!(seq, "dropping frame: send deadline exceeded");
            Err(anyhow::anyhow!("frame send deadline exceeded (dropped)"))
        }
    }
}

pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Whether an error means the client hung up rather than something being wrong
/// with the host, so a normal disconnect is not logged as a failure.
pub fn is_client_disconnect(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_prefixes_both_lengths_big_endian() {
        let buf = frame_envelope("hi", &[1, 2, 3]);
        assert_eq!(&buf[0..4], &2u32.to_be_bytes());
        assert_eq!(&buf[4..8], &3u32.to_be_bytes());
        assert_eq!(&buf[8..10], b"hi");
        assert_eq!(&buf[10..], &[1, 2, 3]);
    }

    #[test]
    fn frame_header_carries_the_desktop_size_next_to_the_encoded_size() {
        let geometry = FrameGeometry {
            width: 1280,
            height: 720,
            source_width: 2560,
            source_height: 1440,
        };
        let fields = geometry.header_fields();
        assert_eq!(fields["width"], 1280);
        assert_eq!(fields["height"], 720);
        assert_eq!(fields["source_width"], 2560);
        assert_eq!(fields["source_height"], 1440);
    }

    #[test]
    fn jpeg_header_reports_the_desktop_size_as_the_source() {
        let raw = jpeg_frame_header(7, 1920, 1080);
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["seq"], 7);
        assert_eq!(value["codec"], "jpeg");
        assert_eq!(value["width"], 1920);
        assert_eq!(value["source_width"], 1920);
        assert_eq!(value["source_height"], 1080);
    }

    #[test]
    fn classifies_client_disconnect_io_errors() {
        let broken = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(is_client_disconnect(&broken));
        let other = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(!is_client_disconnect(&other));
    }
}
