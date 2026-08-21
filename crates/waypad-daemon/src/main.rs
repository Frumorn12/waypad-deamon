//! The Waypad host daemon.
//!
//! Platform-neutral from here down: the binary picks a [`PlatformHost`] at
//! compile time and everything else is the same code on Linux and Windows.

use anyhow::{Context, bail};
use std::{io::IsTerminal, path::PathBuf, sync::Arc};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use waypad_core::{
    backend::PlatformHost,
    config::Config,
    invite::{Invite, InviteRoute, qr_terminal},
    server,
    state::{
        StatePaths, create_pairing_code, load_or_create_identity, load_trusted_devices,
        rotate_identity, save_trusted_devices,
    },
};
use waypad_ui::{LogBuffer, LogBufferLayer};

#[cfg(target_os = "linux")]
fn platform_host(paths: &StatePaths) -> Arc<dyn PlatformHost> {
    Arc::new(waypad_linux::host::LinuxHost::new(paths.clone()))
}

#[cfg(target_os = "windows")]
fn platform_host(_paths: &StatePaths) -> Arc<dyn PlatformHost> {
    Arc::new(waypad_windows::host::WindowsHost::new())
}

/// Puts an icon in the notification area, where there is one.
///
/// Windows only. A Linux desktop has a terminal and a systemd unit, and the
/// tray crates that cover it drag GTK into the build for a convenience it does
/// not need. Failure here is never fatal: the panel URL still works.
#[cfg(target_os = "windows")]
fn start_tray(url: &str) {
    if let Err(err) = waypad_windows::tray::spawn(url.to_string()) {
        tracing::warn!(%err, "the tray icon is unavailable");
    }
}

#[cfg(not(target_os = "windows"))]
fn start_tray(_url: &str) {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    let logs = LogBuffer::new();
    init_logging(&config.log_level, logs.clone());
    let paths = StatePaths::new(&config);
    let host = platform_host(&paths);

    match cli.command.as_deref().unwrap_or("serve") {
        "serve" => {
            let identity = Arc::new(load_or_create_identity(&paths)?);
            let panel = waypad_ui::start(waypad_ui::PanelContext {
                config: config.clone(),
                paths: paths.clone(),
                identity: identity.clone(),
                host: host.clone(),
                logs,
            })
            .await?;
            println!("Waypad control panel: {}", panel.url());
            start_tray(panel.url());
            // Only when a person is watching. Started from the login item there
            // is no console and no one asked for a browser window; the tray
            // icon is the way in then.
            if std::io::stdout().is_terminal()
                && let Err(err) = waypad_ui::open_in_browser(panel.url())
            {
                tracing::debug!(%err, "could not open the panel automatically");
            }
            server::run(config, paths, identity, host).await
        }
        "pair-code" => {
            let identity = load_or_create_identity(&paths)?;
            let code = create_pairing_code(&config, &paths)?;
            println!("Waypad pairing code: {}", code.code);
            println!("Expires at unix timestamp: {}", code.expires_at);
            println!("Host fingerprint: {}", identity.fingerprint);
            println!(
                "Enter this code in the Android app and verify the fingerprint if pairing manually."
            );
            Ok(())
        }
        "doctor" => {
            let identity = load_or_create_identity(&paths)?;
            let capabilities = host.detect_capabilities(&config).await;
            println!("Platform: {}", host.name());
            println!("Host name: {}", host.hostname());
            println!("Host fingerprint: {}", identity.fingerprint);
            println!("{}", serde_json::to_string_pretty(&capabilities)?);
            Ok(())
        }
        "invite" => invite_command(&config, &paths, host.as_ref(), &cli.trailing).await,
        "devices" => devices_command(&paths, &cli.trailing),
        "rotate-host-key" => {
            let identity = rotate_identity(&paths)?;
            println!("Rotated Waypad host identity.");
            println!("New host fingerprint: {}", identity.fingerprint);
            println!("Previously paired Android hosts must be re-paired.");
            Ok(())
        }
        "write-sample-config" => {
            let path = cli.config.unwrap_or_else(Config::default_path);
            Config::write_sample(&path)?;
            println!("Wrote sample config to {}", path.display());
            Ok(())
        }
        #[cfg(target_os = "linux")]
        "authorize-portal" => authorize_portal_command(&paths).await,
        other => bail!("unknown command: {other}"),
    }
}

/// Pre-approves screen capture once, so later streams start without a dialog.
#[cfg(target_os = "linux")]
async fn authorize_portal_command(paths: &StatePaths) -> anyhow::Result<()> {
    use std::time::Duration;
    println!("Opening ScreenCast portal authorization (60s timeout)...");
    println!("A dialog should appear on your desktop. Approve screen sharing.");
    println!("This needs to be done only ONCE.");
    println!();
    match tokio::time::timeout(
        Duration::from_secs(60),
        waypad_linux::screen::authorize_portal(),
    )
    .await
    {
        Ok(Ok(token)) => {
            waypad_core::state::save_portal_restore_token(paths, &token)?;
            println!("Portal authorized successfully!");
            println!("You can now stream at 60 FPS without any host approval.");
            Ok(())
        }
        // Not an error: the grim fallback still produces a picture, so a denied
        // or absent dialog leaves the daemon usable rather than broken.
        Ok(Err(err)) => {
            eprintln!();
            eprintln!("Authorization failed: {err}");
            eprintln!();
            eprintln!("The portal dialog did not appear or was denied.");
            eprintln!("This is OK — the daemon will automatically use the grim");
            eprintln!("screenshot backend instead. It's slower but works without approval.");
            eprintln!();
            eprintln!("To try again later, re-run this command.");
            Ok(())
        }
        Err(_elapsed) => {
            eprintln!();
            eprintln!("Authorization timed out after 60 seconds.");
            eprintln!("The portal dialog did not appear on your desktop.");
            eprintln!();
            eprintln!("This is OK — the daemon will use the grim fallback automatically.");
            eprintln!("Streams will work at 20-25 fps without any approval.");
            eprintln!();
            eprintln!("To try portal again: waypad-daemon authorize-portal");
            Ok(())
        }
    }
}

async fn invite_command(
    config: &Config,
    paths: &StatePaths,
    host: &dyn PlatformHost,
    trailing: &[String],
) -> anyhow::Result<()> {
    let identity = load_or_create_identity(paths)?;
    let mut qr = false;
    let mut address: Option<String> = None;
    let mut remote_address: Option<String> = None;
    let mut allow_public_pairing = config.allow_public_pairing;
    let mut port = config.control_port;
    let mut ttl = config.pairing_code_ttl_seconds;
    let mut args = trailing.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--qr" => qr = true,
            "--address" => address = args.next().cloned(),
            "--remote-address" => remote_address = args.next().cloned(),
            "--allow-public-pairing" => allow_public_pairing = true,
            "--port" => {
                port = args
                    .next()
                    .context("usage: invite --port <1-65535>")?
                    .parse()?;
            }
            "--ttl" => {
                ttl = args
                    .next()
                    .context("usage: invite --ttl <seconds>")?
                    .parse()?;
            }
            other => bail!("unknown invite option: {other}"),
        }
    }
    let mut invite_config = config.clone();
    invite_config.pairing_code_ttl_seconds = ttl.clamp(30, 900);
    let code = create_pairing_code(&invite_config, paths)?;
    let lan_address = match address {
        Some(address) => address,
        None => host
            .primary_lan_address()
            .await
            .unwrap_or_else(|| "127.0.0.1".into()),
    };
    let can_pair_publicly = !config.require_private_lan || allow_public_pairing;
    let (route, policy) = match remote_address {
        Some(_) if can_pair_publicly => (InviteRoute::DirectPublic, "public-pairing"),
        Some(_) => (InviteRoute::DirectPublic, "public-reconnect"),
        None => (InviteRoute::DirectLan, "lan-only"),
    };
    let invite = Invite {
        host: host.hostname(),
        address: remote_address
            .clone()
            .unwrap_or_else(|| lan_address.clone()),
        lan_address,
        remote_address: remote_address.clone(),
        port,
        fingerprint: identity.fingerprint.clone(),
        code: code.code.clone(),
        expires_at: code.expires_at,
        route,
        policy,
        allow_public_pairing,
    };
    let payload = invite.payload();

    println!(
        "Waypad invite expires at unix timestamp: {}",
        code.expires_at
    );
    println!("Pairing code: {}", code.code);
    println!("Payload: {payload}");
    if qr {
        print!("{}", qr_terminal(&payload)?);
    } else {
        println!("Run `waypad-daemon invite --qr` to print a terminal QR code.");
    }
    if remote_address.is_some() {
        if can_pair_publicly {
            println!();
            println!("Remote pairing enabled for this invite. Ensure TCP/{port} is reachable");
            println!("from the internet and that your firewall restricts it appropriately.");
        } else {
            println!();
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!(" REMOTE PAIRING IS CURRENTLY BLOCKED");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("This QR contains a public endpoint, but the daemon config blocks");
            println!("pairing from public networks (require_private_lan=true and");
            println!("allow_public_pairing=false).");
            println!();
            println!("Options to enable outside-LAN pairing:");
            println!("  1) Set allow_public_pairing=true in the config");
            println!("     (keeps LAN-only for reconnect).");
            println!("  2) Set require_private_lan=false to allow all public traffic.");
            println!();
            println!("Only do this if TCP/{port} is port-forwarded and protected by your");
            println!("firewall. Pairing still requires the one-time 6-digit code.");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        }
    }
    Ok(())
}

fn devices_command(paths: &StatePaths, trailing: &[String]) -> anyhow::Result<()> {
    let mut devices = load_trusted_devices(paths)?;
    match trailing.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            if devices.devices.is_empty() {
                println!("No trusted devices.");
                return Ok(());
            }
            for device in &devices.devices {
                println!(
                    "{}\t{}\trevoked={}\tlast_seen={:?}",
                    device.id, device.name, device.revoked, device.last_seen_at
                );
            }
            Ok(())
        }
        "revoke" => {
            let id = trailing
                .get(1)
                .context("usage: waypad-daemon devices revoke <device-id>")?;
            if devices.revoke(id) {
                save_trusted_devices(paths, &devices)?;
                println!("Revoked device {id}");
                Ok(())
            } else {
                bail!("No trusted device found with id {id}")
            }
        }
        other => bail!("unknown devices subcommand: {other}"),
    }
}

/// Sends logs to the console and to the control panel at once.
///
/// The panel copy is what makes a daemon started from a login item
/// diagnosable: it has no console, and on Windows there is no journal to fall
/// back on either.
fn init_logging(default_level: &str, logs: LogBuffer) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .compact(),
        )
        .with(LogBufferLayer::new(logs))
        .init();
}

#[derive(Debug)]
struct Cli {
    config: Option<PathBuf>,
    command: Option<String>,
    trailing: Vec<String>,
}

impl Cli {
    fn parse() -> Self {
        let mut args = std::env::args().skip(1);
        let mut config = None;
        let mut command = None;
        let mut trailing = Vec::new();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    config = args.next().map(PathBuf::from);
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                value if command.is_none() => {
                    command = Some(value.to_string());
                }
                value => trailing.push(value.to_string()),
            }
        }
        Self {
            config,
            command,
            trailing,
        }
    }
}

fn print_help() {
    println!(
        "waypad-daemon commands:
  serve                         Run the daemon
  pair-code                     Create a local one-time pairing code
  invite [--qr]                 Create a waypad:// invite; add --remote-address host for mobile data
  invite [--qr --allow-public-pairing --remote-address <host>]
                                Allow public-network pairing for this invite
  doctor                        Print platform, input, capture, and audio diagnostics
  devices list                  List trusted Android devices
  devices revoke <device-id>    Revoke a trusted Android device
  rotate-host-key               Rotate host identity and require re-pairing
  write-sample-config           Write the default JSON config"
    );
    #[cfg(target_os = "linux")]
    println!(
        "  authorize-portal              Pre-authorize screen capture (approve ONCE, then auto-approve forever)"
    );
    println!(
        "
Options:
  --config <path>               Use an explicit config file
  -h, --help                    Show this help"
    );
}
