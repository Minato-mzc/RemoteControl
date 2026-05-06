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

/// Render one HTML page with a QR tile per discovered LAN address. The user
/// scans whichever tile matches the network the phone is currently on (home
/// Wi-Fi vs. phone hotspot vs. wired). Auto-opens in the default browser.
pub fn save_qr_html_and_open(
    addrs: &[DiscoveredAddr],
    port: u16,
    code: &str,
    key_b64url: &str,
) -> Result<PathBuf> {
    if addrs.is_empty() {
        anyhow::bail!("no LAN address candidates");
    }

    let mut tiles = String::new();
    for a in addrs {
        let payload = build_payload(&a.addr.to_string(), port, code, key_b64url);
        let qr = QrCode::new(payload.as_bytes()).context("build QR")?;
        let svg_xml = qr
            .render::<svg::Color>()
            .min_dimensions(280, 280)
            .quiet_zone(true)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build();

        let (kind_label, kind_class) = match a.kind {
            InterfaceKind::Physical => ("物理网卡", "ok"),
            InterfaceKind::Unknown => ("未知类型", "warn"),
            InterfaceKind::Virtual => ("虚拟网卡（手机一般不可达）", "bad"),
        };

        let _ = write!(
            &mut tiles,
            r##"<div class="card">
                  <div class="qr">{svg_xml}</div>
                  <div class="ip">{ip}:{port}</div>
                  <div class="iface {kind_class}">{iface} · {kind_label}</div>
                  <div class="meta">{payload}</div>
                </div>"##,
            svg_xml = svg_xml,
            ip = a.addr,
            port = port,
            iface = html_escape(&a.iface_name),
            kind_label = kind_label,
            kind_class = kind_class,
            payload = html_escape(&payload),
        );
    }

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
    .meta {{ font-family: ui-monospace, Consolas, monospace; color: #999;
             font-size: 11px; margin-top: 12px; word-break: break-all; }}
  </style>
</head>
<body>
  <h1>RemoteControl 配对</h1>
  <p>用手机 App 扫和你当前网络对应的那张二维码</p>
  <div>配对码 <span class="code">{code}</span> · 5 分钟有效，单次使用</div>
  <div class="grid">{tiles}</div>
</body>
</html>"##,
        code = code,
        tiles = tiles,
    );

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
