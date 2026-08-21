use crate::{
    config::Config,
    platform::command_exists,
    protocol::{BrightnessAction, MediaAction, SystemAction, VolumeAction},
};
use anyhow::{Context, bail};
use tokio::process::Command;

pub async fn media(action: MediaAction) -> anyhow::Result<()> {
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

pub async fn volume(action: VolumeAction) -> anyhow::Result<()> {
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

pub async fn brightness(action: BrightnessAction) -> anyhow::Result<()> {
    if !command_exists("brightnessctl") {
        bail!("Brightness control unavailable on this system");
    }
    let arg = match action {
        BrightnessAction::Up => "5%+",
        BrightnessAction::Down => "5%-",
    };
    run("brightnessctl", &["set", arg]).await
}

pub async fn clipboard_set(text: &str) -> anyhow::Result<()> {
    if !command_exists("wl-copy") {
        bail!("Clipboard integration unavailable: wl-copy is not installed");
    }
    if text.len() > 64 * 1024 {
        bail!("Clipboard text rejected: maximum length is 64 KiB");
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

pub async fn system(config: &Config, action: SystemAction) -> anyhow::Result<()> {
    match action {
        SystemAction::Lock => {
            if !command_exists("loginctl") {
                bail!("Lock unavailable: loginctl is not installed");
            }
            run("loginctl", &["lock-session"]).await
        }
        SystemAction::Suspend => {
            if !config.allow_suspend {
                bail!("Suspend is disabled by daemon configuration");
            }
            run("systemctl", &["suspend"]).await
        }
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
