//! Volume, media, brightness, clipboard, and session actions on Linux.
//!
//! Every one of these is a shell-out to whatever the distribution happens to
//! ship. The tool is probed before it is run so an absent one is reported as a
//! sentence the user can act on ("playerctl is not installed") rather than a
//! process spawn failure.

use crate::platform::command_exists;
use anyhow::{Context, bail};
use async_trait::async_trait;
use tokio::process::Command;
use waypad_core::{
    backend::SystemBackend,
    protocol::{BrightnessAction, MediaAction, VolumeAction},
};

#[derive(Debug, Default)]
pub struct LinuxSystemBackend;

impl LinuxSystemBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SystemBackend for LinuxSystemBackend {
    async fn media(&self, action: MediaAction) -> anyhow::Result<()> {
        if !command_exists("playerctl") {
            bail!("Media controls unavailable: playerctl is not installed");
        }
        let arg = match action {
            MediaAction::PlayPause => "play-pause",
            MediaAction::Next => "next",
            MediaAction::Previous => "previous",
            MediaAction::Stop => "stop",
        };
        run("playerctl", &[arg]).await
    }

    async fn volume(&self, action: VolumeAction) -> anyhow::Result<()> {
        if command_exists("wpctl") {
            let args: Vec<&str> = match action {
                VolumeAction::Up => vec!["set-volume", "@DEFAULT_AUDIO_SINK@", "5%+"],
                VolumeAction::Down => vec!["set-volume", "@DEFAULT_AUDIO_SINK@", "5%-"],
                VolumeAction::MuteToggle => vec!["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"],
            };
            return run("wpctl", &args).await;
        }
        if command_exists("pactl") {
            let args: Vec<&str> = match action {
                VolumeAction::Up => vec!["set-sink-volume", "@DEFAULT_SINK@", "+5%"],
                VolumeAction::Down => vec!["set-sink-volume", "@DEFAULT_SINK@", "-5%"],
                VolumeAction::MuteToggle => vec!["set-sink-mute", "@DEFAULT_SINK@", "toggle"],
            };
            return run("pactl", &args).await;
        }
        bail!("Volume control unavailable: neither wpctl nor pactl is installed")
    }

    async fn brightness(&self, action: BrightnessAction) -> anyhow::Result<()> {
        if !command_exists("brightnessctl") {
            bail!("Brightness control unavailable on this system");
        }
        let arg = match action {
            BrightnessAction::Up => "5%+",
            BrightnessAction::Down => "5%-",
        };
        run("brightnessctl", &["set", arg]).await
    }

    async fn clipboard_set(&self, text: &str) -> anyhow::Result<()> {
        // The length cap lives in the command handler, which rejects oversized
        // text before it reaches any backend; duplicating it here would only
        // create two limits that can drift apart.
        if !command_exists("wl-copy") {
            bail!("Clipboard integration unavailable: wl-copy is not installed");
        }
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn wl-copy")?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(text.as_bytes()).await?;
        }
        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("wl-copy exited with {status}")
        }
    }

    async fn lock(&self) -> anyhow::Result<()> {
        if !command_exists("loginctl") {
            bail!("Lock unavailable: loginctl is not installed");
        }
        run("loginctl", &["lock-session"]).await
    }

    async fn suspend(&self) -> anyhow::Result<()> {
        // Whether suspend is permitted at all is decided by the config, checked
        // by the caller: this only performs it.
        run("systemctl", &["suspend"]).await
    }
}

/// Reports which of these actions this host can actually perform.
pub fn detect_system_capabilities() -> waypad_core::capability::SystemCapabilities {
    waypad_core::capability::SystemCapabilities {
        volume: command_exists("wpctl") || command_exists("pactl"),
        media: command_exists("playerctl"),
        brightness: command_exists("brightnessctl"),
        clipboard: command_exists("wl-copy"),
        lock: command_exists("loginctl"),
        suspend: command_exists("systemctl"),
    }
}

async fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .await
        .with_context(|| format!("failed to execute {program}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{program} exited with {status}")
    }
}
