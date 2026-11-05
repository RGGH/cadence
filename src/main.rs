//! cadence — a terminal dashboard for BLE Cycling Speed & Cadence sensors.
//!
//! Works with any sensor that exposes the standard Cycling Speed and Cadence
//! Service (0x1816), e.g. the Saris Speed/Cadence sensor.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use btleplug::api::{
    bleuuid::uuid_from_u16, Central, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph},
    DefaultTerminal, Frame,
};
use tokio::{sync::mpsc, time};
use uuid::Uuid;

// ───────────────────────── constants ─────────────────────────

const CSC_SERVICE: Uuid = uuid_from_u16(0x1816);
const CSC_MEASUREMENT: Uuid = uuid_from_u16(0x2A5B);

/// If no new revolution arrives for this long, treat the rider as stopped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(3);
/// Seconds of history shown on the graph (one sample per second).
const WINDOW_SECS: usize = 180;
/// Target cadence line drawn on the graph.
const TARGET_RPM: f64 = 90.0;
/// Readings above these are treated as glitches and ignored.
const MAX_CRANK_RPS: f64 = 250.0 / 60.0;
const MAX_WHEEL_RPS: f64 = 20.0;

// ───────────────────────── CSC parsing ─────────────────────────

/// One decoded CSC Measurement notification.
#[derive(Debug, Clone, Copy)]
struct Csc {
    /// (cumulative wheel revolutions, last wheel event time in 1/1024 s)
    wheel: Option<(u32, u16)>,
    /// (cumulative crank revolutions, last crank event time in 1/1024 s)
    crank: Option<(u16, u16)>,
}

fn parse_csc(d: &[u8]) -> Option<Csc> {
    let flags = *d.first()?;
    let mut i = 1;

    let wheel = if flags & 0x01 != 0 {
        let b = d.get(i..i + 6)?;
        i += 6;
        Some((
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
        ))
    } else {
        None
    };

    let crank = if flags & 0x02 != 0 {
        let b = d.get(i..i + 4)?;
        Some((
            u16::from_le_bytes([b[0], b[1]]),
            u16::from_le_bytes([b[2], b[3]]),
        ))
    } else {
        None
    };

    Some(Csc { wheel, crank })
}

// ───────────────────────── rate tracking ─────────────────────────

/// Turns a stream of (cumulative revs, event time) pairs into revs/second.
struct Rate {
    /// 0xFFFF for crank counters, 0xFFFF_FFFF for wheel counters.
    mask: u32,
    max_rps: f64,
    prev: Option<(u32, u16)>,
    last_event: Option<Instant>,
    rps: f64,
    total_revs: u64,
}

impl Rate {
    fn new(mask: u32, max_rps: f64) -> Self {
        Self { mask, max_rps, prev: None, last_event: None, rps: 0.0, total_revs: 0 }
    }

    /// Forget the previous sample (used after a reconnect, where the 16-bit
    /// timestamp may have wrapped any number of times).
    fn rebaseline(&mut self) {
        self.prev = None;
        self.last_event = None;
        self.rps = 0.0;
    }

    fn update(&mut self, revs: u32, time: u16, now: Instant) {
        let Some((prev_revs, prev_time)) = self.prev else {
            self.prev = Some((revs, time));
            self.last_event = Some(now);
            return;
        };

        let dr = revs.wrapping_sub(prev_revs) & self.mask;
        if dr == 0 {
            // Sensors repeat the last event while you're stopped.
            return;
        }

        let was_idle = self
            .last_event
            .map_or(true, |t| now.duration_since(t) > IDLE_TIMEOUT);

        self.prev = Some((revs, time));
        self.last_event = Some(now);
        self.total_revs += u64::from(dr);

        if was_idle {
            // The gap since the previous event is meaningless (and may exceed
            // the 64 s timestamp wrap) — start fresh from this event.
            self.rps = 0.0;
            return;
        }

        let dt = f64::from(time.wrapping_sub(prev_time)) / 1024.0;
        if dt > 0.0 {
            let rps = f64::from(dr) / dt;
            if rps <= self.max_rps {
                self.rps = rps;
            }
        }
    }

    /// Current revolutions per second, 0 if stopped.
    fn current(&self, now: Instant) -> f64 {
        match self.last_event {
            Some(t) if now.duration_since(t) <= IDLE_TIMEOUT => self.rps,
            _ => 0.0,
        }
    }
}

// ───────────────────────── app state ─────────────────────────

enum Msg {
    Status { text: String, connected: bool },
    Csc(Csc),
}

struct App {
    wheel_circumference_m: f64,
    status: String,
    connected: bool,
    started: Instant,

    crank: Rate,
    wheel: Rate,
    crank_seen: bool,
    wheel_seen: bool,

    history: VecDeque<f64>,
    max_cadence: f64,
    cadence_sum: f64,
    pedalling_secs: u64,
    elapsed_secs: u64,
    max_speed: f64,
    last_sample: Instant,

    quit: bool,
}

impl App {
    fn new(wheel_circumference_m: f64) -> Self {
        let now = Instant::now();
        Self {
            wheel_circumference_m,
            status: "Starting…".into(),
            connected: false,
            started: now,
            crank: Rate::new(0xFFFF, MAX_CRANK_RPS),
            wheel: Rate::new(0xFFFF_FFFF, MAX_WHEEL_RPS),
            crank_seen: false,
            wheel_seen: false,
            history: VecDeque::with_capacity(WINDOW_SECS + 1),
            max_cadence: 0.0,
            cadence_sum: 0.0,
            pedalling_secs: 0,
            elapsed_secs: 0,
            max_speed: 0.0,
            last_sample: now,
            quit: false,
        }
    }

    fn reset_stats(&mut self) {
        self.history.clear();
        self.max_cadence = 0.0;
        self.cadence_sum = 0.0;
        self.pedalling_secs = 0;
        self.elapsed_secs = 0;
        self.max_speed = 0.0;
        self.crank.total_revs = 0;
        self.wheel.total_revs = 0;
        self.started = Instant::now();
    }

    fn cadence(&self, now: Instant) -> f64 {
        self.crank.current(now) * 60.0
    }

    fn speed_kmh(&self, now: Instant) -> f64 {
        self.wheel.current(now) * self.wheel_circumference_m * 3.6
    }

    fn distance_km(&self) -> f64 {
        self.wheel.total_revs as f64 * self.wheel_circumference_m / 1000.0
    }

    fn avg_cadence(&self) -> f64 {
        if self.pedalling_secs == 0 {
            0.0
        } else {
            self.cadence_sum / self.pedalling_secs as f64
        }
    }

    fn on_msg(&mut self, msg: Msg, now: Instant) {
        match msg {
            Msg::Status { text, connected } => {
                if connected && !self.connected {
                    // fresh connection: timestamps may have wrapped while away
                    self.crank.rebaseline();
                    self.wheel.rebaseline();
                }
                self.status = text;
                self.connected = connected;
            }
            Msg::Csc(csc) => {
                if let Some((revs, t)) = csc.crank {
                    self.crank_seen = true;
                    self.crank.update(u32::from(revs), t, now);
                }
                if let Some((revs, t)) = csc.wheel {
                    self.wheel_seen = true;
                    self.wheel.update(revs, t, now);
                }
            }
        }
    }

    /// Called once a second: record a history point and update the stats.
    fn sample(&mut self, now: Instant) {
        let cad = self.cadence(now);
        if self.history.len() >= WINDOW_SECS {
            self.history.pop_front();
        }
        self.history.push_back(cad);

        self.elapsed_secs += 1;
        if cad > 0.0 {
            self.pedalling_secs += 1;
            self.cadence_sum += cad;
            self.max_cadence = self.max_cadence.max(cad);
        }
        self.max_speed = self.max_speed.max(self.speed_kmh(now));
    }

    fn on_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('r') => self.reset_stats(),
            _ => {}
        }
    }
}

// ───────────────────────── Bluetooth ─────────────────────────

async fn get_adapter() -> Result<Adapter> {
    let manager = Manager::new()
        .await
        .context("could not talk to BlueZ — is `bluetooth.service` running?")?;
    manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapter found (check `rfkill list` and `bluetoothctl show`)"))
}

fn is_candidate(name: Option<&str>, services: &[Uuid]) -> bool {
    services.contains(&CSC_SERVICE)
        || name.is_some_and(|n| {
            let n = n.to_lowercase();
            n.contains("saris") || n.contains("cadence")
        })
}

async fn find_sensor(adapter: &Adapter) -> Result<Peripheral> {
    loop {
        for p in adapter.peripherals().await? {
            if let Some(props) = p.properties().await? {
                if is_candidate(props.local_name.as_deref(), &props.services) {
                    return Ok(p);
                }
            }
        }
        time::sleep(Duration::from_millis(500)).await;
    }
}

async fn run_session(adapter: &Adapter, tx: &mpsc::UnboundedSender<Msg>) -> Result<()> {
    let status = |text: String, connected: bool| {
        let _ = tx.send(Msg::Status { text, connected });
    };

    status("Scanning for a CSC sensor… (shake it to wake it)".into(), false);
    adapter.start_scan(ScanFilter::default()).await?;
    let sensor = find_sensor(adapter).await;
    let _ = adapter.stop_scan().await;
    let sensor = sensor?;

    let name = sensor
        .properties()
        .await?
        .and_then(|p| p.local_name)
        .unwrap_or_else(|| sensor.address().to_string());

    status(format!("Connecting to {name}…"), false);
    sensor.connect().await?;
    sensor.discover_services().await?;

    let characteristic = sensor
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == CSC_MEASUREMENT)
        .ok_or_else(|| anyhow!("{name} has no CSC Measurement characteristic"))?;

    sensor.subscribe(&characteristic).await?;
    let mut notifications = sensor.notifications().await?;
    status(format!("Connected: {name}"), true);

    while let Some(n) = notifications.next().await {
        if n.uuid == CSC_MEASUREMENT {
            if let Some(csc) = parse_csc(&n.value) {
                if tx.send(Msg::Csc(csc)).is_err() {
                    return Ok(()); // UI gone
                }
            }
        }
    }

    let _ = sensor.disconnect().await;
    Ok(())
}

/// Connect → stream → reconnect, forever.
async fn ble_task(adapter: Adapter, tx: mpsc::UnboundedSender<Msg>) {
    loop {
        let result = run_session(&adapter, &tx).await;
        let text = match result {
            Ok(()) => "Sensor disconnected (asleep?) — rescanning…".to_string(),
            Err(e) => format!("Bluetooth error: {e:#} — retrying…"),
        };
        if tx.send(Msg::Status { text, connected: false }).is_err() {
            return;
        }
        time::sleep(Duration::from_secs(2)).await;
    }
}

// ───────────────────────── UI ─────────────────────────

fn cadence_zone(rpm: f64) -> (Color, &'static str) {
    match rpm {
        r if r < 1.0 => (Color::DarkGray, "stopped"),
        r if r < 70.0 => (Color::Red, "low"),
        r if r < 85.0 => (Color::Yellow, "steady"),
        r if r <= 100.0 => (Color::Green, "target zone"),
        _ => (Color::Cyan, "spinning"),
    }
}

fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn big(text: String, color: Color) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(color).add_modifier(Modifier::BOLD)))
}

fn dim(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(Color::DarkGray)))
}

fn draw(f: &mut Frame, app: &App) {
    let now = Instant::now();
    let [status_area, stats_area, chart_area, help_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(6),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // ── status bar
    let status_color = if app.connected { Color::Green } else { Color::Yellow };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(if app.connected { "● " } else { "○ " }, Style::default().fg(status_color)),
            Span::raw(app.status.clone()),
        ]))
        .block(Block::default().borders(Borders::ALL).title(" Cycling sensor ")),
        status_area,
    );

    // ── stat panels
    let [cad_area, spd_area, ses_area] = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .areas(stats_area);

    let cad = app.cadence(now);
    let (zone_color, zone_label) = cadence_zone(cad);
    let cad_lines = if app.crank_seen {
        vec![
            Line::from(""),
            big(format!("{cad:.0} rpm"), zone_color),
            dim(zone_label),
        ]
    } else {
        vec![Line::from(""), dim("no crank data yet"), dim("(mount sensor on crank arm)")]
    };
    f.render_widget(
        Paragraph::new(cad_lines)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" Cadence ")),
        cad_area,
    );

    let spd_lines = if app.wheel_seen {
        vec![
            big(format!("{:.1} km/h", app.speed_kmh(now)), Color::Cyan),
            Line::from(format!("max  {:.1} km/h", app.max_speed)),
            Line::from(format!("dist {:.2} km", app.distance_km())),
        ]
    } else {
        vec![Line::from(""), dim("no wheel data"), dim("(mount sensor on wheel hub)")]
    };
    f.render_widget(
        Paragraph::new(spd_lines)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" Speed ")),
        spd_area,
    );

    let ses_lines = vec![
        Line::from(format!("avg cadence   {:>5.0} rpm", app.avg_cadence())),
        Line::from(format!("max cadence   {:>5.0} rpm", app.max_cadence)),
        Line::from(format!("pedalling     {:>8}", fmt_duration(app.pedalling_secs))),
        Line::from(format!("elapsed       {:>8}", fmt_duration(app.elapsed_secs))),
    ];
    f.render_widget(
        Paragraph::new(ses_lines)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" Session ")),
        ses_area,
    );

    // ── rolling graph (x = seconds ago, 0 = now)
    let n = app.history.len();
    let points: Vec<(f64, f64)> = app
        .history
        .iter()
        .enumerate()
        .map(|(i, &v)| (-((n - 1 - i) as f64), v))
        .collect();
    let window = WINDOW_SECS as f64;
    let target = [(-window, TARGET_RPM), (0.0, TARGET_RPM)];
    let y_max = ((app.max_cadence.max(100.0) + 10.0) / 20.0).ceil() * 20.0;

    let datasets = vec![
        Dataset::default()
            .name(format!("target {TARGET_RPM:.0}"))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::DarkGray))
            .data(&target),
        Dataset::default()
            .name("cadence")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(zone_color))
            .data(&points),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Cadence — last {} min ", WINDOW_SECS / 60)),
        )
        .x_axis(
            Axis::default()
                .bounds([-window, 0.0])
                .labels(vec!["-3m", "-2m", "-1m", "now"]),
        )
        .y_axis(
            Axis::default()
                .title("rpm")
                .bounds([0.0, y_max])
                .labels(vec![
                    "0".to_string(),
                    format!("{:.0}", y_max / 2.0),
                    format!("{y_max:.0}"),
                ]),
        );
    f.render_widget(chart, chart_area);

    // ── help line
    f.render_widget(
        Paragraph::new(dim(" q quit   r reset stats")),
        help_area,
    );
}

// ───────────────────────── main loop ─────────────────────────

fn spawn_input(tx: mpsc::UnboundedSender<KeyEvent>) {
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if let Event::Key(k) = ev {
                if k.kind == KeyEventKind::Press && tx.send(k).is_err() {
                    break;
                }
            }
        }
    });
}

async fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut msgs: mpsc::UnboundedReceiver<Msg>,
    mut keys: mpsc::UnboundedReceiver<KeyEvent>,
) -> Result<()> {
    let mut redraw = time::interval(Duration::from_millis(100));
    let mut sampler = time::interval(Duration::from_secs(1));
    sampler.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    app.last_sample = Instant::now();

    while !app.quit {
        tokio::select! {
            _ = redraw.tick() => {}
            _ = sampler.tick() => app.sample(Instant::now()),
            Some(m) = msgs.recv() => app.on_msg(m, Instant::now()),
            Some(k) = keys.recv() => app.on_key(k),
        }
        terminal.draw(|f| draw(f, &app))?;
    }
    Ok(())
}

// ───────────────────────── CLI ─────────────────────────

const USAGE: &str = "\
cadence — live dashboard for BLE cycling speed/cadence sensors

USAGE:
    cadence [--wheel-mm <millimetres>]

OPTIONS:
    --wheel-mm <mm>   Wheel circumference for speed/distance (default 2105,
                      i.e. 700x25c). See the table on any tyre manufacturer's site.
    -h, --help        Show this help
";

fn parse_args() -> Result<f64> {
    let mut wheel_mm = 2105.0_f64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--wheel-mm" => {
                wheel_mm = args
                    .next()
                    .ok_or_else(|| anyhow!("--wheel-mm needs a value"))?
                    .parse()
                    .context("--wheel-mm must be a number")?;
                if !(500.0..=3000.0).contains(&wheel_mm) {
                    bail!("--wheel-mm {wheel_mm} doesn't look like a wheel circumference");
                }
            }
            other => bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(wheel_mm / 1000.0)
}

#[tokio::main]
async fn main() -> Result<()> {
    let wheel_m = parse_args()?;

    // Do this before touching the terminal so errors print normally.
    let adapter = get_adapter().await?;

    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (key_tx, key_rx) = mpsc::unbounded_channel();
    let ble = tokio::spawn(ble_task(adapter, msg_tx));
    spawn_input(key_tx);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(wheel_m), msg_rx, key_rx).await;
    ratatui::restore();

    ble.abort();
    result
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn parses_crank_only_packet() {
        // flags=0x02, crank revs=300 (0x012C), time=2048 (0x0800)
        let c = parse_csc(&[0x02, 0x2C, 0x01, 0x00, 0x08]).unwrap();
        assert_eq!(c.crank, Some((300, 2048)));
        assert!(c.wheel.is_none());
    }

    #[test]
    fn parses_wheel_and_crank_packet() {
        let c = parse_csc(&[0x03, 1, 0, 0, 0, 0x00, 0x04, 5, 0, 0x00, 0x02]).unwrap();
        assert_eq!(c.wheel, Some((1, 1024)));
        assert_eq!(c.crank, Some((5, 512)));
    }

    #[test]
    fn rejects_truncated_packet() {
        assert!(parse_csc(&[0x02, 0x2C]).is_none());
        assert!(parse_csc(&[]).is_none());
    }

    #[test]
    fn cadence_90rpm() {
        let now = Instant::now();
        let mut r = Rate::new(0xFFFF, MAX_CRANK_RPS);
        r.update(100, 0, now);
        // 1.5 revs/s = 90 rpm → 3 revs in 2 s (2048 ticks)
        r.update(103, 2048, now + Duration::from_secs(2));
        // first update after baseline is not "idle", so we get a rate
        assert!((r.current(now + Duration::from_secs(2)) * 60.0 - 90.0).abs() < 0.01);
    }

    #[test]
    fn handles_counter_and_timestamp_wraparound() {
        let now = Instant::now();
        let mut r = Rate::new(0xFFFF, MAX_CRANK_RPS);
        r.update(65534, 65000, now);
        // revs wrap 65534 → 2 (4 revs); time wraps 65000 → 1000 (1536 ticks = 1.5 s)
        r.update(2, 1000, now + Duration::from_secs(1));
        let rpm = r.current(now + Duration::from_secs(1)) * 60.0;
        assert!((rpm - 160.0).abs() < 0.01, "got {rpm}");
    }

    #[test]
    fn reports_zero_when_stopped_and_restarts_cleanly() {
        let now = Instant::now();
        let mut r = Rate::new(0xFFFF, MAX_CRANK_RPS);
        r.update(10, 0, now);
        r.update(11, 1024, now + Duration::from_secs(1)); // 60 rpm
        assert!(r.current(now + Duration::from_secs(1)) > 0.9);

        // repeated identical packets while stopped
        r.update(11, 1024, now + Duration::from_secs(5));
        assert_eq!(r.current(now + Duration::from_secs(5)), 0.0);

        // restart after a long gap: first event only re-baselines…
        r.update(12, 9999, now + Duration::from_secs(60));
        assert_eq!(r.current(now + Duration::from_secs(60)), 0.0);
        // …the next gives a proper rate
        r.update(13, 9999 + 1024, now + Duration::from_secs(61));
        assert!((r.current(now + Duration::from_secs(61)) * 60.0 - 60.0).abs() < 0.01);
    }

    #[test]
    fn ignores_glitch_spikes() {
        let now = Instant::now();
        let mut r = Rate::new(0xFFFF, MAX_CRANK_RPS);
        r.update(0, 0, now);
        r.update(1, 1024, now + Duration::from_secs(1)); // 60 rpm
        r.update(50, 1034, now + Duration::from_secs(2)); // absurd
        let rpm = r.current(now + Duration::from_secs(2)) * 60.0;
        assert!((rpm - 60.0).abs() < 0.01, "got {rpm}");
    }

    #[test]
    fn ui_renders_without_panicking() {
        let mut app = App::new(2.105);
        let now = Instant::now();
        app.on_msg(Msg::Status { text: "Connected: Saris CS".into(), connected: true }, now);
        app.on_msg(Msg::Csc(Csc { wheel: None, crank: Some((0, 0)) }), now);
        app.on_msg(
            Msg::Csc(Csc { wheel: None, crank: Some((3, 2048)) }),
            now + Duration::from_secs(2),
        );
        for i in 0..5 {
            app.sample(now + Duration::from_secs(2) + Duration::from_millis(i * 10));
        }
        for size in [(100, 30), (60, 20), (20, 8)] {
            let mut t = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
            t.draw(|f| draw(f, &app)).unwrap();
        }
        // cadence 90 rpm should appear on a normal-size screen
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| draw(f, &app)).unwrap();
        let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("90 rpm"), "buffer:\n{text}");
    }
}
