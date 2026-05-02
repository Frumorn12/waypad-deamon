use anyhow::{Context, bail};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use waypad_daemon::{
    capability::Capabilities,
    config::Config,
    platform::command_output,
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
            let capabilities = Capabilities::detect(&config).await;
            println!("Host fingerprint: {}", identity.fingerprint);
            println!("{}", serde_json::to_string_pretty(&capabilities)?);
            Ok(())
        }
        "invite" => invite_command(&config, &paths, &cli.trailing),
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

fn invite_command(config: &Config, paths: &StatePaths, trailing: &[String]) -> anyhow::Result<()> {
    let identity = load_or_create_identity(paths)?;
    let mut qr = false;
    let mut address: Option<String> = None;
    let mut remote_address: Option<String> = None;
    let mut port = config.control_port;
    let mut ttl = config.pairing_code_ttl_seconds;
    let mut args = trailing.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--qr" => qr = true,
            "--address" => address = args.next().cloned(),
            "--remote-address" => remote_address = args.next().cloned(),
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
    let local_address = address.unwrap_or_else(default_invite_address);
    let route = if remote_address.is_some() {
        "direct-public"
    } else {
        "direct-lan"
    };
    let payload = invite_payload(
        &discovery_hostname(),
        &local_address,
        remote_address.as_deref(),
        port,
        &identity.fingerprint,
        &code.code,
        code.expires_at,
        route,
    );
    println!(
        "Waypad invite expires at unix timestamp: {}",
        code.expires_at
    );
    println!("Pairing code: {}", code.code);
    println!("Payload: {payload}");
    if qr {
        print_qr(&payload)?;
    } else {
        println!("Run `waypad-daemon invite --qr` to print a terminal QR code.");
    }
    if remote_address.is_some() && config.require_private_lan {
        println!(
            "Warning: config require_private_lan=true rejects public mobile-data clients. Set it to false only if this port is protected by pairing and firewall policy."
        );
    }
    Ok(())
}

fn invite_payload(
    host: &str,
    address: &str,
    remote_address: Option<&str>,
    port: u16,
    fingerprint: &str,
    code: &str,
    expires: u64,
    route: &str,
) -> String {
    let primary_address = remote_address.unwrap_or(address);
    let mut query = vec![
        ("v", "1".to_string()),
        ("host", host.to_string()),
        ("address", primary_address.to_string()),
        ("lan_address", address.to_string()),
        ("port", port.to_string()),
        ("fingerprint", fingerprint.to_string()),
        ("code", code.to_string()),
        ("expires", expires.to_string()),
        ("route", route.to_string()),
    ];
    if let Some(remote) = remote_address {
        query.push(("remote_address", remote.to_string()));
    }
    format!(
        "waypad://invite?{}",
        query
            .into_iter()
            .map(|(key, value)| format!("{key}={}", url_encode(&value)))
            .collect::<Vec<_>>()
            .join("&")
    )
}

fn print_qr(payload: &str) -> anyhow::Result<()> {
    let output = std::process::Command::new("qrencode")
        .args(["-t", "ANSIUTF8", "-m", "1", payload])
        .output()
        .context("qrencode is required for terminal QR output")?;
    if !output.status.success() {
        bail!(
            "qrencode failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn default_invite_address() -> String {
    command_output("ip", &["-4", "route", "get", "1.1.1.1"])
        .and_then(|raw| parse_ip_route_src(&raw))
        .or_else(|| {
            command_output("hostname", &["-I"]).and_then(|raw| {
                raw.split_whitespace()
                    .find(|part| part.contains('.') && *part != "127.0.0.1")
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| "127.0.0.1".into())
}

fn parse_ip_route_src(raw: &str) -> Option<String> {
    let mut parts = raw.split_whitespace();
    while let Some(part) = parts.next() {
        if part == "src" {
            return parts
                .next()
                .filter(|value| value.contains('.') && *value != "127.0.0.1")
                .map(str::to_string);
        }
    }
    None
}

fn discovery_hostname() -> String {
    command_output("hostname", &[])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "waypad-host".into())
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
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
  invite [--qr]                 Create a waypad:// invite; add --remote-address host for mobile data
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_payload_contains_pairing_metadata_and_remote_address() {
        let payload = invite_payload(
            "pc",
            "192.168.1.20",
            Some("203.0.113.10"),
            47771,
            "aa:bb",
            "123456",
            99,
            "direct-public",
        );

        assert!(payload.starts_with("waypad://invite?"));
        assert!(payload.contains("address=203.0.113.10"));
        assert!(payload.contains("lan_address=192.168.1.20"));
        assert!(payload.contains("remote_address=203.0.113.10"));
        assert!(payload.contains("code=123456"));
        assert!(payload.contains("fingerprint=aa%3Abb"));
    }

    #[test]
    fn url_encode_preserves_safe_chars_and_escapes_colons() {
        assert_eq!(url_encode("abc-_.~09"), "abc-_.~09");
        assert_eq!(url_encode("aa:bb"), "aa%3Abb");
    }

    #[test]
    fn parses_route_source_address_for_invites() {
        let raw = "1.1.1.1 via 192.168.1.1 dev wlan0 src 192.168.1.40 uid 1000";

        assert_eq!(parse_ip_route_src(raw), Some("192.168.1.40".to_string()));
        assert_eq!(
            parse_ip_route_src("local 127.0.0.1 dev lo src 127.0.0.1"),
            None
        );
    }
}
