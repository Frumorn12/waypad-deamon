# Waypad Protocol

Waypad uses a custom low-latency TCP protocol instead of HTTP polling. TCP is used for reliable ordering; cryptographic framing is implemented at the application layer so the Android client can pin the daemon host key without a local TLS PKI.

Default ports:

| Purpose | Port | Protocol |
| --- | ---: | --- |
| Discovery | 47770 | UDP broadcast |
| Control | 47771 | TCP with encrypted Waypad frames |
| Screen stream | 47771 | Token-attached TCP frame stream on the control listener |

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
  "input_supported": true,
  "capture_backend": "wayland-screencast-portal",
  "capture_supported": true
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

The daemon can also print an expiring QR invite:

```bash
waypad-daemon invite --qr
```

The payload is a normal deep link, for example:

```text
waypad://invite?v=1&host=desktop&address=192.168.0.184&lan_address=192.168.0.184&port=47771&fingerprint=aa%3Abb&code=123456&expires=1770000000&route=direct-lan
```

`--remote-address <host-or-ip>` adds `remote_address` and marks the route as
`direct-public`. For remote invites, `address` is the public endpoint and
`lan_address` remains present as a fallback. Android treats the endpoints as
ordered candidates. The embedded pairing code remains short-lived and single
use; the signed handshake and host fingerprint pinning are still mandatory.

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
| `pointer_move_absolute` | Source-local absolute pointer motion for remote screen control. |
| `pointer_button` | Left, middle, or right button press/release. |
| `scroll` | Smooth pointer-axis scroll. |
| `external_input` | Normalized input events from Android-attached mouse, keyboard, touchpad, or controller devices. |
| `key` | XKB keysym press/release. |
| `text` | Sends characters as keysyms. |
| `shortcut` | Sends a validated shortcut sequence. |
| `media` | `playerctl` media actions. |
| `volume` | `wpctl` or `pactl` volume actions. |
| `brightness` | `brightnessctl` brightness actions. |
| `clipboard_set` | Sets Wayland clipboard via `wl-copy`. |
| `list_screen_sources` | Lists portal picker and/or concrete monitor sources. |
| `start_screen_stream` | Starts a token-protected screen frame stream. |
| `stop_screen_stream` | Stops a running screen stream session. |
| `request_key_frame` | Forces an immediate H.264 keyframe on a running session. |
| `system` | Lock or suspend. Suspend is disabled by default. |

Unsupported commands return an authenticated error with a user-facing reason.

## External Android Input

The Android client forwards hardware devices connected to the phone with:

```json
{
  "name": "external_input",
  "device_id": "android:7:abcd1234",
  "device_type": "keyboard",
  "event": {
    "type": "keyboard_key",
    "keysym": 97,
    "state": "pressed",
    "repeat": false
  }
}
```

`device_type` is one of `keyboard`, `mouse`, `touchpad`, `gamepad`, `joystick`, or `unknown`. Event types are:

| Event | Host behavior |
| --- | --- |
| `device_connected` / `device_disconnected` | Logged for diagnostics. |
| `pointer_move` | Relative pointer motion through the active backend. |
| `pointer_button` | Left/middle/right button through the active backend. |
| `pointer_scroll` | Smooth scroll through the active backend. |
| `keyboard_key` | XKB keysym press/release through the active backend. |
| `controller_button` / `controller_axis` | Sent to the Linux `uinput` virtual gamepad backend when `external_input.controller` is true. |

`get_capabilities` includes `external_input.pointer`, `external_input.keyboard`, and `external_input.controller`. Pointer and keyboard follow the current input backend. Controller support is independent and reflects whether the daemon user can open `/dev/uinput`; current Wayland portal and Hyprland IPC APIs do not provide generic gamepad injection themselves.

## Screen Sources

`list_screen_sources` returns:

```json
{
  "sources": [
    {
      "id": "hyprland:monitor:DP-1",
      "label": "DP-1 (monitor description)",
      "kind": "monitor",
      "backend": "hyprland-grim",
      "width": 1920,
      "height": 1080,
      "x": 0,
      "y": 0,
      "scale": 1.0,
      "focused": true
    }
  ]
}
```

When the standard portal path is available, the daemon also exposes `portal:chooser`. The actual monitor/window is selected locally through the compositor portal dialog.

## Screen Stream

The Android app starts a stream with:

```json
{
  "name": "start_screen_stream",
  "source_id": "hyprland:monitor:DP-1",
  "max_fps": 60,
  "jpeg_quality": 58,
  "bitrate_kbps": 6000,
  "max_width": 1280,
  "max_height": 1280,
  "audio": true,
  "audio_bitrate_kbps": 96,
  "audio_frame_ms": 20
}
```

`bitrate_kbps` is optional and only steers the H.264 encoder. Clients that omit
it keep driving the stream through `jpeg_quality`, which the daemon maps onto a
bitrate.

`audio` is optional and defaults to **true**: desktop audio rides along on the
same socket unless the client asks for a silent stream with `"audio": false`.
`audio_bitrate_kbps` is clamped to `32..256` and `audio_frame_ms` snaps to `10`
or `20`. See [Desktop audio](#desktop-audio) below.

The daemon replies:

```json
{
  "session_id": "...",
  "stream_port": 47771,
  "token": "...",
  "codec": "h264",
  "transport": "waypad-control-port-stream-v2",
  "source": { "id": "hyprland:monitor:DP-1" },
  "actual_fps": 30,
  "actual_quality": 58,
  "actual_bitrate_kbps": 6000,
  "audio": {
    "running": false,
    "muted": false,
    "codec": "opus",
    "sample_rate": 48000,
    "channels": 2,
    "frame_ms": 20,
    "bitrate_kbps": 96,
    "monitor_source": null,
    "packets_sent": 0,
    "packets_dropped": 0,
    "reason": "Desktop audio is captured from the monitor of the current default sink (...)"
  }
}
```

`audio.running` is still false here: the audio producer writes to the stream
socket, so it only starts once a client attaches. `reason` explains why audio is
unavailable when it is.

`codec` is advisory: it is `h264` when the session will run on the PipeWire
pipeline with a working H.264 encoder, and `jpeg` otherwise. The handshake line
below is what actually decides how the payloads must be decoded.

For `waypad-control-port-stream-v2`, the app opens a fresh TCP connection to `stream_port` and writes this JSON line before any encrypted control-channel handshake:

```json
{"type":"stream_connect","token":"..."}
```

The daemon peeks at new TCP connections on the control listener. If the first line is a valid `stream_connect` token for a pending screen session, that socket is attached to the stream producer. Just before the first frame it receives one handshake line naming the payload codec:

```text
WAYPAD_STREAM_V2\n   H.264 payloads
WAYPAD_STREAM_V1\n   JPEG payloads
```

The line is written lazily, once the producer has actually encoded something,
because the portal path may still fall back to the JPEG `grim` producer while
the socket is untouched. Clients must therefore wait for the handshake as long
as they are willing to wait for the first frame; portal approval happens in
between.

Frames then repeat, identically for both versions:

```text
u32_be header_length
u32_be payload_length
header_length bytes of UTF-8 JSON
payload_length bytes of payload
```

`WAYPAD_STREAM_V1` headers contain `seq`, `timestamp_ms`, `codec` (`jpeg`),
`width`, and `height`, and the payload is a complete JPEG image.

`WAYPAD_STREAM_V2` headers add two flags:

```json
{"seq": 0, "timestamp_ms": 0, "width": 1920, "height": 1080,
 "codec": "h264", "key_frame": true, "config": false}
```

- The payload is one or more H.264 NAL units in Annex-B form, with `00 00 00 01`
  or `00 00 01` start codes.
- `config: true` marks a codec-config payload carrying only SPS/PPS. It is sent
  ahead of the first keyframe and repeated ahead of **every** later keyframe,
  which is what lets a client rebuild its decoder mid-stream. The same parameter
  sets also stay inline in the keyframe payload, so a decoder that ignores
  config frames still finds them before the IDR.
- `key_frame: true` marks an IDR access unit. Everything else is a single
  non-IDR access unit that depends on the frames before it, so payloads must be
  fed to the decoder in order and none may be skipped.
- `seq` counts envelopes, not pictures: a config frame consumes a `seq` of its
  own.
- `width` and `height` describe the encoded video, which is the downscaled size
  when `max_width`/`max_height` asked for one.
- The stream always opens on a keyframe. The pipeline is started per attached
  client, so its first encoded picture is an IDR and nothing is forwarded before
  it.

### Requesting a keyframe

A client whose decoder was destroyed and rebuilt — on Android this happens every
time the app returns to the foreground and the `SurfaceView` recreates its
`Surface` — stays black until it sees SPS/PPS followed by an IDR. It asks for one
on the control channel:

```json
{"name": "request_key_frame", "session_id": "..."}
```

The daemon replies with an empty ok. The next frames on the stream are a
`config: true` payload followed by a `key_frame: true` IDR. Requests are
coalesced and rate limited to one per second; a request for a session that has
not attached a client yet succeeds and does nothing, because such a stream
always opens on a keyframe anyway. An unknown or expired `session_id` is an
error.

Because the daemon drives GStreamer through `gst-launch-1.0`, there is no way to
push a force-key-unit event into a running pipeline: the keyframe is served by
respawning the encoder, which costs a few hundred milliseconds of stream gap.
JPEG sessions accept the command and ignore it, since every JPEG frame is
already a keyframe.

## Desktop audio

Audio shares the screen stream socket. There is no second connection, no second
port and no second handshake: audio envelopes are simply interleaved with the
video ones and told apart by their `codec` field, which is what the Android
client already routes on. Envelopes are written whole under one lock, so the two
producers can never cut each other's frames.

```json
{"seq": 0, "timestamp_ms": 0, "codec": "opus", "sample_rate": 48000,
 "channels": 2, "frame_ms": 20, "pre_skip": 312,
 "key_frame": false, "config": false}
```

- The payload is **one bare Opus packet**, no container and no RTP header.
- `seq` counts audio envelopes only; it is a separate sequence from the video one.
- `key_frame` and `config` are **always false**, and that is load bearing. The
  Android batch pruner keeps everything from the last `key_frame` onwards plus
  the last `config` before it, without looking at the codec, so an audio
  envelope claiming either flag would make the client drop video frames or evict
  the H.264 parameter sets. Audio stays invisible to the video drop policy.
- There is consequently **no audio config envelope**. Everything a decoder needs
  is repeated on every packet as plain header fields, so a client that joins late
  or loses a packet can always (re)build its decoder from the next one. Android
  synthesises the `csd-0` *OpusHead*, `csd-1` pre-skip and `csd-2` seek pre-roll
  buffers from `channels`, `sample_rate` and `pre_skip`.
- Audio packets are droppable. Losing one costs a `frame_ms` gap and nothing else.

Capture is the monitor of the **current default sink**, resolved at stream start
from `pactl get-default-sink`, never hardcoded, so switching output device is
picked up. Encoding is Opus at 48 kHz stereo, `audio-type=generic`
(`OPUS_APPLICATION_AUDIO`), short frames, and `bitrate-type=constrained-vbr`
with `dtx=true`: a sink monitor never stops producing samples, so a silent
desktop is a full stream of digital silence, and constant bitrate would spend the
whole budget on it. Measured, 10 s of silence costs 127 kB under `cbr` and 7.5 kB
under `constrained-vbr`, while real content costs the same either way. DTX is
paired with that setting rather than used alone because libopus ignores it under
hard CBR.

Audio never affects video. It runs in its own task, and a failing pipeline is
logged at `error` and leaves the picture untouched.

### Audio commands

```json
{"name": "start_desktop_audio", "session_id": "...", "bitrate_kbps": 96, "frame_ms": 20}
{"name": "stop_desktop_audio", "session_id": "..."}
{"name": "set_desktop_audio_mute", "session_id": "...", "muted": true}
{"name": "get_desktop_audio_status", "session_id": "..."}
```

All four reply with the `audio` object shown above. `bitrate_kbps` and `frame_ms`
are optional on `start_desktop_audio`.

Muting acts on the **host**: the encoder keeps running so unmuting is instant,
but no envelope is sent, so a muted stream costs no bandwidth. A client that also
silences its own speaker gets an instant response locally and stops paying for
the stream a moment later.

`max_fps` is clamped to `1..60`, `jpeg_quality` to `35..92`, `bitrate_kbps` to
`500..40000`, and maximum dimensions to `480..3840`. When a maximum dimension is
lower than the source size, the daemon downscales before encoding to reduce
bandwidth and decode latency. H.264 runs CBR with a keyframe every two seconds
and no B-frames. JPEG frame pacing skips missed ticks so slow captures drop
stale frames instead of building a long queue; H.264 never drops encoded frames
on the socket, because a dropped frame would break every frame referencing it,
and instead lets backpressure reach the leaky queue in front of the encoder.

The current MVP stream is token-protected but not encrypted independently; it is intended for trusted LAN, VPN, or deliberately exposed direct-public testing. The authenticated control channel remains encrypted. Older builds used a dynamic per-session stream port, but current builds reuse the stable control port so phone clients are not broken by LAN firewalls or NAT rules that block random high ports. A future WebRTC transport can replace this frame stream while keeping the source and input commands.
