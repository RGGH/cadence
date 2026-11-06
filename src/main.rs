//! cadence — a terminal dashboard for BLE Cycling Speed & Cadence sensors.
//!
//! Works with any sensor that exposes the standard Cycling Speed and Cadence
//! Service (0x1816), e.g. the Saris Speed/Cadence sensor.
//!
//! Rides are recorded to `~/.local/share/cadence/rides.db` (see `db.rs`).

mod db;

use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use btleplug::api::{
    bleuuid::uuid_from_u16, Central, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::{DateTime, Local};
use futures::StreamExt;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Row, Table, TableState},
    DefaultTerminal, Frame,
};
use tokio::{sync::mpsc, time};
use uuid::Uuid;

use db::{unix_now, Db, RideStats, RideSummary, MIN_SAVED_PEDALLING_SECS};

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
/// How many rides the history screen loads.
const HISTORY_LIMIT: usize = 500;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Live,
    History,
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

    // persistence
    db: Option<Db>,
    /// Row id of the ride currently being recorded.
    ride_id: Option<i64>,
    /// Last database problem, shown in the help line. The app keeps working
    /// (just without saving) if the DB is unavailable.
    db_note: Option<String>,

    // history screen
    view: View,
    rides: Vec<RideSummary>,
    selected: usize,
    confirm_delete: bool,

    quit: bool,
}

impl App {
    fn new(wheel_circumference_m: f64, db: Option<Db>, db_note: Option<String>) -> Self {
        let now = Instant::now();
        let mut app = Self {
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
            db,
            ride_id: None,
            db_note,
            view: View::Live,
            rides: Vec::new(),
            selected: 0,
            confirm_delete: false,
            quit: false,
        };
        app.begin_ride();
        app
    }

    // ── ride recording

    /// Open a new ride row in the DB (no-op without a DB).
    fn begin_ride(&mut self) {
        self.ride_id = None;
        if let Some(db) = &self.db {
            match db.start_ride(unix_now(), TARGET_RPM) {
                Ok(id) => self.ride_id = Some(id),
                Err(e) => self.db_note = Some(format!("can't save ride: {e:#}")),
            }
        }
    }

    /// Close the current ride: save its totals, or discard it if it was
    /// just someone fiddling with the sensor.
    fn end_ride(&mut self) {
        let Some(id) = self.ride_id.take() else { return };
        let Some(db) = &self.db else { return };

        let result = if self.pedalling_secs < MIN_SAVED_PEDALLING_SECS {
            db.delete_ride(id)
        } else {
            db.finish_ride(
                id,
                unix_now(),
                &RideStats {
                    pedalling_secs: self.pedalling_secs,
                    avg_cadence: self.avg_cadence(),
                    max_cadence: self.max_cadence,
                },
            )
        };
        if let Err(e) = result {
            self.db_note = Some(format!("can't save ride: {e:#}"));
        }
    }

    /// Save the current ride and start again from zero.
    fn new_ride(&mut self) {
        self.end_ride();
        self.history.clear();
        self.max_cadence = 0.0;
        self.cadence_sum = 0.0;
        self.pedalling_secs = 0;
        self.elapsed_secs = 0;
        self.max_speed = 0.0;
        self.crank.total_revs = 0;
        self.wheel.total_revs = 0;
        self.started = Instant::now();
        self.begin_ride();
    }

    // ── live numbers

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

    /// Called once a second: record a history point, update the stats and
    /// write the sample to the database.
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

        if let (Some(db), Some(id)) = (&self.db, self.ride_id) {
            if let Err(e) = db.add_sample(id, self.elapsed_secs, cad) {
                self.db_note = Some(format!("write failed: {e:#}"));
            }
        }
    }

    // ── history screen

    fn reload_rides(&mut self) {
        match &self.db {
            Some(db) => match db.list_rides(HISTORY_LIMIT) {
                Ok(rides) => self.rides = rides,
                Err(e) => self.db_note = Some(format!("can't read history: {e:#}")),
            },
            None => self.rides.clear(),
        }
        self.selected = self.selected.min(self.rides.len().saturating_sub(1));
    }

    fn open_history(&mut self) {
        self.selected = 0;
        self.confirm_delete = false;
        self.reload_rides();
        self.view = View::History;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rides.is_empty() {
            return;
        }
        let max = (self.rides.len() - 1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
    }

    /// First press arms the delete, second press performs it.
    fn delete_selected(&mut self) {
        let id = match self.rides.get(self.selected) {
            Some(r) => r.id,
            None => return,
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            return;
        }
        self.confirm_delete = false;
        if let Some(db) = &self.db {
            if let Err(e) = db.delete_ride(id) {
                self.db_note = Some(format!("can't delete ride: {e:#}"));
            }
        }
        self.reload_rides();
    }

    // ── input

    fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match self.view {
            View::Live => match k.code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('r') => self.new_ride(),
                KeyCode::Char('h') | KeyCode::Tab => self.open_history(),
                _ => {}
            },
            View::History => {
                if !matches!(k.code, KeyCode::Char('x') | KeyCode::Delete) {
                    self.confirm_delete = false;
                }
                match k.code {
                    KeyCode::Char('q') => self.quit = true,
                    KeyCode::Esc | KeyCode::Tab | KeyCode::Char('h') => self.view = View::Live,
                    KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                    KeyCode::Char('x') | KeyCode::Delete => self.delete_selected(),
                    _ => {}
                }
            }
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

/// Local-time date for the history list, e.g. "Fri 06 Nov  14:32".
fn fmt_date(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .map(|d| d.with_timezone(&Local).format("%a %d %b  %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

fn big(text: String, color: Color) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(color).add_modifier(Modifier::BOLD)))
}

fn dim(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(Color::DarkGray)))
}

fn help_line(app: &App) -> Line<'static> {
    let text = match (app.view, app.confirm_delete) {
        (View::Live, _) => " q quit   r save & start new ride   h history",
        (View::History, false) => " ↑/↓ select   x delete   h back   q quit",
        (View::History, true) => " press x again to delete this ride (any other key cancels)",
    };
    let mut spans = vec![Span::styled(text, Style::default().fg(Color::DarkGray))];
    if let Some(note) = &app.db_note {
        spans.push(Span::styled(
            format!("   ⚠ {note}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

fn draw(f: &mut Frame, app: &App) {
    let [status_area, body_area, help_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(14),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // ── status bar
    let status_color = if app.connected { Color::Green } else { Color::Yellow };
    let mut status_spans = vec![
        Span::styled(if app.connected { "● " } else { "○ " }, Style::default().fg(status_color)),
        Span::raw(app.status.clone()),
    ];
    if app.ride_id.is_some() {
        status_spans.push(Span::styled("   ● rec", Style::default().fg(Color::DarkGray)));
    }
    f.render_widget(
        Paragraph::new(Line::from(status_spans))
            .block(Block::default().borders(Borders::ALL).title(" Cycling sensor ")),
        status_area,
    );

    match app.view {
        View::Live => draw_live(f, app, body_area),
        View::History => draw_history(f, app, body_area),
    }

    f.render_widget(Paragraph::new(help_line(app)), help_area);
}

fn draw_live(f: &mut Frame, app: &App, area: Rect) {
    let now = Instant::now();
    let [stats_area, chart_area] =
        Layout::vertical([Constraint::Length(6), Constraint::Min(8)]).areas(area);

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
}

fn draw_history(f: &mut Frame, app: &App, area: Rect) {
    let total_secs: u64 = app.rides.iter().map(|r| r.duration_secs()).sum();
    let block = Block::default().borders(Borders::ALL).title(format!(
        " History — {} rides, {} total ",
        app.rides.len(),
        fmt_duration(total_secs)
    ));

    if app.rides.is_empty() {
        let note = if app.db.is_some() {
            format!("(rides under {MIN_SAVED_PEDALLING_SECS} s of pedalling aren't kept)")
        } else {
            "(the ride database couldn't be opened)".to_string()
        };
        f.render_widget(
            Paragraph::new(vec![Line::from(""), dim("no saved rides yet"), dim(&note)])
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        return;
    }

    let header = Row::new(["Date", "Length", "Pedalling", "Avg", "Max"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let rows = app.rides.iter().map(|r| {
        Row::new(vec![
            fmt_date(r.started_at),
            fmt_duration(r.duration_secs()),
            fmt_duration(r.pedalling_secs),
            format!("{:.0} rpm", r.avg_cadence),
            format!("{:.0} rpm", r.max_cadence),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(17),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .column_spacing(2)
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ")
    .block(block);

    let mut state = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(table, area, &mut state);
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

    // Normal exit: save the ride. (After a crash, Db::open recovers it.)
    app.end_ride();
    Ok(())
}

// ───────────────────────── CLI ─────────────────────────

const USAGE: &str = "\
cadence — live dashboard for BLE cycling speed/cadence sensors

USAGE:
    cadence [--wheel-mm <millimetres>] [--db <path>]

OPTIONS:
    --wheel-mm <mm>   Wheel circumference for speed/distance (default 2105,
                      i.e. 700x25c). See the table on any tyre manufacturer's site.
    --db <path>       Ride database (default ~/.local/share/cadence/rides.db)
    -h, --help        Show this help
";

struct Args {
    wheel_m: f64,
    db_path: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut wheel_mm = 2105.0_f64;
    let mut db_path = None;
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
            "--db" => {
                db_path = Some(PathBuf::from(
                    args.next().ok_or_else(|| anyhow!("--db needs a path"))?,
                ));
            }
            other => bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(Args { wheel_m: wheel_mm / 1000.0, db_path })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;

    // Do this before touching the terminal so errors print normally.
    let adapter = get_adapter().await?;

    // A broken database shouldn't stop you riding — just run without saving.
    let opened = match &args.db_path {
        Some(p) => Db::open(p),
        None => Db::open_default(),
    };
    let (db, db_note) = match opened {
        Ok(db) => (Some(db), None),
        Err(e) => (None, Some(format!("rides won't be saved: {e:#}"))),
    };

    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (key_tx, key_rx) = mpsc::unbounded_channel();
    let ble = tokio::spawn(ble_task(adapter, msg_tx));
    spawn_input(key_tx);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(args.wheel_m, db, db_note), msg_rx, key_rx).await;
    ratatui::restore();

    ble.abort();
    result
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn screen_text(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    /// Feed a steady 90 rpm and tick `secs` samples.
    fn pedal(app: &mut App, secs: u64) {
        let now = Instant::now();
        app.on_msg(Msg::Csc(Csc { wheel: None, crank: Some((0, 0)) }), now);
        app.on_msg(
            Msg::Csc(Csc { wheel: None, crank: Some((3, 2048)) }),
            now + Duration::from_secs(2),
        );
        for _ in 0..secs {
            app.sample(now + Duration::from_secs(2));
        }
    }

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
        let mut app = App::new(2.105, None, None);
        app.on_msg(
            Msg::Status { text: "Connected: Saris CS".into(), connected: true },
            Instant::now(),
        );
        pedal(&mut app, 5);
        for size in [(100, 30), (60, 20), (20, 8)] {
            let mut t = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
            t.draw(|f| draw(f, &app)).unwrap();
        }
        // cadence 90 rpm should appear on a normal-size screen
        let text = screen_text(&app, 100, 30);
        assert!(text.contains("90 rpm"), "buffer:\n{text}");
    }

    #[test]
    fn short_rides_are_not_saved() {
        let mut app = App::new(2.105, Some(Db::open_in_memory().unwrap()), None);
        pedal(&mut app, 3); // under MIN_SAVED_PEDALLING_SECS
        app.end_ride();
        app.open_history();
        assert!(app.rides.is_empty());
    }

    #[test]
    fn ride_is_saved_and_shows_in_history() {
        let mut app = App::new(2.105, Some(Db::open_in_memory().unwrap()), None);
        pedal(&mut app, 12);
        assert!(app.ride_id.is_some());

        // `r` saves this ride and starts a fresh one
        app.new_ride();
        assert_eq!(app.elapsed_secs, 0);
        assert!(app.ride_id.is_some());

        app.open_history();
        assert_eq!(app.rides.len(), 1);
        assert_eq!(app.rides[0].pedalling_secs, 12);
        assert!((app.rides[0].avg_cadence - 90.0).abs() < 0.01);

        let text = screen_text(&app, 100, 30);
        assert!(text.contains("History"), "buffer:\n{text}");
        assert!(text.contains("90 rpm"), "buffer:\n{text}");
    }

    #[test]
    fn delete_needs_two_presses() {
        let mut app = App::new(2.105, Some(Db::open_in_memory().unwrap()), None);
        pedal(&mut app, 12);
        app.new_ride();
        app.open_history();
        assert_eq!(app.rides.len(), 1);

        app.delete_selected(); // arms
        assert_eq!(app.rides.len(), 1);
        app.delete_selected(); // deletes
        assert!(app.rides.is_empty());
    }
}
