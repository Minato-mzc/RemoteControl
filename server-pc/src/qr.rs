use anyhow::{Context, Result};
use qrcode::render::{svg, unicode};
use qrcode::QrCode;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use crate::config::PROTOCOL_VERSION;
use crate::net::{DiscoveredAddr, InterfaceKind};

pub fn build_payload(host: &str, port: u16, code: &str, key_b64url: &str) -> String {
    format!(
        "rc://{host}:{port}/?v={v}&c={code}&k={key}",
        v = PROTOCOL_VERSION,
        code = code,
        key = key_b64url,
    )
}

/// Build a *combined* QR payload that encodes a LAN endpoint AND an
/// optional relay fallback. Phones with up-to-date app code try LAN
/// first and fall back to relay if LAN is unreachable (cross-network).
/// Older phones see `rc://` and parse only the LAN part; the unknown
/// `rh/rid/rtls` query params are ignored, so backwards compatibility
/// holds for same-network usage.
///
/// Format:
/// `rc://<lan_host>:<lan_port>/?v=N&c=CODE&k=KEY&rh=<authority>&rid=<host_id>&rtls=<flag>`
///
/// We use *three independent* query params (`rh`, `rid`, `rtls`)
/// rather than one packed string. Earlier attempts at packing the
/// triple into a single value with `;` separators bit us on some
/// Android URI parsers that treat `;` as a secondary query-param
/// delimiter — `getQueryParameter("relay")` returned only the first
/// segment and the rest of the tuple silently vanished, leaving the
/// phone with no usable fallback. Three flat params dodge the issue
/// entirely.
pub fn build_combined_payload(
    lan_host: &str,
    lan_port: u16,
    code: &str,
    key_b64url: &str,
    relay: Option<&RelayQrInfo<'_>>,
) -> String {
    let base = build_payload(lan_host, lan_port, code, key_b64url);
    let Some(r) = relay else { return base };
    let scheme_is_https = r.base_url.starts_with("https://");
    let stripped = r
        .base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let authority = if stripped.contains(':') {
        stripped.to_string()
    } else {
        let default_port = if scheme_is_https { 443 } else { 80 };
        format!("{stripped}:{default_port}")
    };
    let tls = if scheme_is_https { 1 } else { 0 };
    format!(
        "{base}&rh={authority}&rid={host_id}&rtls={tls}",
        host_id = r.host_id,
    )
}

pub fn print_qr_to_terminal(host: &str, port: u16, code: &str, key_b64url: &str) -> Result<()> {
    let payload = build_payload(host, port, code, key_b64url);
    let qr = QrCode::new(payload.as_bytes())?;
    let image = qr
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();

    println!();
    println!("{image}");
    println!("Pairing code: {code}   (5 min TTL, single use)");
    println!("URL         : rc://{host}:{port}/?v={PROTOCOL_VERSION}&c={code}&k=…");
    println!();
    Ok(())
}

/// One relay endpoint to render alongside the LAN cards on the QR page.
/// Constructed by `lib.rs` from the active `RelayConfig`; threaded
/// through here so this module doesn't have to know about `relay_client`.
pub struct RelayQrInfo<'a> {
    /// Display URL — e.g. `https://relay.example.com:443`. Used both
    /// for the rendered card label and for building the `rcrelay://`
    /// payload host portion.
    pub base_url: &'a str,
    /// `host_id` minted by the relay during one-shot provisioning.
    pub host_id: &'a str,
}

/// Render one HTML page with a QR tile per discovered LAN address (and
/// optionally one for the configured relay). The user scans whichever
/// tile matches where the phone is — home Wi-Fi, phone hotspot, or
/// cross-network via relay. Auto-opens in the default browser.
/// Build the QR HTML page as a string without writing to disk. Used by
/// [`save_qr_html_and_open`] (file-based path, kept for backwards
/// compatibility) and by `qr_server` (serves it dynamically over HTTP
/// so the in-browser refresh button can re-render with a fresh pairing
/// code without restarting the server).
pub fn build_qr_html(
    addrs: &[DiscoveredAddr],
    port: u16,
    code: &str,
    key_b64url: &str,
    relay: Option<&RelayQrInfo<'_>>,
) -> Result<String> {
    if addrs.is_empty() && relay.is_none() {
        anyhow::bail!("no LAN address candidates and no relay configured");
    }

    // Drop virtual / Hyper-V switch interfaces from the HTML — phones can't
    // route to them, so showing a QR card just creates noise. We still log
    // them in `lib.rs` for debugging, just don't print a QR. If everything
    // got filtered out (no physical NIC), fall back to the full list so
    // the user at least has *something* to scan.
    let physical: Vec<&DiscoveredAddr> = addrs
        .iter()
        .filter(|a| a.kind != InterfaceKind::Virtual)
        .collect();
    let visible: Vec<&DiscoveredAddr> = if physical.is_empty() {
        addrs.iter().collect()
    } else {
        physical
    };

    // ONE combined card with ONE QR.
    //
    // Strategy:
    //   * Pick the best LAN address (first physical NIC) as the primary
    //     endpoint encoded in the `rc://` authority.
    //   * If a relay is configured, append it as `&relay=...` in the
    //     query string so newer phone builds can fall back when LAN is
    //     unreachable (cross-network case).
    //   * Older phone builds parse `rc://` as before — they get LAN
    //     only, which works in the same-Wi-Fi case anyway.
    //
    // If there are *no* LAN addresses (RelayOnly mode), we synthesize a
    // pseudo-host of `0.0.0.0:0` so the URI still parses cleanly; the
    // phone will detect that, ignore the LAN dial entirely, and go
    // straight to the relay.
    let mut tiles = String::new();
    let (primary_addr_str, primary_iface_kind, primary_iface_name) =
        if let Some(first) = visible.first() {
            (first.addr.to_string(), first.kind, first.iface_name.clone())
        } else {
            ("0.0.0.0".to_string(), InterfaceKind::Unknown, String::from("relay-only"))
        };
    let payload = build_combined_payload(
        &primary_addr_str,
        port,
        code,
        key_b64url,
        relay,
    );
    let qr = QrCode::new(payload.as_bytes()).context("build combined QR")?;
    let svg_xml = qr
        .render::<svg::Color>()
        .min_dimensions(320, 320)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();

    // Display string for the card subtitle.
    let lan_display = format!("{primary_addr_str}:{port}");
    let relay_display = relay
        .map(|r| {
            let stripped = r
                .base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/');
            stripped.to_string()
        })
        .unwrap_or_default();
    let (kind_label, kind_class) = match primary_iface_kind {
        InterfaceKind::Physical => ("物理网卡", "ok"),
        InterfaceKind::Unknown => ("--", "warn"),
        InterfaceKind::Virtual => ("虚拟网卡", "bad"),
    };

    let _ = write!(
        &mut tiles,
        r##"<div class="card combo">
              <div class="qr">{svg_xml}</div>
              <div class="ip">{lan_display}</div>
              <div class="iface {kind_class}">{iface} · {kind_label}</div>
              {relay_line}
              <div class="meta">{payload}</div>
            </div>"##,
        svg_xml = svg_xml,
        lan_display = html_escape(&lan_display),
        iface = html_escape(&primary_iface_name),
        kind_class = kind_class,
        kind_label = kind_label,
        relay_line = if relay.is_some() {
            format!(
                r##"<div class="iface relay-tag">跨网络中继: {}</div>"##,
                html_escape(&relay_display)
            )
        } else {
            String::new()
        },
        payload = html_escape(&payload),
    );

    let html = format!(
        r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <title>RemoteControl 配对二维码</title>
  <meta http-equiv="refresh" content="300">
  <style>
    body {{ font-family: -apple-system, "Segoe UI", "Microsoft YaHei", sans-serif;
            text-align: center; padding: 24px; background: #f7f7f8; color: #222; margin: 0; }}
    h1 {{ margin: 0 0 4px; font-size: 22px; }}
    p {{ color: #666; margin: 4px 0; }}
    .code {{ font-size: 36px; letter-spacing: 10px; color: #c0392b;
             font-weight: 700; font-variant-numeric: tabular-nums; margin: 8px 0; }}
    .grid {{ display: flex; flex-wrap: wrap; gap: 24px; justify-content: center;
             margin-top: 24px; }}
    .card {{ background: #fff; border: 1px solid #e2e2e6; border-radius: 12px;
             padding: 20px; max-width: 360px; }}
    .qr svg {{ display: block; width: 280px; height: 280px; margin: 0 auto; }}
    .ip {{ font-family: ui-monospace, Consolas, monospace; font-size: 18px;
           font-weight: 600; margin-top: 12px; }}
    .iface {{ font-size: 13px; margin: 4px 0; }}
    .iface.ok {{ color: #2c7a3e; }}
    .iface.warn {{ color: #b87900; }}
    .iface.bad {{ color: #b00020; }}
    /* Relay card: subtly different border so it's distinguishable from
       LAN cards but still feels part of the set. */
    .card.relay {{ border-color: #1f4d8b; box-shadow: 0 0 0 1px #1f4d8b inset; }}
    .iface.relay-tag {{ color: #1f4d8b; font-weight: 600; }}
    .meta {{ font-family: ui-monospace, Consolas, monospace; color: #999;
             font-size: 11px; margin-top: 12px; word-break: break-all; }}

    /* M6 v2: drag-drop area for PC → phone file sends. Stays hidden in
       the no-connected-session state via JS toggling `.disabled`. */
    .send {{ max-width: 760px; margin: 28px auto 0; }}
    .send h2 {{ font-size: 16px; margin: 0 0 8px; color: #333; }}
    .drop {{ border: 2px dashed #bcd;
             border-radius: 14px; background: #fafbff;
             padding: 28px 16px; transition: background .15s, border-color .15s; }}
    .drop.hot {{ background: #eaf2ff; border-color: #1f4d8b; }}
    .drop p {{ margin: 6px 0; color: #556; }}
    .drop .pick {{ display: inline-block; margin-top: 8px; padding: 6px 14px;
                   border-radius: 999px; background: #1f4d8b; color: #fff;
                   cursor: pointer; font-size: 13px; font-weight: 600; }}
    .send.disabled .drop {{ opacity: .55; pointer-events: none; }}
    .send .hint {{ font-size: 12px; color: #888; margin-top: 8px; }}
    .send .log {{ margin-top: 12px; text-align: left; font-size: 13px;
                  max-height: 200px; overflow-y: auto; }}
    .log .row {{ display: flex; gap: 8px; align-items: center; padding: 6px 8px;
                 border-radius: 8px; margin-bottom: 4px; background: #fff;
                 border: 1px solid #eee; }}
    .log .row.ok {{ border-color: #b8e3c4; background: #f4fbf6; }}
    .log .row.err {{ border-color: #f1c0c0; background: #fcf3f3; }}
    .log .row .name {{ flex: 1; font-family: ui-monospace, Consolas, monospace;
                       font-size: 12px; word-break: break-all; }}
    .log .row .size {{ color: #888; font-size: 11px; white-space: nowrap; }}
  </style>
</head>
<body>
  <h1>RemoteControl 配对</h1>
  <p>手机 App 扫这一个二维码即可，连接路径自动选择（同 WiFi 走 LAN，否则走中继）</p>
  <div>配对码 <span class="code">{code}</span> · 5 分钟有效，单次使用</div>
  <p style="margin-top: 14px;">
    <a href="/refresh"
       style="display: inline-block; padding: 8px 18px; border-radius: 999px;
              background: #1f4d8b; color: #fff; text-decoration: none;
              font-size: 13px; font-weight: 600;">🔄 刷新二维码</a>
  </p>
  <div class="grid">{tiles}</div>

  <section class="send" id="send">
    <h2>📤 发文件到手机</h2>
    <div class="drop" id="drop">
      <p>把文件拖到这里，或者</p>
      <label class="pick">
        选择文件
        <input type="file" id="pick" multiple style="display:none">
      </label>
      <div class="hint">需要手机端已连接。文件会保存到手机 App 私有目录的 Downloads/ 下，文件管理器里能找到。</div>
    </div>
    <div class="log" id="log"></div>
  </section>

  <script>
    const drop = document.getElementById('drop');
    const log  = document.getElementById('log');
    const pick = document.getElementById('pick');

    function fmtBytes(n) {{
      if (n < 1024) return n + ' B';
      if (n < 1024*1024) return (n/1024).toFixed(1) + ' KB';
      if (n < 1024*1024*1024) return (n/1024/1024).toFixed(1) + ' MB';
      return (n/1024/1024/1024).toFixed(2) + ' GB';
    }}

    function addLogRow(file) {{
      const row = document.createElement('div');
      row.className = 'row';
      row.innerHTML = '<span>⏳</span><span class="name"></span><span class="size"></span>';
      row.querySelector('.name').textContent = file.name;
      row.querySelector('.size').textContent = fmtBytes(file.size);
      log.prepend(row);
      return row;
    }}

    function setRowStatus(row, ok, reason) {{
      row.classList.remove('ok','err');
      row.classList.add(ok ? 'ok' : 'err');
      row.firstChild.textContent = ok ? '✓' : '✗';
      if (!ok && reason) {{
        const r = document.createElement('span');
        r.className = 'size';
        r.style.color = '#b00020';
        r.textContent = reason;
        row.appendChild(r);
      }}
    }}

    async function uploadOne(file) {{
      const row = addLogRow(file);
      try {{
        const res = await fetch('/send-file', {{
          method: 'POST',
          headers: {{
            'X-File-Name': encodeURIComponent(file.name),
            'Content-Type': 'application/octet-stream',
          }},
          body: file,
        }});
        const json = await res.json().catch(() => ({{ok:false,reason:'bad response'}}));
        setRowStatus(row, json.ok === true, json.reason || ('HTTP ' + res.status));
      }} catch (e) {{
        setRowStatus(row, false, String(e));
      }}
    }}

    function handleFiles(files) {{
      for (const f of files) uploadOne(f);
    }}

    drop.addEventListener('dragover', e => {{
      e.preventDefault();
      drop.classList.add('hot');
    }});
    drop.addEventListener('dragleave', () => drop.classList.remove('hot'));
    drop.addEventListener('drop', e => {{
      e.preventDefault();
      drop.classList.remove('hot');
      if (e.dataTransfer && e.dataTransfer.files) handleFiles(e.dataTransfer.files);
    }});
    pick.addEventListener('change', e => {{
      handleFiles(e.target.files);
      pick.value = '';
    }});
  </script>
</body>
</html>"##,
        code = code,
        tiles = tiles,
    );

    Ok(html)
}

/// File-based renderer kept around for the no-HTTP-server fallback
/// (mostly relevant in tests or LAN-only smoke checks). New code paths
/// should use [`build_qr_html`] + serve it via `qr_server`.
pub fn save_qr_html_and_open(
    addrs: &[DiscoveredAddr],
    port: u16,
    code: &str,
    key_b64url: &str,
    relay: Option<&RelayQrInfo<'_>>,
) -> Result<PathBuf> {
    let html = build_qr_html(addrs, port, code, key_b64url, relay)?;
    let path = std::env::current_dir()
        .context("get cwd")?
        .join("qrcode.html");
    std::fs::write(&path, html).with_context(|| format!("write {}", path.display()))?;
    open_in_default_app(&path);
    Ok(path)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "windows")]
fn open_in_default_app(path: &PathBuf) {
    // `cmd /C start "" <path>` — empty title arg avoids start treating the path as a window title.
    let _ = Command::new("cmd")
        .args(["/C", "start", "", path.to_string_lossy().as_ref()])
        .spawn();
}

#[cfg(not(target_os = "windows"))]
fn open_in_default_app(_path: &PathBuf) {}
