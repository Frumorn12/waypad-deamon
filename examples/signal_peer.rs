//! Stands in for the phone against a signaling server, so the introduction can be exercised
//! without an Android build.
//!
//! ```text
//! cargo run --example signal_peer -- ws://127.0.0.1:47790/ws <host-fingerprint>
//! ```
//!
//! Opens a session with the named host, sends an SDP offer through it, and prints what comes back.

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

type Sink = futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

async fn send(sink: &mut Sink, value: serde_json::Value) -> anyhow::Result<()> {
    sink.send(Message::Text(value.to_string().into())).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "ws://127.0.0.1:47790/ws".into());
    let fingerprint = args
        .next()
        .expect("pass the host fingerprint as the second argument");

    let (socket, _) = tokio_tungstenite::connect_async(&url).await?;
    let (mut sink, mut stream) = socket.split();

    send(
        &mut sink,
        serde_json::json!({"type": "connect", "fingerprint": fingerprint}),
    )
    .await?;

    let mut offered = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(12);
    while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, stream.next()).await {
        let Message::Text(text) = message else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&text)?;
        println!("phone <- {value}");
        match value["type"].as_str() {
            Some("connected") => {
                let session = value["session"].as_str().unwrap().to_string();
                println!("phone -> offer on session {session}");
                send(
                    &mut sink,
                    serde_json::json!({
                        "type": "signal",
                        "session": session,
                        "payload": {"kind": "offer", "sdp": "v=0 pretend-offer"},
                    }),
                )
                .await?;
                offered = true;
            }
            Some("error") => {
                println!("phone: rejected — {}", value["reason"]);
                break;
            }
            Some("signal") if offered => {
                println!("PHONE GOT THE ANSWER BACK: {}", value["payload"]);
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
