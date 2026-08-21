//! `waypad://invite` deep links and the QR codes that carry them.
//!
//! Shared between the CLI and the local control panel so a QR scanned off a
//! terminal and one scanned off a web page are the same bytes. The payload
//! shape is part of the published protocol; see `docs/PROTOCOL.md`.
//!
//! Rendering is done in-process rather than by shelling out to `qrencode`,
//! which the Linux daemon used to require: a host that cannot print its own
//! invite is a host nobody can pair with, and that is too much to hang on an
//! optional package being installed.

use qrcode::{EcLevel, QrCode, render::unicode};

/// Where an invite says the phone should connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteRoute {
    /// LAN only. The safe default.
    DirectLan,
    /// A deliberately exposed public endpoint, for mobile-data testing.
    DirectPublic,
}

impl InviteRoute {
    fn as_str(self) -> &'static str {
        match self {
            Self::DirectLan => "direct-lan",
            Self::DirectPublic => "direct-public",
        }
    }
}

/// Everything a phone needs to find and trust this host, once.
#[derive(Debug, Clone)]
pub struct Invite {
    pub host: String,
    /// The endpoint the phone should try first.
    pub address: String,
    /// Always present as a fallback, even for a public invite.
    pub lan_address: String,
    pub remote_address: Option<String>,
    pub port: u16,
    pub fingerprint: String,
    pub code: String,
    pub expires_at: u64,
    pub route: InviteRoute,
    pub policy: &'static str,
    pub allow_public_pairing: bool,
}

impl Invite {
    /// Renders the deep link. The pairing code is embedded, so the payload is
    /// as sensitive as the code itself and just as short-lived.
    pub fn payload(&self) -> String {
        let mut query = vec![
            ("v", "1".to_string()),
            ("host", self.host.clone()),
            ("address", self.address.clone()),
            ("lan_address", self.lan_address.clone()),
            ("port", self.port.to_string()),
            ("fingerprint", self.fingerprint.clone()),
            ("code", self.code.clone()),
            ("expires", self.expires_at.to_string()),
            ("route", self.route.as_str().to_string()),
            ("policy", self.policy.to_string()),
            (
                "public_pairing_allowed",
                self.allow_public_pairing.to_string(),
            ),
        ];
        if let Some(remote) = &self.remote_address {
            query.push(("remote_address", remote.clone()));
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
}

/// Renders a payload as a QR code for a terminal.
///
/// Half-block characters put two QR rows in one text row, which is what keeps a
/// code with this much payload inside an 80x24 terminal.
pub fn qr_terminal(payload: &str) -> anyhow::Result<String> {
    let code = build_code(payload)?;
    Ok(code
        .render::<unicode::Dense1x2>()
        .quiet_zone(true)
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build())
}

/// Renders a payload as an SVG, for the local control panel.
pub fn qr_svg(payload: &str) -> anyhow::Result<String> {
    let code = build_code(payload)?;
    Ok(code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

fn build_code(payload: &str) -> anyhow::Result<QrCode> {
    // Low correction on purpose: an invite payload is long, and a phone
    // scanning a screen a foot away has no damage to correct for. Higher
    // levels push the code into a denser version that scans worse, not better.
    QrCode::with_error_correction_level(payload, EcLevel::L)
        .map_err(|err| anyhow::anyhow!("invite payload does not fit in a QR code: {err}"))
}

/// Percent-encodes everything outside the unreserved set.
///
/// Hand-rolled rather than pulled from a crate because the daemon encodes
/// exactly one kind of string and the rule is four lines long.
pub fn url_encode(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Invite {
        Invite {
            host: "pc".into(),
            address: "203.0.113.10".into(),
            lan_address: "192.168.1.20".into(),
            remote_address: Some("203.0.113.10".into()),
            port: 47771,
            fingerprint: "aa:bb".into(),
            code: "123456".into(),
            expires_at: 99,
            route: InviteRoute::DirectPublic,
            policy: "public-pairing",
            allow_public_pairing: true,
        }
    }

    #[test]
    fn invite_payload_contains_pairing_metadata_and_remote_address() {
        let payload = sample().payload();
        assert!(payload.starts_with("waypad://invite?"));
        assert!(payload.contains("address=203.0.113.10"));
        assert!(payload.contains("lan_address=192.168.1.20"));
        assert!(payload.contains("remote_address=203.0.113.10"));
        assert!(payload.contains("code=123456"));
        assert!(payload.contains("fingerprint=aa%3Abb"));
        assert!(payload.contains("policy=public-pairing"));
        assert!(payload.contains("public_pairing_allowed=true"));
    }

    #[test]
    fn a_lan_invite_keeps_the_lan_address_as_the_primary_endpoint() {
        let invite = Invite {
            address: "192.168.1.20".into(),
            remote_address: None,
            route: InviteRoute::DirectLan,
            policy: "lan-only",
            allow_public_pairing: false,
            ..sample()
        };
        let payload = invite.payload();
        assert!(payload.contains("route=direct-lan"));
        assert!(!payload.contains("remote_address"));
    }

    #[test]
    fn url_encode_preserves_safe_chars_and_escapes_colons() {
        assert_eq!(url_encode("abc-_.~09"), "abc-_.~09");
        assert_eq!(url_encode("aa:bb"), "aa%3Abb");
    }

    #[test]
    fn renders_a_real_invite_without_shelling_out() {
        // The whole point of doing this in-process: a host with no qrencode
        // installed must still be able to show a pairing code.
        let payload = sample().payload();
        let terminal = qr_terminal(&payload).expect("payload fits in a QR code");
        assert!(terminal.lines().count() > 10, "{terminal}");
        let svg = qr_svg(&payload).expect("payload fits in a QR code");
        assert!(svg.contains("<svg"), "{}", &svg[..80.min(svg.len())]);
        assert!(svg.contains("</svg>"));
    }
}
