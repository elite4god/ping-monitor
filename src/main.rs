// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use egui_plot::{Line, MarkerShape, Plot, PlotPoints, Points};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use surge_ping::{Client, Config, PingIdentifier, PingSequence, SurgeError, ICMP};
use tokio::sync::OnceCell;

const HISTORY: usize = 300; // samples kept per host for the graph
const MAX_EVENTS: usize = 500;

// ───────────────────────────── Data model ─────────────────────────────

#[derive(Default, Clone, Copy, PartialEq)]
enum Status {
    #[default]
    Unknown,
    Up,
    Down,
}

#[derive(Default)]
struct Stats {
    status: Status,
    sent: u64,
    recv: u64,
    last: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
    streak: u32, // consecutive failures
    last_error: String,
    history: VecDeque<Option<f64>>, // None = timeout / error
}

fn default_threshold() -> u32 {
    3
}

#[derive(Serialize, Deserialize)]
struct Host {
    id: u64,
    name: String,
    address: String,
    interval_ms: u64,
    timeout_ms: u64,
    enabled: bool,
    /// Consecutive failures before the host is considered DOWN.
    #[serde(default = "default_threshold")]
    fail_threshold: u32,
    #[serde(skip)]
    stats: Stats,
}

#[derive(Default)]
struct State {
    hosts: Vec<Host>,
    events: Vec<(String, String)>,
    next_id: u64,
    paused: bool,
}

type Shared = Arc<Mutex<State>>;

fn push_event(events: &mut Vec<(String, String)>, msg: String) {
    let t = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    events.push((t, msg));
    if events.len() > MAX_EVENTS {
        events.remove(0);
    }
}

// ───────────────────────────── Persistence ─────────────────────────────

fn config_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "PingMonitor")?;
    std::fs::create_dir_all(dirs.config_dir()).ok()?;
    Some(dirs.config_dir().join("hosts.json"))
}

fn save_hosts(hosts: &[Host]) {
    if let Some(path) = config_path() {
        if let Ok(json) = serde_json::to_string_pretty(hosts) {
            let _ = std::fs::write(path, json);
        }
    }
}

fn load_hosts() -> Vec<Host> {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ───────────────────────────── ICMP engine ─────────────────────────────

/// Lazily-created ICMP clients (one per IP family).
#[derive(Default)]
struct Pingers {
    v4: OnceCell<Result<Arc<Client>, String>>,
    v6: OnceCell<Result<Arc<Client>, String>>,
}

impl Pingers {
    /// Returns a shared handle. The Client itself is never cloned or dropped
    /// while the app runs: dropping a surge-ping Client stops its receive task.
    async fn get(&self, v6: bool) -> Result<Arc<Client>, String> {
        let cell = if v6 { &self.v6 } else { &self.v4 };
        cell.get_or_init(|| async move {
            let cfg = if v6 {
                Config::builder().kind(ICMP::V6).build()
            } else {
                Config::default()
            };
            Client::new(&cfg)
                .map(Arc::new)
                .map_err(|e| format!("Cannot open ICMP socket: {e}"))
        })
        .await
        .clone()
    }
}

async fn resolve(address: &str) -> Result<IpAddr, String> {
    if let Ok(ip) = address.parse::<IpAddr>() {
        return Ok(ip);
    }
    let all: Vec<_> = tokio::net::lookup_host((address, 0))
        .await
        .map_err(|e| format!("DNS lookup failed: {e}"))?
        .collect();
    all.iter()
        .find(|a| a.is_ipv4())
        .or(all.first())
        .map(|a| a.ip())
        .ok_or_else(|| "DNS lookup returned no addresses".to_string())
}

/// Send one echo request; returns the round-trip time in milliseconds.
async fn ping_once(
    pingers: &Pingers,
    address: &str,
    timeout: Duration,
    id: u64,
    seq: u16,
) -> Result<f64, String> {
    let ip = resolve(address).await?;
    let client = pingers.get(ip.is_ipv6()).await?;
    let ident = PingIdentifier((id as u16) ^ (std::process::id() as u16));
    let mut pinger = client.pinger(ip, ident).await;
    pinger.timeout(timeout);
    match pinger.ping(PingSequence(seq), &[0u8; 32]).await {
        Ok((_packet, rtt)) => Ok(rtt.as_secs_f64() * 1000.0),
        Err(SurgeError::Timeout { .. }) => Err("Request timed out".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// One of these tasks runs per host. It exits when the host is removed.
async fn monitor(id: u64, shared: Shared, pingers: Arc<Pingers>) {
    let mut seq: u16 = 0;
    loop {
        let (address, interval, timeout, active) = {
            let s = shared.lock().unwrap();
            match s.hosts.iter().find(|h| h.id == id) {
                Some(h) => (
                    h.address.clone(),
                    Duration::from_millis(h.interval_ms.max(100)),
                    Duration::from_millis(h.timeout_ms.max(100)),
                    h.enabled && !s.paused,
                ),
                None => return, // host was deleted
            }
        };

        if !active {
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }

        let started = Instant::now();
        seq = seq.wrapping_add(1);
        let result = ping_once(&pingers, &address, timeout, id, seq).await;

        {
            let mut s = shared.lock().unwrap();
            let State { hosts, events, .. } = &mut *s;
            let Some(h) = hosts.iter_mut().find(|h| h.id == id) else {
                return;
            };
            let st = &mut h.stats;
            st.sent += 1;
            if st.history.len() >= HISTORY {
                st.history.pop_front();
            }
            let mut event = None;
            match result {
                Ok(ms) => {
                    st.recv += 1;
                    st.last = Some(ms);
                    st.min = Some(st.min.map_or(ms, |m| m.min(ms)));
                    st.max = Some(st.max.map_or(ms, |m| m.max(ms)));
                    st.sum += ms;
                    st.streak = 0;
                    st.last_error.clear();
                    st.history.push_back(Some(ms));
                    if st.status == Status::Down {
                        event = Some(format!("{} ({}) is back UP", h.name, h.address));
                    }
                    st.status = Status::Up;
                }
                Err(e) => {
                    st.last = None;
                    st.streak += 1;
                    st.last_error = e;
                    st.history.push_back(None);
                    if st.streak >= h.fail_threshold && st.status != Status::Down {
                        st.status = Status::Down;
                        event = Some(format!(
                            "{} ({}) is DOWN: {}",
                            h.name, h.address, st.last_error
                        ));
                    }
                }
            }
            if let Some(msg) = event {
                push_event(events, msg);
            }
        }

        let next = started + interval;
        tokio::time::sleep(next.saturating_duration_since(Instant::now())).await;
    }
}

// ───────────────────────────── GUI ─────────────────────────────

struct PingMonApp {
    rt: tokio::runtime::Runtime,
    shared: Shared,
    pingers: Arc<Pingers>,
    selected: Option<u64>,
    new_name: String,
    new_addr: String,
    new_interval: u64,
    new_timeout: u64,
}

impl PingMonApp {
    fn new() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        let hosts = load_hosts();
        let next_id = hosts.iter().map(|h| h.id).max().map_or(1, |m| m + 1);
        let shared: Shared = Arc::new(Mutex::new(State {
            hosts,
            next_id,
            ..Default::default()
        }));
        let pingers = Arc::new(Pingers::default());

        let ids: Vec<u64> = shared.lock().unwrap().hosts.iter().map(|h| h.id).collect();
        for id in ids {
            rt.spawn(monitor(id, shared.clone(), pingers.clone()));
        }

        Self {
            rt,
            shared,
            pingers,
            selected: None,
            new_name: String::new(),
            new_addr: String::new(),
            new_interval: 1000,
            new_timeout: 2000,
        }
    }

    fn add_host(&mut self, st: &mut State) {
        let address = self.new_addr.trim().to_string();
        if address.is_empty() {
            return;
        }
        let name = if self.new_name.trim().is_empty() {
            address.clone()
        } else {
            self.new_name.trim().to_string()
        };
        let id = st.next_id;
        st.next_id += 1;
        st.hosts.push(Host {
            id,
            name,
            address,
            interval_ms: self.new_interval,
            timeout_ms: self.new_timeout,
            enabled: true,
            fail_threshold: default_threshold(),
            stats: Stats::default(),
        });
        save_hosts(&st.hosts);
        self.rt
            .spawn(monitor(id, self.shared.clone(), self.pingers.clone()));
        self.selected = Some(id);
        self.new_name.clear();
        self.new_addr.clear();
    }
}

fn fmt_ms(v: Option<f64>) -> String {
    v.map_or("—".to_string(), |v| format!("{v:.1} ms"))
}

impl eframe::App for PingMonApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(500));

        let shared = self.shared.clone();
        let mut guard = shared.lock().unwrap();
        let st: &mut State = &mut guard;

        // ── Top bar: add host ──
        egui::TopBottomPanel::top("add_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(110.0));
                ui.label("Host / IP");
                let addr = ui.add(
                    egui::TextEdit::singleline(&mut self.new_addr)
                        .desired_width(160.0)
                        .hint_text("8.8.8.8 or example.com"),
                );
                ui.label("Interval");
                ui.add(
                    egui::DragValue::new(&mut self.new_interval)
                        .range(200..=3_600_000)
                        .speed(10)
                        .suffix(" ms"),
                );
                ui.label("Timeout");
                ui.add(
                    egui::DragValue::new(&mut self.new_timeout)
                        .range(200..=60_000)
                        .speed(10)
                        .suffix(" ms"),
                );
                let enter = addr.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("➕ Add").clicked() || enter {
                    self.add_host(st);
                }
                ui.separator();
                ui.checkbox(&mut st.paused, "Pause all");
            });
            ui.add_space(4.0);
        });

        // ── Bottom: event log (outermost) ──
        egui::TopBottomPanel::bottom("events")
            .resizable(true)
            .default_height(120.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Event log");
                    if ui.small_button("Clear").clicked() {
                        st.events.clear();
                    }
                });
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (t, msg) in &st.events {
                            let color = if msg.contains("DOWN") {
                                Color32::from_rgb(230, 80, 80)
                            } else {
                                Color32::from_rgb(90, 200, 120)
                            };
                            ui.horizontal(|ui| {
                                ui.weak(t);
                                ui.label(RichText::new(msg).color(color));
                            });
                        }
                    });
            });

        // ── Bottom: detail panel for the selected host ──
        if let Some(sel) = self.selected {
            let mut remove = false;
            let mut changed = false;
            egui::TopBottomPanel::bottom("detail")
                .resizable(true)
                .default_height(240.0)
                .show(ctx, |ui| {
                    let Some(h) = st.hosts.iter_mut().find(|h| h.id == sel) else {
                        ui.label("Host no longer exists.");
                        return;
                    };
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("{}  ({})", h.name, h.address));
                        changed |= ui.checkbox(&mut h.enabled, "Enabled").changed();
                        ui.label("Interval");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut h.interval_ms)
                                    .range(200..=3_600_000)
                                    .speed(10)
                                    .suffix(" ms"),
                            )
                            .changed();
                        ui.label("Timeout");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut h.timeout_ms)
                                    .range(200..=60_000)
                                    .speed(10)
                                    .suffix(" ms"),
                            )
                            .changed();
                        ui.label("Down after");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut h.fail_threshold)
                                    .range(1..=100)
                                    .suffix(" fails"),
                            )
                            .changed();
                        if ui.button("Reset stats").clicked() {
                            h.stats = Stats::default();
                        }
                        if ui.button("🗑 Remove").clicked() {
                            remove = true;
                        }
                    });
                    if !h.stats.last_error.is_empty() {
                        ui.colored_label(Color32::from_rgb(230, 80, 80), &h.stats.last_error);
                    }

                    // Build graph data: line segments broken at timeouts.
                    let n = h.stats.history.len();
                    let mut segs: Vec<Vec<[f64; 2]>> = vec![vec![]];
                    let mut ok_pts = Vec::new();
                    let mut lost_pts = Vec::new();
                    for (i, s) in h.stats.history.iter().enumerate() {
                        let x = i as f64 - n as f64 + 1.0;
                        match s {
                            Some(v) => {
                                segs.last_mut().unwrap().push([x, *v]);
                                ok_pts.push([x, *v]);
                            }
                            None => {
                                lost_pts.push([x, 0.0]);
                                if !segs.last().unwrap().is_empty() {
                                    segs.push(vec![]);
                                }
                            }
                        }
                    }
                    Plot::new("latency_plot")
                        .allow_scroll(false)
                        .include_y(0.0)
                        .x_axis_label("samples ago")
                        .y_axis_label("ms")
                        .show(ui, |p| {
                            for s in segs {
                                if s.len() > 1 {
                                    p.line(
                                        Line::new(PlotPoints::from(s))
                                            .color(Color32::from_rgb(90, 200, 120)),
                                    );
                                }
                            }
                            if !ok_pts.is_empty() {
                                p.points(
                                    Points::new(ok_pts)
                                        .radius(2.0_f32)
                                        .color(Color32::from_rgb(90, 200, 120)),
                                );
                            }
                            if !lost_pts.is_empty() {
                                p.points(
                                    Points::new(lost_pts)
                                        .shape(MarkerShape::Cross)
                                        .radius(5.0_f32)
                                        .color(Color32::from_rgb(230, 80, 80)),
                                );
                            }
                        });
                });
            if remove {
                st.hosts.retain(|h| h.id != sel);
                self.selected = None;
                changed = true;
            }
            if changed {
                save_hosts(&st.hosts);
            }
        }

        // ── Centre: host table ──
        egui::CentralPanel::default().show(ctx, |ui| {
            if st.hosts.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No hosts yet — add one using the bar above.");
                });
                return;
            }
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("hosts")
                    .striped(true)
                    .num_columns(9)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        for head in ["", "Name", "Address", "Last", "Avg", "Min", "Max", "Sent", "Loss"] {
                            ui.strong(head);
                        }
                        ui.end_row();

                        for h in st.hosts.iter() {
                            let s = &h.stats;
                            let (color, tip) = if !h.enabled || st.paused {
                                (Color32::GRAY, "Paused")
                            } else {
                                match s.status {
                                    Status::Unknown => (Color32::GRAY, "Waiting…"),
                                    Status::Up => (Color32::from_rgb(90, 200, 120), "Up"),
                                    Status::Down => (Color32::from_rgb(230, 80, 80), "Down"),
                                }
                            };
                            ui.label(RichText::new("●").color(color).size(16.0))
                                .on_hover_text(tip);
                            let mut name = RichText::new(&h.name);
                            if s.status == Status::Down {
                                name = name.color(Color32::from_rgb(230, 80, 80)).strong();
                            }
                            if ui.selectable_label(self.selected == Some(h.id), name).clicked() {
                                self.selected = Some(h.id);
                            }
                            ui.label(&h.address);
                            ui.label(fmt_ms(s.last));
                            let avg = (s.recv > 0).then(|| s.sum / s.recv as f64);
                            ui.label(fmt_ms(avg));
                            ui.label(fmt_ms(s.min));
                            ui.label(fmt_ms(s.max));
                            ui.label(s.sent.to_string());
                            let loss = if s.sent > 0 {
                                100.0 * (s.sent - s.recv) as f64 / s.sent as f64
                            } else {
                                0.0
                            };
                            ui.label(format!("{loss:.1}%"));
                            ui.end_row();
                        }
                    });
            });
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 660.0])
            .with_title("Ping Monitor"),
        ..Default::default()
    };
    eframe::run_native(
        "Ping Monitor",
        options,
        Box::new(|_cc| Ok(Box::new(PingMonApp::new()))),
    )
}