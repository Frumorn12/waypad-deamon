# Architecture

Waypad-daemon is a user-session Linux daemon. It is intentionally not a root system service because Wayland input authorization, xdg-desktop-portal, and compositor consent are scoped to the logged-in graphical session.

## Components

| Module | Responsibility |
| --- | --- |
| `config` | JSON config loading and defaults. |
| `state` | Host identity, pairing code, trusted devices, private file permissions. |
| `crypto` | P-256 handshake, host signatures, HKDF, AES-GCM frame encryption. |
| `discovery` | UDP LAN discovery. |
| `server` | TCP listener, authentication, command routing, rate limiting. |
| `capability` | Session, portal, libei, connectivity, and system helper detection. |
| `input` | Wayland RemoteDesktop portal backend and unsupported fallback. |
| `screen` | Screen source enumeration, ScreenCast/PipeWire stream sessions, and Hyprland capture fallback. |
| `system_control` | Volume, media, brightness, clipboard, lock, suspend commands. |
| `platform` | Environment and compositor detection helpers. |

## Wayland Input Strategy

Waypad does not use `xdotool` or XTest because those are X11-era mechanisms and do not model Wayland security. The supported path is:

1. Detect a Wayland session.
2. Detect `org.freedesktop.portal.Desktop` on the session bus.
3. Detect `org.freedesktop.portal.RemoteDesktop`.
4. Request keyboard/pointer devices through the portal.
5. Wait for local user approval.
6. Send input through `NotifyPointer*` and `NotifyKeyboard*` portal methods.

If portal support is missing or approval is denied, input commands fail with explicit messages. This is expected on some Hyprland/wlroots setups depending on installed portal backend and version.

The daemon also detects whether RemoteDesktop version 2 may expose `ConnectToEIS`, and whether libei appears installed, but MVP input uses portal Notify methods. libei event sending is the next backend extension point.

When Hyprland is detected and the RemoteDesktop portal is missing, the daemon can use a `hyprland-ipc` backend. This talks to Hyprland's user-session IPC socket, not root/uinput, and is isolated behind the same `InputManager` abstraction as the portal backend. It supports cursor movement, mouse button state, scroll wheel events, shortcuts, and direct ASCII text events. Unsupported text falls back to `wl-copy` paste, so only that fallback path temporarily replaces the Wayland clipboard.

External Android mouse and keyboard devices use the same input abstraction as touchpad and remote-screen input. The protocol keeps Android device metadata and normalized event types, but host-side pointer and keyboard events still terminate in RemoteDesktop portal methods or the Hyprland IPC fallback.

Controller/gamepad forwarding uses a separate Linux `uinput` backend because current Wayland RemoteDesktop portal methods and the Hyprland IPC fallback do not expose a generic virtual gamepad injection API. When `/dev/uinput` is writable by the daemon user, controller buttons and axes are mapped to a standard "Waypad Android Virtual Gamepad" device using Linux `BTN_*` and `ABS_*` codes. This is intentionally isolated from pointer/keyboard injection so Wayland portal behavior remains compositor-scoped while gamepad support uses the kernel input API that browser Gamepad APIs already understand.

## Remote Screen Strategy

Remote screen support is intentionally Wayland-first:

1. Detect `org.freedesktop.portal.ScreenCast`.
2. Detect PipeWire runtime availability.
3. Detect a usable GStreamer `pipewiresrc` pipeline with an H.264 or JPEG encoder.
4. Offer a portal source picker when the standard portal stack is usable.
5. On Hyprland only, offer an isolated `hyprland-grim` monitor fallback when portal streaming dependencies are incomplete.

The control channel negotiates a short-lived stream session and token. The Android app then opens a second LAN TCP connection to the daemon's stable control port, sends a `stream_connect` JSON line with that token, and receives a handshake line followed by JSON-header framed payloads. Reusing the control listener avoids dynamic high-port failures on real phones and keeps the MVP small and shippable without adding a partial WebRTC stack. The transport is designed so a future WebRTC backend can replace the frame stream without changing source selection or input mapping commands.

## Screen Encoding

The PipeWire path encodes H.264 and announces `WAYPAD_STREAM_V2`. MJPEG cost
150-300 KB per 1080p frame, which is 45-90 Mbit/s at 30 FPS: enough to saturate
Wi-Fi, push the TCP socket into backpressure, and leave the phone rendering
seconds-old frames. H.264 reaches comparable quality near 5-10 Mbit/s.

The encoder is chosen once per process by actually running each candidate on a
one-buffer pipeline, because having the plugin installed does not prove it can
initialise — NVENC additionally needs a working CUDA context on this hybrid
Intel/NVIDIA setup. `nvh264enc` is tried first, then `x264enc` with
`tune=zerolatency speed-preset=veryfast`; when neither starts, the pipeline
falls back to `jpegenc` and the stream stays on `WAYPAD_STREAM_V1`. The selected
encoder is logged and reported as `capture.h264_encoder` in `doctor`.

Both encoders run CBR with no B-frames, zero reordering delay, and a keyframe
every two seconds. Keyframes on connect need no explicit force: the pipeline is
spawned per attached client, so its first picture is always an IDR, and the
reader forwards nothing before it. SPS/PPS are repeated ahead of every keyframe,
not just the first, because a client whose decoder is rebuilt mid-stream cannot
restart without them. A leaky queue sits in front of the encoder so backpressure
drops raw frames, never encoded ones; the queue after the encoder is deliberately
not leaky.

`request_key_frame` serves an on-demand IDR. Driving GStreamer through
`gst-launch-1.0` leaves no way to push a force-key-unit event into a running
pipeline, so the encoder is respawned instead: a fresh pipeline always opens on
an IDR with its parameter sets. Requests coalesce and are rate limited to one
per second so a client cannot make the pipeline thrash. This is why the portal
session and its PipeWire fd are held for the whole stream rather than dropped
right after the first spawn.

## Pipeline Negotiation

A GStreamer capsfilter constrains, it never converts. Anything it pins that the
elements in front of it cannot change travels back upstream as a requirement on
the source. `pipewiresrc` cannot satisfy an arbitrary size or frame rate, so it
answers `error set output format: -22 (Invalid argument)` and the pipeline never
prerolls. That is why `videorate`, `videoscale`, and `videoconvert` all sit
between the source and the capsfilter: rate, size, and pixel format each need
their own converter. Removing any one of them silently breaks the fast path and
drops the stream onto the `grim` fallback, which caps out near 6 FPS — a failure
mode that looks like slowness rather than an error, so
`pipeline_can_convert_everything_the_capsfilter_pins` guards it in the tests.

`videorate` runs with `drop-only=true max-rate=FPS` instead of a fixed frame rate
in the capsfilter: it never duplicates frames, so a still screen costs no
bandwidth, and it never holds a buffer back to pace output. Square pixels are
requested explicitly, because sources that negotiate a non-square aspect leave
the phone unable to correct it.

Because portal frames arrive in system memory (PipeWire is pinned to SHM to work
around DMA-BUF problems with the NVIDIA driver), `videoconvert` produces NV12 for
NVENC or I420 for x264 on the CPU.

When the fast path fails, the reason is logged at `error` with the producer's own
stderr and reported back to the app as `portal_last_error` on the next
`start_screen_stream`. The `grim` fallback still renders a picture, so a
permanently broken portal pipeline is otherwise invisible.

The daemon splits the encoder's Annex-B byte stream back into whole access units
before framing them, since pipe reads land on arbitrary boundaries. NAL units are
only cut once the start code of the next one has been seen, boundaries follow
AUD/parameter-set/`first_mb_in_slice` rules so multi-slice pictures stay whole,
and the buffered picture is released early once the producer pipe goes idle to
avoid holding a frame for a full frame interval.

Stream settings are not cosmetic. Android sends `max_fps`, `jpeg_quality`,
`bitrate_kbps`, `max_width`, and `max_height` in `start_screen_stream`. The
daemon clamps FPS to 1..60, JPEG quality to 35..92, bitrate to 500..40000 kbit/s,
and maximum dimensions to 480..3840. Clients that predate H.264 never send a
bitrate, so `jpeg_quality` is mapped onto bits per pixel and combined with the
encoded resolution and frame rate, which puts 1080p30 at quality 70 near
7 Mbit/s. Hyprland `grim` capture uses the requested scale before JPEG encoding.

Portal capture requires local user approval. Hyprland `grim` capture is compositor-specific and deliberately isolated behind the `ScreenManager`; it is not treated as a general Wayland backend.

Absolute pointer control uses the existing input abstraction. Hyprland maps source-local coordinates to global compositor coordinates and dispatches `movecursor`. The RemoteDesktop portal path exposes absolute motion through `NotifyPointerMotionAbsolute`, but some portal backends require a shared screencast stream id; those failures are surfaced to the Android app instead of silently falling back to incorrect coordinates.

## Security Boundaries

The daemon binds to LAN by default but still treats the LAN as hostile:

- Unknown clients only receive a signed handshake.
- Pairing requires a local one-time code, rate-limited per IP.
- Commands require authentication.
- Device tokens can be revoked.
- Host key rotation invalidates existing trust.
- Pairing from public internet source addresses is rejected when
  `require_private_lan=true` and `allow_public_pairing=false`.
- **Already-paired devices can reconnect from any IP** because they hold
  cryptographically strong session tokens.

QR invites are expiring pairing helpers, not permanent credentials. The
`invite` command creates a normal one-time pairing code, embeds it in a
`waypad://invite` payload with the host fingerprint, endpoint hints, port,
route, expiry, and pairing policy (`lan-only`, `public-pairing`, or
`public-reconnect`). When a remote endpoint is supplied, the payload includes
both `remote_address` and `lan_address`; Android can try the public/direct
endpoint first and then fall back to LAN. The app still verifies the signed
daemon handshake and pins the host key before trusting the connection.

## Connectivity Model

Current builds support direct TCP connectivity:

- LAN direct through discovery, manual IP entry, or `waypad-daemon invite --qr`.
- Public direct through `waypad-daemon invite --qr --remote-address <host>` when
  the user intentionally exposes TCP `47771`.
- Pairing from public IPs is allowed only when the user explicitly opts in
  (`allow_public_pairing=true` or `require_private_lan=false`).
- Reconnection from public IPs is always allowed for already-paired devices;
  token authentication is the security boundary, not source IP.

The daemon reports this in `capabilities.connectivity`, including
`public_pairing_allowed`. It also explicitly reports that relay, signaling,
STUN, and TURN are not available. That keeps outside-LAN behavior honest: port
forwarding or a VPN can work today, but automatic NAT traversal requires a
future WebRTC/ICE/TURN backend.

## Service Model

Use `systemd --user`, not a system service:

- The daemon needs the user D-Bus session bus.
- Portal dialogs must appear in the user's graphical session.
- Wayland compositor permissions are user-session scoped.
- Running as root would not grant correct Wayland authority and would increase risk.
