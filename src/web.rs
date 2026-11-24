//! `--gui` mode: serves the 3D browser dashboard and streams live telemetry
//! to it as Server-Sent Events. No extra dependencies — just tokio.
//!
//! The same `App` as the TUI does the work (rate tracking, stats, ride
//! recording), so rides taken in the browser show up in the TUI history too.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    time,
};

use super::{App, Msg};

const PAGE: &str = include_str!("../web/index.html");
// Bundled so the GUI works offline (curl it into web/ — see the README steps).
const THREE_JS: &str = include_str!("../web/three.module.js");

async fn send(sock: &mut TcpStream, ctype: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn telemetry(app: &App, now: Instant) -> String {
    format!(
        r#"{{"cadence":{:.1},"avg":{:.1},"max":{:.1},"elapsed":{},"pedalling":{},"connected":{},"status":{}}}"#,
        app.cadence(now),
        app.avg_cadence(),
        app.max_cadence,
        app.elapsed_secs,
        app.pedalling_secs,
        app.connected,
        json_str(&app.status),
    )
}

async fn handle(mut sock: TcpStream, mut rx: watch::Receiver<String>) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    let n = sock.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");

    match path {
        "/events" => {
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n",
            )
            .await?;
            loop {
                let line = format!("data: {}\n\n", rx.borrow_and_update().clone());
                sock.write_all(line.as_bytes()).await?; // errors once the tab closes
                if rx.changed().await.is_err() {
                    return Ok(());
                }
            }
        }
        "/" | "/index.html" => send(&mut sock, "text/html; charset=utf-8", PAGE).await,
        "/three.module.js" => send(&mut sock, "text/javascript; charset=utf-8", THREE_JS).await,
        _ => {
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
        }
    }
}

async fn serve(listener: TcpListener, rx: watch::Receiver<String>) {
    while let Ok((sock, _)) = listener.accept().await {
        tokio::spawn(handle(sock, rx.clone()));
    }
}

/// Headless counterpart of the TUI's `run`.
pub async fn run(
    mut app: App,
    mut msgs: mpsc::UnboundedReceiver<Msg>,
    port: u16,
    open_browser: bool,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("can't listen on 127.0.0.1:{port} (try --port)"))?;
    let url = format!("http://127.0.0.1:{port}/");
    eprintln!("cadence GUI → {url}   (Ctrl-C stops and saves the ride)");

    let (tx, rx) = watch::channel(telemetry(&app, Instant::now()));
    tokio::spawn(serve(listener, rx));
    if open_browser {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }

    let mut publish = time::interval(Duration::from_millis(100));
    let mut sampler = time::interval(Duration::from_secs(1));
    sampler.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = publish.tick() => { let _ = tx.send(telemetry(&app, Instant::now())); }
            _ = sampler.tick() => app.sample(Instant::now()),
            Some(m) = msgs.recv() => app.on_msg(m, Instant::now()),
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    app.end_ride();
    Ok(())
}
