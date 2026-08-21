//! The control panel page.
//!
//! One self-contained document: no build step, no bundler, no assets to ship
//! next to the binary. It is served from memory and everything it needs is in
//! this string.
//!
//! The token arrives in the query string, and the page moves it into a header
//! for its own calls so it never ends up in a bookmark or a history entry that
//! survives the daemon it belonged to.

pub fn render() -> &'static str {
    PAGE
}

const PAGE: &str = r##"<!doctype html>
<html lang="en" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Waypad</title>
<style>
  :root {
    color-scheme: dark;
    --bg: #0e1116;
    --panel: #161b22;
    --panel-2: #1c2230;
    --line: #2a3240;
    --text: #e6edf3;
    --muted: #8b98a8;
    --accent: #7cc7ea;
    --ok: #56d364;
    --warn: #e3b341;
    --bad: #f47067;
    --radius: 14px;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 32px 20px 64px;
    background: var(--bg); color: var(--text);
    font: 15px/1.55 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  }
  .wrap { max-width: 880px; margin: 0 auto; }
  header { display: flex; align-items: baseline; gap: 14px; flex-wrap: wrap; margin-bottom: 6px; }
  h1 { font-size: 26px; letter-spacing: .14em; margin: 0; }
  h2 { font-size: 15px; margin: 0 0 14px; letter-spacing: .04em; color: var(--muted); text-transform: uppercase; }
  .host { color: var(--accent); font-size: 17px; }
  .sub { color: var(--muted); margin: 0 0 26px; }
  .card {
    background: var(--panel); border: 1px solid var(--line);
    border-radius: var(--radius); padding: 20px; margin-bottom: 18px;
  }
  .row { display: flex; gap: 12px; align-items: center; flex-wrap: wrap; }
  .grow { flex: 1; }
  button {
    background: var(--accent); color: #06131b; border: 0; cursor: pointer;
    font: 600 14px/1 system-ui, sans-serif; padding: 11px 18px; border-radius: 999px;
  }
  button:hover { filter: brightness(1.08); }
  button.ghost { background: transparent; color: var(--text); border: 1px solid var(--line); }
  button.danger { background: transparent; color: var(--bad); border: 1px solid var(--bad); padding: 7px 14px; }
  button:disabled { opacity: .5; cursor: default; filter: none; }
  code, .mono { font-family: ui-monospace, "Cascadia Code", Consolas, monospace; }
  .fingerprint { color: var(--muted); font-size: 12px; word-break: break-all; }
  .pill {
    display: inline-flex; align-items: center; gap: 6px; padding: 4px 11px;
    border-radius: 999px; font-size: 12.5px; border: 1px solid var(--line);
    background: var(--panel-2);
  }
  .pill.on  { color: var(--ok);   border-color: color-mix(in srgb, var(--ok) 45%, var(--line)); }
  .pill.off { color: var(--muted); }
  .caps { display: grid; gap: 10px; }
  .cap { padding: 12px 14px; border: 1px solid var(--line); border-radius: 10px; background: var(--panel-2); }
  .cap .head { display: flex; justify-content: space-between; gap: 12px; align-items: center; }
  .cap .name { font-weight: 600; }
  .cap .why { color: var(--muted); font-size: 13px; margin-top: 6px; }
  .code {
    font: 700 40px/1 ui-monospace, Consolas, monospace;
    letter-spacing: .22em; color: var(--accent);
  }
  .pair { display: flex; gap: 24px; align-items: center; flex-wrap: wrap; }
  .qr { background: #fff; padding: 10px; border-radius: 10px; line-height: 0; }
  .qr svg { width: 190px; height: 190px; display: block; }
  table { width: 100%; border-collapse: collapse; }
  th { text-align: left; font-size: 12px; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); padding-bottom: 8px; }
  td { padding: 9px 0; border-top: 1px solid var(--line); vertical-align: middle; }
  .logs {
    background: #0a0d12; border: 1px solid var(--line); border-radius: 10px;
    padding: 12px; height: 260px; overflow: auto;
    font: 12.5px/1.6 ui-monospace, Consolas, monospace; white-space: pre-wrap;
  }
  .logs .error { color: var(--bad); }
  .logs .warn  { color: var(--warn); }
  .logs .info  { color: var(--text); }
  .logs .debug, .logs .trace { color: var(--muted); }
  .empty { color: var(--muted); font-style: italic; }
  .switch { display: flex; align-items: center; gap: 10px; cursor: pointer; }
  .switch input { width: 18px; height: 18px; accent-color: var(--accent); }
  .toast {
    position: fixed; inset: auto 0 24px 0; margin: 0 auto; width: fit-content;
    max-width: 90vw; background: var(--panel-2); border: 1px solid var(--line);
    border-radius: 999px; padding: 10px 20px; opacity: 0; transition: opacity .2s;
    pointer-events: none;
  }
  .toast.show { opacity: 1; }
  .toast.bad { border-color: var(--bad); color: var(--bad); }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <h1>WAYPAD</h1>
    <span class="host" id="host">…</span>
    <span class="pill" id="platform">…</span>
  </header>
  <p class="sub">Host control panel · serving on this machine only</p>

  <div class="card">
    <h2>Pair a phone</h2>
    <div class="pair">
      <div class="grow">
        <div class="code" id="code">— — — — — —</div>
        <p class="sub" id="code-note" style="margin:10px 0 0">
          Generate a one-time code, then enter it in the Waypad app — or scan the QR.
        </p>
        <div class="row" style="margin-top:16px">
          <button id="pair">Generate code</button>
        </div>
      </div>
      <div id="qr-wrap" hidden><div class="qr" id="qr"></div></div>
    </div>
  </div>

  <div class="card">
    <h2>What this host can do</h2>
    <div class="caps" id="caps"></div>
  </div>

  <div class="card">
    <h2>Trusted devices</h2>
    <div id="devices"></div>
  </div>

  <div class="card">
    <h2>Settings</h2>
    <label class="switch">
      <input type="checkbox" id="autostart">
      <span>Start Waypad when I log in</span>
    </label>
    <p class="sub" id="autostart-note" style="margin:10px 0 0"></p>
    <p class="fingerprint" style="margin-top:18px">
      Host fingerprint <span id="fingerprint" class="mono"></span>
    </p>
  </div>

  <div class="card">
    <h2>Recent activity</h2>
    <div class="logs" id="logs"></div>
  </div>
</div>
<div class="toast" id="toast"></div>

<script>
// The token arrives in the query string because that is the only way a browser
// can be handed one. It is moved into a header for every call and stripped from
// the address bar, so it does not end up in history or a bookmark that outlives
// the daemon that issued it.
const TOKEN = new URLSearchParams(location.search).get("token") || "";
history.replaceState(null, "", location.pathname);

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: { ...(options.headers || {}), "X-Waypad-Token": TOKEN },
  });
  const text = await response.text();
  const data = text ? JSON.parse(text) : {};
  if (!response.ok) throw new Error(data.error || response.statusText);
  return data;
}

let toastTimer;
function toast(message, bad = false) {
  const element = document.getElementById("toast");
  element.textContent = message;
  element.classList.toggle("bad", bad);
  element.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => element.classList.remove("show"), 3200);
}

function pill(on, label) {
  return `<span class="pill ${on ? "on" : "off"}">${on ? "●" : "○"} ${label}</span>`;
}

function capability(name, entry) {
  // The reason is shown whether or not the capability works. A control that
  // silently does nothing is the thing this panel exists to prevent.
  const why = entry.reason ? `<div class="why">${escapeHtml(entry.reason)}</div>` : "";
  const backend = entry.backend && entry.backend !== "noop"
    ? `<span class="pill">${escapeHtml(entry.backend)}</span>` : "";
  return `<div class="cap">
    <div class="head">
      <span class="name">${name}</span>
      <span>${backend} ${pill(entry.supported, entry.supported ? "ready" : "unavailable")}</span>
    </div>${why}</div>`;
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function renderStatus(status) {
  document.getElementById("host").textContent = status.host_name;
  document.getElementById("platform").textContent = status.platform;
  document.getElementById("fingerprint").textContent = status.fingerprint;

  const caps = status.capabilities;
  const system = caps.system || {};
  document.getElementById("caps").innerHTML = [
    capability("Remote input", caps.input),
    capability("Screen capture", caps.capture),
    capability("Desktop audio", caps.audio_capture),
    capability("Controller forwarding", {
      supported: caps.external_input.controller,
      backend: caps.external_input.backend,
      reason: caps.external_input.reason,
    }),
    `<div class="cap"><div class="head"><span class="name">System controls</span><span>
      ${pill(system.volume, "volume")} ${pill(system.media, "media")}
      ${pill(system.brightness, "brightness")} ${pill(system.clipboard, "clipboard")}
      ${pill(system.lock, "lock")}</span></div></div>`,
  ].join("");

  const devices = status.devices || [];
  document.getElementById("devices").innerHTML = devices.length === 0
    ? `<p class="empty">No phone has been paired with this host yet.</p>`
    : `<table><thead><tr><th>Device</th><th>Status</th><th></th></tr></thead><tbody>` +
      devices.map((device) => `<tr>
        <td>${escapeHtml(device.name)}<div class="fingerprint">${escapeHtml(device.id)}</div></td>
        <td>${device.revoked ? '<span class="pill off">revoked</span>' : pill(true, "trusted")}</td>
        <td style="text-align:right">${device.revoked ? "" :
          `<button class="danger" data-revoke="${escapeHtml(device.id)}">Revoke</button>`}</td>
      </tr>`).join("") + `</tbody></table>`;

  const toggle = document.getElementById("autostart");
  const note = document.getElementById("autostart-note");
  if (status.autostart === null || status.autostart === undefined) {
    toggle.disabled = true;
    note.textContent = status.autostart_error || "Login start cannot be managed on this host.";
  } else {
    toggle.checked = status.autostart;
    toggle.disabled = false;
    note.textContent = "Waypad runs in the background and shows an icon in the tray.";
  }
}

async function refresh() {
  try {
    renderStatus(await api("/api/status"));
  } catch (err) {
    toast(err.message, true);
  }
}

document.getElementById("pair").addEventListener("click", async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  try {
    const result = await api("/api/pair-code", { method: "POST" });
    document.getElementById("code").textContent = result.code.split("").join(" ");
    document.getElementById("qr").innerHTML = result.qr_svg;
    document.getElementById("qr-wrap").hidden = false;
    const expires = new Date(result.expires_at * 1000);
    document.getElementById("code-note").textContent =
      `Valid until ${expires.toLocaleTimeString()}. It works once, then it is spent.`;
  } catch (err) {
    toast(err.message, true);
  } finally {
    button.disabled = false;
  }
});

document.getElementById("devices").addEventListener("click", async (event) => {
  const id = event.target.dataset && event.target.dataset.revoke;
  if (!id) return;
  if (!confirm("Revoke this device? It will have to pair again.")) return;
  try {
    await api(`/api/devices/revoke?id=${encodeURIComponent(id)}`, { method: "POST" });
    toast("Device revoked");
    refresh();
  } catch (err) {
    toast(err.message, true);
  }
});

document.getElementById("autostart").addEventListener("change", async (event) => {
  const enabled = event.currentTarget.checked;
  try {
    await api(`/api/autostart?enabled=${enabled}`, { method: "POST" });
    toast(enabled ? "Waypad will start when you log in" : "Login start turned off");
  } catch (err) {
    // Put the switch back where it was: leaving it showing a state the host
    // never reached is worse than not moving at all.
    event.currentTarget.checked = !enabled;
    toast(err.message, true);
  }
});

let logCursor = 0;
async function pollLogs() {
  try {
    const { lines } = await api(`/api/logs?since=${logCursor}`);
    if (lines.length) {
      const view = document.getElementById("logs");
      const atBottom = view.scrollHeight - view.scrollTop - view.clientHeight < 40;
      for (const line of lines) {
        const row = document.createElement("div");
        row.className = line.level;
        const seconds = (line.at_ms / 1000).toFixed(1).padStart(7);
        row.textContent = `${seconds}s  ${line.message}`;
        view.appendChild(row);
        logCursor = Math.max(logCursor, line.at_ms + 1);
      }
      // Only follows if the reader was already at the bottom, so scrolling back
      // to read something does not get yanked away on the next line.
      if (atBottom) view.scrollTop = view.scrollHeight;
    }
  } catch { /* a poll that fails is retried on the next tick */ }
}

refresh();
pollLogs();
setInterval(refresh, 5000);
setInterval(pollLogs, 1000);
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_self_contained() {
        let page = render();
        // No build step and nothing to ship beside the binary: an external
        // reference would be a request the strict loopback panel cannot serve.
        assert!(!page.contains("http://"), "page fetches something remote");
        assert!(!page.contains("https://"), "page fetches something remote");
        assert!(!page.contains("<script src"), "page loads external script");
        assert!(!page.contains("<link"), "page loads an external stylesheet");
    }

    #[test]
    fn every_call_carries_the_token_in_a_header() {
        let page = render();
        assert!(page.contains("X-Waypad-Token"));
        // Stripped from the address bar so it cannot be bookmarked or land in
        // browser history after the daemon that issued it is gone.
        assert!(page.contains("history.replaceState"));
    }

    #[test]
    fn device_names_are_escaped_before_they_reach_the_page() {
        // A device name comes from the phone, so it is attacker-controlled as
        // far as this page is concerned.
        let page = render();
        assert!(page.contains("function escapeHtml"));
        assert!(page.contains("escapeHtml(device.name)"));
        assert!(page.contains("escapeHtml(entry.reason)"));
    }

    #[test]
    fn the_panel_offers_what_the_cli_does() {
        let page = render();
        for route in [
            "/api/status",
            "/api/pair-code",
            "/api/devices/revoke",
            "/api/autostart",
            "/api/logs",
        ] {
            assert!(page.contains(route), "page never calls {route}");
        }
    }
}
