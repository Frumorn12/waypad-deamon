//! Client for the Waypad signaling server.
//!
//! The daemon listens on the LAN, which makes it unreachable from anywhere else: a home router
//! will not forward to it, and under CGNAT there is nothing to forward. The way out is for neither
//! end to listen. The daemon dials *out* to a rendezvous server and stays connected, the phone
//! dials out too, and the server introduces them so they can negotiate a direct WebRTC path.
//!
//! What travels over this connection is only the introduction: SDP offers and ICE candidates,
//! forwarded verbatim. Media and input never touch it, and the peers still run the ordinary
//! end-to-end Waypad handshake over whatever transport comes out of the negotiation, so a hostile
//! signaling server can deny service but cannot listen in.
//!
//! Registration proves the daemon owns the fingerprint it claims, otherwise anyone could squat on
//! it and intercept connection attempts. The proof reuses the host's existing P-256 identity over
//! a transcript that is domain-separated from the control-channel handshake, so a signature made
//! here can never be replayed there.

use crate::crypto::{HostIdentity, b64, b64_decode};
use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use ring::rand::SystemRandom;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Must match `REGISTER_TRANSCRIPT_PREFIX` in the signaling server.
const REGISTER_TRANSCRIPT_PREFIX: &[u8] = b"WAYPAD-SIGNAL-REGISTER-v1";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Messages the daemon sends to the signaling server.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Register {
        fingerprint: String,
        public_key: String,
    },
    RegisterProof {
        signature: String,
    },
    Signal {
        session: String,
        payload: Value,
    },
    Leave {
        session: String,
    },
    Ping,
}

/// Messages the signaling server sends back.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Welcome {
        protocol: u32,
    },
    Challenge {
        nonce: String,
    },
    Registered {
        fingerprint: String,
    },
    PeerJoined {
        session: String,
    },
    /// Only ever sent to a phone opening a session; a host never receives it, but the variant
    /// has to exist or the message would fail to parse if the server ever misrouted one.
    Connected {
        #[allow(dead_code)]
        session: String,
    },
    Signal {
        session: String,
        payload: Value,
    },
    PeerLeft {
        session: String,
        reason: String,
    },
    Pong,
    Error {
        reason: String,
        detail: Option<String>,
    },
}

/// What the rest of the daemon reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalEvent {
    /// The daemon is published and reachable under its fingerprint.
    Registered,
    /// A phone opened a session; the WebRTC negotiation for it starts here.
    PeerJoined {
        session: String,
    },
    /// An SDP offer or ICE candidate from the phone, forwarded untouched.
    Signal {
        session: String,
        payload: Value,
    },
    PeerLeft {
        session: String,
        reason: String,
    },
    /// The link to the rendezvous server dropped; a reconnect is already scheduled.
    Disconnected {
        reason: String,
    },
}

/// A live registration with the signaling server.
///
/// The connection is kept up by a background task that re-registers after every drop, because a
/// daemon that is only sometimes registered is worse than one that never was: the phone would see
/// it as intermittently missing with no explanation.
pub struct SignalClient {
    outbound: mpsc::UnboundedSender<ClientMessage>,
    events: mpsc::UnboundedReceiver<SignalEvent>,
}

impl SignalClient {
    /// Starts the connection and keeps it alive until the returned client is dropped.
    pub fn start(url: String, identity: HostIdentity) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_forever(url, identity, outbound_rx, event_tx));
        Self {
            outbound: outbound_tx,
            events: event_rx,
        }
    }

    /// Forwards an SDP or ICE payload to the phone on the other end of a session.
    pub fn send_signal(&self, session: &str, payload: Value) -> anyhow::Result<()> {
        self.outbound
            .send(ClientMessage::Signal {
                session: session.to_string(),
                payload,
            })
            .context("signaling client is no longer running")
    }

    /// Tears down one session.
    pub fn leave(&self, session: &str) -> anyhow::Result<()> {
        self.outbound
            .send(ClientMessage::Leave {
                session: session.to_string(),
            })
            .context("signaling client is no longer running")
    }

    pub async fn next_event(&mut self) -> Option<SignalEvent> {
        self.events.recv().await
    }
}

/// Reconnect loop. Each attempt registers from scratch, since the server keeps no state for a
/// socket that went away.
async fn run_forever(
    url: String,
    identity: HostIdentity,
    mut outbound: mpsc::UnboundedReceiver<ClientMessage>,
    events: mpsc::UnboundedSender<SignalEvent>,
) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match run_session(&url, &identity, &mut outbound, &events).await {
            Ok(()) => {
                debug!(%url, "signaling session closed cleanly");
                backoff = RECONNECT_MIN;
            }
            Err(err) => {
                warn!(%url, %err, backoff_secs = backoff.as_secs(), "signaling session failed");
                let _ = events.send(SignalEvent::Disconnected {
                    reason: err.to_string(),
                });
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn run_session(
    url: &str,
    identity: &HostIdentity,
    outbound: &mut mpsc::UnboundedReceiver<ClientMessage>,
    events: &mpsc::UnboundedSender<SignalEvent>,
) -> anyhow::Result<()> {
    let (socket, _) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(url))
            .await
            .context("timed out connecting to the signaling server")?
            .context("could not reach the signaling server")?;
    let (mut sink, mut stream) = socket.split();

    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        register(identity, &mut sink, &mut stream),
    )
    .await
    .context("timed out registering with the signaling server")??;

    info!(fingerprint = %identity.fingerprint, "registered with the signaling server");
    let _ = events.send(SignalEvent::Registered);

    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.tick().await;

    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else {
                    bail!("signaling server closed the connection");
                };
                match message? {
                    Message::Text(text) => {
                        if let Some(event) = decode_event(text.as_str())? {
                            let _ = events.send(event);
                        }
                    }
                    Message::Close(_) => bail!("signaling server closed the connection"),
                    _ => {}
                }
            }
            outgoing = outbound.recv() => {
                let Some(outgoing) = outgoing else {
                    // The client handle went away: nothing left to relay.
                    return Ok(());
                };
                send(&mut sink, &outgoing).await?;
            }
            _ = keepalive.tick() => {
                send(&mut sink, &ClientMessage::Ping).await?;
            }
        }
    }
}

/// Runs register → challenge → proof, which is what publishes the fingerprint.
async fn register<S, R>(identity: &HostIdentity, sink: &mut S, stream: &mut R) -> anyhow::Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    send(
        sink,
        &ClientMessage::Register {
            fingerprint: identity.fingerprint.clone(),
            public_key: identity.public_key_b64.clone(),
        },
    )
    .await?;

    let nonce = loop {
        match expect_message(stream).await? {
            // The welcome arrives first and carries nothing this client needs yet.
            ServerMessage::Welcome { protocol } => debug!(protocol, "signaling server welcome"),
            ServerMessage::Challenge { nonce } => break nonce,
            ServerMessage::Error { reason, detail } => {
                bail!(
                    "signaling server rejected registration: {reason}{}",
                    detail.map(|d| format!(" ({d})")).unwrap_or_default()
                )
            }
            other => bail!("unexpected message while registering: {other:?}"),
        }
    };

    let nonce = b64_decode(&nonce).context("signaling challenge nonce is not valid base64")?;
    let transcript = register_transcript(&identity.fingerprint, &nonce);
    let signature = identity
        .key_pair()?
        .sign(&SystemRandom::new(), &transcript)
        .map_err(|_| anyhow::anyhow!("could not sign the signaling challenge"))?;
    send(
        sink,
        &ClientMessage::RegisterProof {
            signature: b64(signature.as_ref()),
        },
    )
    .await?;

    loop {
        match expect_message(stream).await? {
            ServerMessage::Registered { fingerprint } => {
                debug!(%fingerprint, "signaling registration confirmed");
                return Ok(());
            }
            ServerMessage::Welcome { .. } | ServerMessage::Pong => {}
            ServerMessage::Error { reason, detail } => {
                bail!(
                    "signaling server rejected the proof: {reason}{}",
                    detail.map(|d| format!(" ({d})")).unwrap_or_default()
                )
            }
            other => bail!("unexpected message while registering: {other:?}"),
        }
    }
}

/// The bytes signed to prove ownership of a fingerprint.
///
/// Byte-for-byte identical to the server's own construction, prefix included; the prefix is what
/// keeps this signature from being replayable against the control-channel handshake.
pub fn register_transcript(fingerprint: &str, nonce: &[u8]) -> Vec<u8> {
    let mut transcript =
        Vec::with_capacity(REGISTER_TRANSCRIPT_PREFIX.len() + fingerprint.len() + nonce.len());
    transcript.extend_from_slice(REGISTER_TRANSCRIPT_PREFIX);
    transcript.extend_from_slice(fingerprint.as_bytes());
    transcript.extend_from_slice(nonce);
    transcript
}

/// Turns a server message into an event, or `None` for the ones only the transport cares about.
fn decode_event(text: &str) -> anyhow::Result<Option<SignalEvent>> {
    let message: ServerMessage =
        serde_json::from_str(text).context("signaling server sent a message we cannot parse")?;
    let event = match message {
        ServerMessage::PeerJoined { session } => Some(SignalEvent::PeerJoined { session }),
        ServerMessage::Signal { session, payload } => {
            Some(SignalEvent::Signal { session, payload })
        }
        ServerMessage::PeerLeft { session, reason } => {
            Some(SignalEvent::PeerLeft { session, reason })
        }
        ServerMessage::Error { reason, detail } => {
            // Errors here are per-request and not fatal to the registration, so they are logged
            // rather than thrown: losing the whole link over one bad relay would be worse.
            warn!(%reason, ?detail, "signaling server reported an error");
            None
        }
        ServerMessage::Welcome { .. }
        | ServerMessage::Challenge { .. }
        | ServerMessage::Registered { .. }
        | ServerMessage::Connected { .. }
        | ServerMessage::Pong => None,
    };
    Ok(event)
}

async fn expect_message<R>(stream: &mut R) -> anyhow::Result<ServerMessage>
where
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let Some(message) = stream.next().await else {
            bail!("signaling server closed the connection during registration");
        };
        match message? {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str())
                    .context("signaling server sent a message we cannot parse");
            }
            Message::Close(_) => {
                bail!("signaling server closed the connection during registration")
            }
            _ => continue,
        }
    }
}

async fn send<S>(sink: &mut S, message: &ClientMessage) -> anyhow::Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let json = serde_json::to_string(message)?;
    sink.send(Message::Text(json.into()))
        .await
        .context("could not send to the signaling server")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_matches_what_the_server_verifies() {
        let transcript = register_transcript("aa:bb", b"nonce");
        assert!(transcript.starts_with(REGISTER_TRANSCRIPT_PREFIX));
        assert!(transcript.ends_with(b"nonce"));
        assert_eq!(
            transcript.len(),
            REGISTER_TRANSCRIPT_PREFIX.len() + "aa:bb".len() + 5
        );
    }

    #[test]
    fn a_different_fingerprint_signs_different_bytes() {
        assert_ne!(
            register_transcript("aa:bb", b"nonce"),
            register_transcript("cc:dd", b"nonce")
        );
    }

    #[test]
    fn register_serialises_the_way_the_server_expects() {
        let raw = serde_json::to_string(&ClientMessage::Register {
            fingerprint: "aa:bb".into(),
            public_key: "key".into(),
        })
        .unwrap();
        assert!(raw.contains("\"type\":\"register\""));
        assert!(raw.contains("\"fingerprint\":\"aa:bb\""));
        assert!(raw.contains("\"public_key\":\"key\""));
    }

    #[test]
    fn signal_carries_the_payload_untouched() {
        let raw = serde_json::to_string(&ClientMessage::Signal {
            session: "s1".into(),
            payload: serde_json::json!({"sdp": "v=0", "nested": {"a": [1, 2]}}),
        })
        .unwrap();
        assert!(raw.contains("\"type\":\"signal\""));
        assert!(raw.contains("\"sdp\":\"v=0\""));
        assert!(raw.contains("\"nested\""));
    }

    #[test]
    fn a_peer_joining_becomes_an_event() {
        let event = decode_event(r#"{"type":"peer_joined","session":"s1"}"#).unwrap();
        assert_eq!(
            event,
            Some(SignalEvent::PeerJoined {
                session: "s1".into()
            })
        );
    }

    #[test]
    fn a_relayed_payload_survives_the_round_trip() {
        let event =
            decode_event(r#"{"type":"signal","session":"s1","payload":{"candidate":"a=x"}}"#)
                .unwrap();
        match event {
            Some(SignalEvent::Signal { session, payload }) => {
                assert_eq!(session, "s1");
                assert_eq!(payload["candidate"], "a=x");
            }
            other => panic!("expected a signal event, got {other:?}"),
        }
    }

    #[test]
    fn transport_only_messages_produce_no_event() {
        for raw in [
            r#"{"type":"welcome","protocol":1}"#,
            r#"{"type":"pong"}"#,
            r#"{"type":"registered","fingerprint":"aa:bb"}"#,
            r#"{"type":"connected","session":"s1"}"#,
        ] {
            assert_eq!(decode_event(raw).unwrap(), None, "for {raw}");
        }
    }

    #[test]
    fn a_relay_error_is_logged_rather_than_ending_the_session() {
        // Losing the registration over one rejected relay would take the host offline for
        // everyone, so per-request errors must not surface as events.
        let event = decode_event(r#"{"type":"error","reason":"unknown_session"}"#).unwrap();
        assert_eq!(event, None);
    }

    #[test]
    fn an_unparseable_message_is_an_error_not_a_silent_drop() {
        assert!(decode_event("{not json").is_err());
        assert!(decode_event(r#"{"type":"something_new"}"#).is_err());
    }

    #[test]
    fn peer_left_carries_the_reason_through() {
        let event =
            decode_event(r#"{"type":"peer_left","session":"s1","reason":"peer_disconnected"}"#)
                .unwrap();
        assert_eq!(
            event,
            Some(SignalEvent::PeerLeft {
                session: "s1".into(),
                reason: "peer_disconnected".into(),
            })
        );
    }
}
