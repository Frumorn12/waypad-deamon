# Waypad Protocol

Waypad uses a custom low-latency TCP protocol instead of HTTP polling. TCP is used for reliable ordering; cryptographic framing is implemented at the application layer so the Android client can pin the daemon host key without a local TLS PKI.

Default ports:

| Purpose | Port | Protocol |
| --- | ---: | --- |
| Discovery | 47770 | UDP broadcast |
| Control | 47771 | TCP with encrypted Waypad frames |

## Discovery

The Android app broadcasts:

```text
WAYPAD_DISCOVER_V1
```

The daemon replies with JSON:

```json
{
  "service": "dev.waypad.daemon",
  "protocol": 1,
  "host_name": "desktop",
  "control_port": 47771,
  "host_fingerprint": "abcd:...",
  "input_backend": "wayland-portal",
  "input_supported": true
}
```

Discovery is only a convenience mechanism. Trust is established by the signed TCP handshake and pairing code.

## Handshake

The client sends a plaintext `client_hello` containing an ephemeral P-256 ECDH public key. The daemon replies with:

- Daemon long-term P-256 ECDSA public key.
- Daemon host fingerprint, computed as SHA-256 over the host public key and grouped as colon-separated hex.
- Daemon ephemeral P-256 ECDH public key.
- 32-byte random session nonce.
- ECDSA signature over `WAYPAD-HANDSHAKE-v1 || client_ephemeral || server_ephemeral || session_nonce`.

The client verifies the signature and checks the fingerprint against discovery, QR/manual pairing data, or a previously pinned host.

## Encrypted Frames

Both sides derive keys with HKDF-SHA256:

- Salt: session nonce.
- Input key material: ECDH shared secret.
- Client-to-server info: `waypad v1 c2s`.
- Server-to-client info: `waypad v1 s2c`.

Frames are newline-delimited JSON:

```json
{
  "seq": 0,
  "ciphertext": "base64(aes-gcm-json)"
}
```

AES-GCM nonces are `C2S\0 || seq_u64_be` or `S2C\0 || seq_u64_be`. The sequence number is also authenticated as AEAD additional data. Receivers reject out-of-order frames.

## Pairing

Pairing requires a local code:

```bash
waypad-daemon pair-code
```

The app sends an encrypted `pair_request` with the code and Android device name. If valid, the daemon creates a trusted device, stores only a SHA-256 hash of the random 256-bit session token, and returns the token once.

## Authentication

The app sends an encrypted `auth_request` with:

```json
{
  "type": "auth_request",
  "request_id": "...",
  "device_id": "...",
  "session_token": "...",
  "app_version": "0.1.0"
}
```

The daemon rejects all control commands until authentication succeeds.

## Commands

Each command is encrypted:

```json
{
  "type": "command",
  "request_id": "...",
  "command": {
    "name": "pointer_move",
    "dx": 12.0,
    "dy": -3.5
  }
}
```

Current command names:

| Command | Purpose |
| --- | --- |
| `get_health` | Service health. |
| `get_host_info` | Host name, fingerprint, protocol. |
| `get_capabilities` | Wayland, portal, and system capability model. |
| `prepare_input` | Starts the RemoteDesktop portal approval flow. |
| `pointer_move` | Relative pointer motion. |
| `pointer_button` | Left, middle, or right button press/release. |
| `scroll` | Smooth pointer-axis scroll. |
| `key` | XKB keysym press/release. |
| `text` | Sends characters as keysyms. |
| `shortcut` | Sends a validated shortcut sequence. |
| `media` | `playerctl` media actions. |
| `volume` | `wpctl` or `pactl` volume actions. |
| `brightness` | `brightnessctl` brightness actions. |
| `clipboard_set` | Sets Wayland clipboard via `wl-copy`. |
| `system` | Lock or suspend. Suspend is disabled by default. |

Unsupported commands return an authenticated error with a user-facing reason.
