use anyhow::{Context, bail};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use waypad_daemon::{
    capability::Capabilities,
    config::Config,
    server,
    state::{
        StatePaths, create_pairing_code, load_or_create_identity, load_trusted_devices,
        rotate_identity, save_trusted_devices,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    init_logging(&config.log_level);
    let paths = StatePaths::new(&config);

    match cli.command.as_deref().unwrap_or("serve") {
        "serve" => {
            let identity = load_or_create_identity(&paths)?;
            server::run(config, paths, identity).await
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
            let capabilities = Capabilities::detect(config.allow_suspend).await;
            println!("Host fingerprint: {}", identity.fingerprint);
            println!("{}", serde_json::to_string_pretty(&capabilities)?);
            Ok(())
        }
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
        other => bail!("unknown command: {other}"),
    }
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

fn init_logging(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
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
  doctor                        Print Wayland, portal, and backend diagnostics
  devices list                  List trusted Android devices
  devices revoke <device-id>    Revoke a trusted Android device
  rotate-host-key               Rotate host identity and require re-pairing
  write-sample-config           Write the default JSON config

Options:
  --config <path>               Use an explicit config file
  -h, --help                    Show this help"
    );
}
