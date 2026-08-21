//! Connects to a signaling server, registers, and prints what comes back.
//!
//! Exists so the client can be pointed at a real server by hand:
//!
//! ```text
//! cargo run --example signal_probe -- ws://127.0.0.1:47790/ws
//! ```
//!
//! It uses the daemon's own host identity, so what it proves is exactly what the daemon would do.

use waypad_daemon::{
    config::Config,
    signal::{SignalClient, SignalEvent},
    state::{StatePaths, load_or_create_identity},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,waypad_daemon=debug")
        .init();
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:47790/ws".to_string());

    let config = Config::load(None)?;
    let paths = StatePaths::new(&config);
    let identity = load_or_create_identity(&paths)?;
    println!("fingerprint: {}", identity.fingerprint);
    println!("connecting to {url}");

    let mut client = SignalClient::start(url, identity);
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(40);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, client.next_event()).await {
            Ok(Some(event)) => {
                println!("event: {event:?}");
                match &event {
                    SignalEvent::Registered => println!("REGISTERED OK"),
                    // Answer whatever the phone offers, which is what the WebRTC negotiation
                    // will really do once a peer connection sits behind this.
                    SignalEvent::Signal { session, .. } => {
                        client.send_signal(
                            session,
                            serde_json::json!({"kind": "answer", "sdp": "v=0 pretend-answer"}),
                        )?;
                        println!("host -> answer on session {session}");
                    }
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(())
}
