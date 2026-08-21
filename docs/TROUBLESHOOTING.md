# Troubleshooting

## Run Diagnostics

```bash
waypad-daemon doctor
```

Watch logs:

```bash
journalctl --user -u waypad-daemon -f
```

## Hyprland on Arch or CachyOS

Install portal and useful helpers:

```bash
sudo pacman -S xdg-desktop-portal xdg-desktop-portal-hyprland wireplumber playerctl brightnessctl wl-clipboard
systemctl --user restart xdg-desktop-portal xdg-desktop-portal-hyprland
waypad-daemon doctor
```

If `RemoteDesktop` is unavailable, the daemon cannot use the portal input path. This is a compositor/portal capability issue, not an Android networking issue.

On Hyprland, Waypad may still expose the `hyprland-ipc` fallback. That backend talks to the user-session Hyprland IPC socket directly, supports pointer motion, mouse button hold/release, wheel-style scrolling, shortcuts, and direct ASCII text events. Unsupported text falls back to `wl-copy` paste, which requires `wl-clipboard` and temporarily replaces the current Wayland clipboard.

For screen viewing on Hyprland, install PipeWire/GStreamer helpers if you want the standard portal stream path:

```bash
sudo pacman -S pipewire wireplumber gst-plugin-pipewire gst-plugins-good grim
systemctl --user restart pipewire wireplumber xdg-desktop-portal xdg-desktop-portal-hyprland
waypad-daemon doctor
```

If the portal stream path is incomplete but Hyprland and `grim` are available, Waypad exposes concrete monitor sources through the isolated `hyprland-grim` fallback.

## "Remote input unavailable: RemoteDesktop portal not available"

Check:

```bash
busctl --user tree org.freedesktop.portal.Desktop
pacman -Qs xdg-desktop-portal
systemctl --user status xdg-desktop-portal xdg-desktop-portal-hyprland
```

Hyprland users should ensure `xdg-desktop-portal-hyprland` is installed and not masked.

## "Screen capture unavailable: ScreenCast portal not available"

Check the ScreenCast portal:

```bash
busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.ScreenCast --no-pager
systemctl --user status xdg-desktop-portal
```

On Hyprland, ensure `xdg-desktop-portal-hyprland` is installed and running. On GNOME/KDE, use the desktop's portal backend and update the portal packages if the interface is missing.

## "PipeWire capture could not be initialized"

Check PipeWire and GStreamer:

```bash
systemctl --user status pipewire wireplumber
gst-inspect-1.0 pipewiresrc
gst-inspect-1.0 h264parse
gst-inspect-1.0 nvh264enc   # NVIDIA hardware encoder, gst-plugins-bad
gst-inspect-1.0 x264enc     # software fallback, gst-plugins-ugly
gst-inspect-1.0 jpegenc     # last-resort MJPEG fallback
```

If `pipewiresrc` is missing, install the PipeWire GStreamer plugin package for your distribution.

`waypad-daemon doctor` reports the encoder the daemon actually managed to start
as `capture.h264_encoder`. Note that an installed plugin is not enough: NVENC
also needs a usable CUDA context, so the daemon probes each candidate on a
one-buffer pipeline at startup and logs `H.264 screen encoder selected`.

## Picture Works But There Is No Sound

Audio is optional and deliberately cannot take the video down with it, so a
broken audio pipeline looks exactly like a normal stream with no sound. It is
never silent about it, though: check the capability first, then the log.

```bash
waypad-daemon doctor | grep -A11 audio_capture
journalctl --user -u waypad-daemon | grep -i 'desktop audio'
```

`audio_capture.reason` names what is missing — `pactl` absent, a GStreamer
element missing (the list is in `missing_elements`), or no monitor source exposed
by any output device. If the capability is `supported: true` but there is still
no sound, the failure happened at stream time and was logged at `error` level
with the reason attached; it is never swallowed.

`audio_capture.monitor_source` shows which device the daemon *would* capture. It
must be the `.monitor` of your current output. The daemon re-resolves it every
time a stream starts, so if you switched output device, restart the stream rather
than the daemon.

To confirm the capture path independently of Waypad:

```bash
pactl get-default-sink
pactl list short sources | grep monitor
```

On the phone, the audio path logs under the `WaypadAudioPlayer` tag:

```bash
adb logcat -s WaypadAudioPlayer
```

`codec_started` and `track_started` mean the decoder and the output track came
up. `bufferedMs` is the audio latency actually queued for the speaker and should
sit near 100 ms; `driftMs` is the transit delay above the fastest packet of the
session and should stay near zero. A `dropped` counter that keeps climbing means
the link cannot keep up and the latency guard is cutting the backlog.

Sound stops but the picture continues after a phone call or another app playing
audio: that is audio focus working as intended. It resumes when the other app
gives the focus back.

## Stream Starts But Input Fails

This is a normal partial-support case. Capture and control are separate capabilities. The app can show the screen while the daemon reports that RemoteDesktop input is blocked or unsupported.

For portal input, tap "Approve portal" in the app and approve pointer/keyboard control on the Linux host. For Hyprland fallback, confirm `waypad-daemon doctor` reports `input.backend = hyprland-ipc`.

## External Mouse Or Keyboard On Android Does Nothing

The Android app forwards external devices only while connected in Pad or Screen mode. On the host, check capabilities:

```bash
waypad-daemon doctor | grep -A8 external_input
journalctl --user -u waypad-daemon -f
```

`external_input.pointer` and `external_input.keyboard` follow the normal input backend. If they are false, fix RemoteDesktop portal approval or the Hyprland IPC fallback first. If Android logs show `external_input_unsupported`, the host is explicitly rejecting that class rather than dropping it silently.

## Controller Or Gamepad Forwarding Does Not Work

Android controller detection and protocol transport are implemented. The host-side injection path uses Linux `uinput`, so first check:

```bash
waypad-daemon doctor | grep -A8 external_input
ls -l /dev/uinput
journalctl --user -u waypad-daemon -f
```

If `external_input.controller = true`, open the remote screen in Waypad, keep the Android app focused/fullscreen, and press a controller button. The daemon should log that an Android controller attached to the virtual gamepad, and browser tests such as `hardwaretester.com/gamepad` should see `Waypad Android Virtual Gamepad` on the PC.

If `external_input.controller = false`, the reason usually says `/dev/uinput` is missing or not writable. Load the `uinput` kernel module and add a udev rule or group policy that allows the Waypad user to open `/dev/uinput`; do not run the whole daemon as root just for controller support. After changing permissions, restart `waypad-daemon`.

## Android Reports "Connection Closed" Or "Broken Pipe"

Watch daemon logs while pressing Start in the Android Screen tab:

```bash
journalctl --user -u waypad-daemon -f
```

Healthy current logs show:

```text
screen stream session pending client attach ... stream_port=47771
screen stream attach request received on control port
screen stream client attached ...
```

If logs show a random high `stream_port`, the Android app and daemon are from mismatched builds. Rebuild the daemon, install it, and restart the user service:

```bash
cargo build --release
install -Dm755 target/release/waypad-daemon ~/.local/bin/waypad-daemon
systemctl --user restart waypad-daemon
```

If Android still cannot connect to `47771`, confirm the daemon is listening and the phone can reach the host IP:

```bash
ss -ltnp | grep 47771
ip -4 addr
```

## QR Invite Shows 127.0.0.1 Or The Phone Cannot Connect

Current builds choose the LAN address from the active IPv4 route:

```bash
ip -4 route get 1.1.1.1
waypad-daemon invite --qr
```

The QR payload should contain the `src` address from that route, not
`127.0.0.1`. If the phone must use a different interface, pass it explicitly:

```bash
waypad-daemon invite --qr --address 192.168.0.184
```

For mobile-data/outside-LAN pairing, expose the daemon's TCP port intentionally
and provide the reachable public endpoint:

```bash
waypad-daemon invite --qr --remote-address your-public-hostname.example
```

This is direct TCP. The daemon does not provide a relay, STUN, TURN, or automatic
ICE traversal yet.

### Pairing policy on the QR

The QR now includes a `policy` field that tells the Android app whether remote
pairing is actually allowed:

- `lan-only` — the QR has no public endpoint. Works only on the same network.
- `public-pairing` — the QR has a public endpoint and the daemon is configured
to accept new pairing attempts from public IPs (`allow_public_pairing=true` or
`require_private_lan=false`).
- `public-reconnect` — the QR has a public endpoint, but the daemon currently
**blocks new pairing from public IPs** (`require_private_lan=true` and
`allow_public_pairing=false`). Already-paired devices can still reconnect from
mobile data, but a new phone scanning this QR will be rejected with a clear
error.

### Fixing "Remote pairing blocked by host policy"

If the Android app shows this error after scanning a QR on mobile data, the
daemon config is blocking public pairing. Choose one fix:

**Option A — Recommended (keeps LAN-only restriction for reconnection):**
```bash
# edit ~/.config/waypad-daemon/config.json
# add or set:
#   "allow_public_pairing": true
systemctl --user restart waypad-daemon
```

**Option B — Legacy (allows all public traffic):**
```bash
# edit ~/.config/waypad-daemon/config.json
# set:
#   "require_private_lan": false
systemctl --user restart waypad-daemon
```

Only do this if TCP `47771` is port-forwarded and protected by your firewall.
Pairing still requires the one-time 6-digit code, and all traffic is encrypted.

With `--remote-address`, the QR also includes `lan_address`. Android clients try
the public/direct endpoint first and then the LAN endpoint, so the same QR is
usable when the phone is on mobile data or on the same Wi-Fi. If both fail, the
advertised endpoint is unreachable or the daemon is rejecting that source
address.

## "Remote pairing blocked by host policy"

See the section above [QR Invite Shows 127.0.0.1 Or The Phone Cannot Connect].
The daemon is correctly telling the Android app that it refuses new pairing
from public networks. Either pair while on the same LAN, or set
`allow_public_pairing=true` in the daemon config after ensuring your firewall
restricts TCP `47771` appropriately.

## Stream Is Very Slow (~10 FPS Average)

### Grim is limited to ~10 FPS by design

The grim backend takes a full JPEG screenshot each frame using the `grim`
tool. Each screenshot cycle takes ~100-110ms on Hyprland at 432p. This
gives a theoretical maximum of ~9-10 FPS regardless of the FPS setting.

**For 30+ FPS, you MUST use the Portal (PipeWire/GStreamer) path.**
The grim backend is a fallback for hosts where the portal is not available.

Grim optimizations applied:
- Force scale 0.4 (never above 40% resolution)
- Quality capped at 35
- Cursor not captured (faster)
- Loop sleeps remaining frame time instead of skipping missed ticks, maximizing throughput
- Send deadline removed for grim (large JPEG frames send at TCP speed)

### Check which source is active

```bash
journalctl --user -u waypad-daemon -f | grep 'backend='
```

- `backend=hyprland-grim` — screenshot-per-frame, slow. Switch to Portal picker.
- `backend=wayland-screencast-portal` — PipeWire pipeline, fast.

If the portal path is selected but the stream is still ~6 FPS, the fast pipeline
failed and fell back silently in an older build. Current builds log this at
`error` level and report it to the app:

```bash
journalctl --user -u waypad-daemon -f | grep 'falling back to the grim'
```

The same reason is returned as `portal_last_error` in the next
`start_screen_stream` response.

### Why grim is slow

Each grim frame spawns a new `grim` process that takes a full-screen JPEG
screenshot. At 1080p, this takes 80-200 ms per frame (5-12 FPS max). Grim is
intended as a fallback for hosts without PipeWire/GStreamer capture.

### Fix: use Portal picker

1. In the Android app, go to Remote Display → Sources
2. Select **"Portal picker (60 FPS capable)"**
3. **First time only**: A ScreenCast approval dialog appears on the Linux host — approve it.
   After the first approval, the daemon saves a `restore_token` and all future streams
   start automatically without any dialog.
4. The stream now uses the PipeWire + GStreamer pipeline

If you need to re-authorize (e.g., after changing monitors), delete the saved token:
```bash
rm ~/.config/waypad-daemon/portal_restore_token.json
systemctl --user restart waypad-daemon
```

If Portal picker appears but stream fails immediately with GStreamer errors
(`"error set output format: -22"` or `"pipeline doesn't want to preroll"`):

1. Remove forced format caps (done — the pipeline now auto-negotiates format)
2. Check PipeWire: `systemctl --user status pipewire wireplumber`
3. Check GStreamer plugins: `gst-inspect-1.0 pipewiresrc videoconvert h264parse nvh264enc`
4. On NVIDIA GPUs, DMA-BUF negotiation may fail. The pipeline now detects this
   and auto-falls back to grim
```bash
sudo pacman -S pipewire wireplumber xdg-desktop-portal \
  xdg-desktop-portal-hyprland gst-plugin-pipewire gst-plugins-good
systemctl --user restart pipewire wireplumber \
  xdg-desktop-portal xdg-desktop-portal-hyprland
```

If Portal picker appears but **no approval dialog shows on Linux**, check:
1. The portal backend is running: `systemctl --user status xdg-desktop-portal-hyprland`
2. The dialog might be hidden behind other windows — check your taskbar
3. Try deleting any stale restore token: `rm ~/.config/waypad-daemon/portal_restore_token.json`
4. Restart the daemon: `systemctl --user restart waypad-daemon`
5. On first stream start after deleting the token, the dialog should appear

### Verify portal throughput

```bash
journalctl --user -u waypad-daemon -f | grep 'throughput'
```

Healthy output: `fps_measured=52.3 fps_target=60 frames=104`
If measured FPS is still low despite using portal, the bottleneck is in:
- Compositor capture rate (check compositor is running)
- PipeWire buffer settings
- Encode complexity (reduce bitrate or resolution)
- Network bandwidth (TCP cannot keep up with frame size)

## 60 FPS Setting Does Not Seem To Apply

The Android app sends `max_fps`, `jpeg_quality`, `bitrate_kbps`, `max_width`,
and `max_height` when starting a screen stream. The daemon logs the accepted
values:

```bash
journalctl --user -u waypad-daemon -f | grep 'starting screen stream'
```

For Game Mode or Ultra Low Latency, expect `fps=60` and a smaller max dimension.
Actual delivered FPS still depends on compositor capture speed, PipeWire/GStreamer
availability, encode speed, Wi-Fi quality, and Android decode time.
JPEG frames are sent with a 12 ms deadline and dropped when they miss it. H.264
frames are never dropped on the socket, because every later frame references
them; backpressure is absorbed by the leaky queue in front of the encoder
instead.

### Pipeline low-latency tuning

The GStreamer pipeline is configured for interactive streaming:

```
pipewiresrc(fd=3) →
queue(max-size-buffers=4, leaky=downstream) →
videorate(drop-only=true, max-rate=FPS) → videoscale → videoconvert(n-threads=4) →
caps(video/x-raw,format=NV12|I420[,width,height],pixel-aspect-ratio=1/1) →
nvh264enc(bitrate=K, max-bitrate=K, rc-mode=cbr, preset=p4,
          tune=ultra-low-latency, zerolatency=true, bframes=0,
          gop-size=2*FPS, repeat-sequence-header=true, aud=true) →
caps(video/x-h264,profile=high) →
h264parse(config-interval=-1) →
caps(video/x-h264,stream-format=byte-stream,alignment=au) →
queue(max-size-buffers=8) →
fdsink(fd=1, sync=false)
```

`videorate`, `videoscale`, and `videoconvert` are all mandatory, not
optimizations. A capsfilter constrains but never converts, so any property it
pins without a converter in front of it becomes a requirement on `pipewiresrc`,
which cannot satisfy it and fails with:

```
stream error: error set output format: -22 (Invalid argument)
streaming stopped, reason not-negotiated (-4)
ERROR: pipeline doesn't want to preroll.
```

The daemon then falls back to `grim`, which looks like ~6 FPS slowness rather
than a failure. If you see that error, check that no capsfilter pins a property
whose converter is missing.

Each element is tuned to minimize buffering:
- `drop-only=true` on videorate caps the rate without ever duplicating frames
  and without holding a buffer back to pace output
- `leaky=downstream` on the queue in FRONT of the encoder: raw frames are the
  only ones safe to drop when the network cannot keep up
- The queue AFTER the encoder is deliberately not leaky
- `zerolatency` plus `bframes=0` removes reordering delay
- `rc-mode=cbr` keeps bandwidth predictable so Wi-Fi does not saturate
- `gop-size=2*FPS` bounds recovery time after a decoder glitch; a client that
  rebuilds its decoder can also ask for an immediate IDR with `request_key_frame`
- `repeat-sequence-header` and `config-interval=-1` put SPS/PPS before every IDR
- `format=NV12` (NVENC) or `I420` (x264) is the cheapest upload/encode path
- `sync=false` on fdsink avoids blocking on stdout

`x264enc tune=zerolatency speed-preset=veryfast` replaces `nvh264enc` when NVENC
does not start; `jpegenc` replaces the whole encoder block when neither H.264
encoder is usable, and the stream then announces `WAYPAD_STREAM_V1` instead of
`WAYPAD_STREAM_V2`.

### Honest FPS reporting

The daemon now returns `actual_fps` and `actual_quality` in the stream start response. If the selected source cannot support the requested FPS (e.g., grim is capped at 30), the app displays the clamped value so you know exactly what the host is delivering.

### Portal restore tokens

The daemon uses xdg-desktop-portal persist_mode + restore_token for
automatic session restoration.

**First time**: Select "Portal picker" → portal dialog appears on desktop →
approve → restore_token saved. Only the FIRST time needs approval.

**Subsequent times**: restore_token is passed to CreateSession, skipping
SelectSources entirely. No dialog appears. The portal returns a new
restore_token which replaces the old one (restore tokens are single-use).

**If restore fails**: token was revoked, permissions changed, or source
disappeared. Daemon falls back to grim automatically. To re-authorize:
```bash
rm ~/.config/waypad-daemon/portal_restore_token.json
systemctl --user restart waypad-daemon
```

### Display persistence

The daemon remembers which screen source was last used:
```bash
cat ~/.config/waypad-daemon/preferred_source.json
```

On subsequent stream starts, the same source is restored automatically.
If that source is no longer available (e.g. monitor disconnected), the
default source is used instead.

### Stream source selection matters for FPS

The daemon exposes two kinds of sources:
1. **Portal picker (60 FPS capable)** — uses PipeWire + GStreamer pipeline.
   Can reach 30-60 FPS. Requires user to approve a ScreenCast portal dialog
   on the Linux host.
2. **Hyprland monitor (screenshot fallback — slower)** — uses `grim` per-frame
   screenshot capture. Capped at 15 FPS. Suitable for desktop viewing only,
   not for gaming or smooth video.

The Android app auto-selects the portal chooser when available. If you
see a grim monitor source, make sure `xdg-desktop-portal`,
`pipewire`, `wireplumber`, and `gst-plugin-pipewire` are installed.

### Frame send deadline

Each frame must be sent to the TCP socket within 12 ms. If the kernel send
buffer is full (network congestion), the frame is dropped and the pipeline
continues to the next frame. This prevents the common pattern of buffering
old frames and then bursting them all at once.

## Controller Forwarding Latency

uinput events are batched to reduce kernel call overhead:
- **Button events**: Written and flushed immediately (critical for timing)
- **Axis events**: Written and synced, but flushed only when a button event
  arrives or the batch is complete. This coalesces multiple axis updates
  into a single kernel operation.

To verify:

```bash
journalctl --user -u waypad-daemon -f | grep "virtual gamepad"
```

## Performance Bottleneck Diagnosis

### Check capture backend

```bash
waypad-daemon doctor | grep -A5 capture
```

`wayland-screencast-portal` (PipeWire/GStreamer) = fast, can reach 60 fps.
`hyprland-grim` = slow, single-frame screenshot per tick, 5-15 fps max.

### Check GStreamer pipeline health

```bash
journalctl --user -u waypad-daemon -f | grep "gstreamer"
```

Warnings about pipewire feed stalls or jpegenc errors indicate capture issues.
The pipeline will continue with frame drops rather than stalling.

### Check frame dropping activity

```bash
journalctl --user -u waypad-daemon -f | grep "dropping frame"
```

If this appears frequently, the network cannot keep up with the frame rate.
Reduce quality/resolution or switch to a lower FPS profile.

### Check uinput availability for controllers

```bash
ls -l /dev/uinput
waypad-daemon doctor | grep -A8 external_input
```

`external_input.controller = true` requires writable `/dev/uinput`.

## Input Works But Stream Fails

Check `capture` in `waypad-daemon doctor`. Input may use RemoteDesktop or Hyprland IPC even when ScreenCast/PipeWire is unavailable. Use the app's Pad mode as a fallback while fixing portal or PipeWire capture.

## "Input injection requires portal approval"

Open the Android app, connect, then tap "Approve portal". A local portal dialog should appear on the Linux host. Approve keyboard and pointer control.

## Pairing Fails

Create a fresh code:

```bash
waypad-daemon pair-code
```

Pairing codes expire after 5 minutes by default and are single use.

## Device Was Lost or Sold

Revoke it:

```bash
waypad-daemon devices list
waypad-daemon devices revoke <device-id>
```

## Host Fingerprint Changed

The Android app refuses to connect if the pinned host fingerprint changes. This can happen after:

- `waypad-daemon rotate-host-key`
- deleting the daemon state directory
- restoring from a different Linux user profile

Remove the trusted host on Android and pair again only if you intentionally changed the host key.

## Stream Gets Stuck On "Connecting" / Portal Never Appears

**The daemon now handles this automatically.** If the portal dialog doesn't
appear within 15 seconds, the stream falls back to the grim screenshot backend.
This works without any host approval and delivers 20-25 fps.

### What happens:
1. Daemon tries portal path for 15 seconds
2. If portal succeeds → 60 FPS PipeWire stream
3. If portal fails/times out → auto-fallback to grim (20-25 FPS)
4. No more "connecting stream" hangs!

### To try portal again later:
```bash
waypad-daemon authorize-portal
```
This command has a 15-second timeout. Approve the dialog if it appears.
If it doesn't, the grim fallback continues to work.

## Portal Never Appears (Default Grim Fallback)

If the portal dialog never appears on your desktop (common on Hyprland),
**the daemon now defaults to grim automatically**. No manual intervention needed.

### What happens:
1. First stream start → grim monitor auto-selected (no portal picker)
2. Stream delivers ~9-10 fps (grim is fundamentally limited to this)
3. **No timeouts, no retry loops, no "screen stream failed after retries"**
4. Stream starts immediately on connection

### If you ever get the portal working:
```bash
waypad-daemon authorize-portal   # approve dialog if it appears
```
After approval, future streams auto-switch to portal at 30-60 fps.

### Manual source switching:
In the Android app, you can still select "Portal picker" from the sources list.
If portal is not approved, the daemon silently switches to grim.

## X11 Capture Backend (60 FPS, No Portal)

Waypad now supports X11 screen capture via ffmpeg. This backend:
- **Does NOT need xdg-desktop-portal** — no approval dialog
- **Does NOT need PipeWire** — captures via X11
- **Delivers real 60 FPS** — ffmpeg runs continuously, no per-frame overhead
- **Auto-detected** — X11 monitor sources appear first in the list

### Requirements:
```bash
# ffmpeg must be installed
sudo pacman -S ffmpeg

# XWayland must be running (check with:)
echo $DISPLAY  # should show :1 or :0
pgrep Xwayland
```

### How it works:
1. ffmpeg captures the X11 screen via `x11grab` at the requested FPS
2. Frames are encoded as MJPEG and piped to stdout
3. The daemon reads frames and sends them via TCP (same envelope as other backends)
4. Android side decodes JPEG frames as usual — this backend stays on
   `WAYPAD_STREAM_V1`; only the PipeWire path sends H.264

### Limitations:
- Only captures X11/XWayland windows, not native Wayland apps
- Most games (Steam, Proton, Wine) run through XWayland by default
- For native Wayland apps, launch with: `GDK_BACKEND=x11 ./my-app`

### Selecting a specific monitor:
The daemon auto-detects all connected monitors via `xrandr`.
Select the desired monitor in the Android app sources list.
