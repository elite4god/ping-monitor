// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod targets;

use eframe::egui::{self, Align2, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use egui_plot::{Line, MarkerShape, Plot, PlotPoints, Points};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
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
    resolved: Option<IpAddr>, // last address the host name resolved to
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

/// Spread the first ping of many hosts out so they don't all fire at once.
fn stagger(index: usize, interval_ms: u64) -> Duration {
    Duration::from_millis((index as u64 * 15) % interval_ms.max(1))
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

/// Resolve a host name (or parse an IP literal). Prefers IPv4 for DNS names.
async fn resolve(address: &str, timeout: Duration) -> Result<IpAddr, String> {
    if let Ok(ip) = address.parse::<IpAddr>() {
        return Ok(ip);
    }
    let lookup = tokio::net::lookup_host((address, 0));
    let found = tokio::time::timeout(timeout, lookup)
        .await
        .map_err(|_| "DNS lookup timed out".to_string())?
        .map_err(|e| format!("DNS lookup failed: {e}"))?;
    let all: Vec<_> = found.collect();
    all.iter()
        .find(|a| a.is_ipv4())
        .or(all.first())
        .map(|a| a.ip())
        .ok_or_else(|| "DNS lookup returned no addresses".to_string())
}

/// Send one echo request; returns the round-trip time in milliseconds.
async fn ping_ip(
    pingers: &Pingers,
    ip: IpAddr,
    timeout: Duration,
    id: u64,
    seq: u16,
) -> Result<f64, String> {
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
async fn monitor(id: u64, shared: Shared, pingers: Arc<Pingers>, start_delay: Duration) {
    // Different hosts start with different sequence numbers so two entries that
    // resolve to the same IP never share (ip, seq) keys.
    let mut seq: u16 = (id as u16).wrapping_mul(7919);
    tokio::time::sleep(start_delay).await;
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
        let (resolved, result) = match resolve(&address, timeout).await {
            Ok(ip) => (Some(ip), ping_ip(&pingers, ip, timeout, id, seq).await),
            Err(e) => (None, Err(e)),
        };

        {
            let mut s = shared.lock().unwrap();
            let State { hosts, events, .. } = &mut *s;
            let Some(h) = hosts.iter_mut().find(|h| h.id == id) else {
                return;
            };
            let st = &mut h.stats;
            if resolved.is_some() {
                st.resolved = resolved;
            }
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

const RED: Color32 = Color32::from_rgb(230, 80, 80);
const GREEN: Color32 = Color32::from_rgb(90, 200, 120);

struct PingMonApp {
    rt: tokio::runtime::Runtime,
    shared: Shared,
    pingers: Arc<Pingers>,
    selected: Option<u64>,
    new_name: String,
    new_targets: String,
    new_interval: u64,
    new_timeout: u64,
    message: String,
    message_is_error: bool,
    pending_import: bool,
    confirm_clear: bool,
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

        let hosts: Vec<(u64, u64)> = shared
            .lock()
            .unwrap()
            .hosts
            .iter()
            .map(|h| (h.id, h.interval_ms))
            .collect();
        for (i, (id, interval_ms)) in hosts.into_iter().enumerate() {
            rt.spawn(monitor(
                id,
                shared.clone(),
                pingers.clone(),
                stagger(i, interval_ms),
            ));
        }

        Self {
            rt,
            shared,
            pingers,
            selected: None,
            new_name: String::new(),
            new_targets: String::new(),
            new_interval: 1000,
            new_timeout: 2000,
            message: String::new(),
            message_is_error: false,
            pending_import: false,
            confirm_clear: false,
        }
    }

    fn set_message(&mut self, msg: String, is_error: bool) {
        self.message = msg;
        self.message_is_error = is_error;
    }

    /// Create a monitored host for every target. Returns true when the input
    /// was fully accepted (nothing was rejected).
    fn add_targets(&mut self, st: &mut State, ex: targets::Expanded, name: &str) -> bool {
        let multi = ex.targets.len() > 1;
        let mut existing: HashSet<String> = st
            .hosts
            .iter()
            .map(|h| h.address.to_ascii_lowercase())
            .collect();
        let mut added = 0usize;
        let mut dups = 0usize;

        for addr in ex.targets {
            if !existing.insert(addr.to_ascii_lowercase()) {
                dups += 1;
                continue;
            }
            let host_name = if name.is_empty() {
                addr.clone()
            } else if multi {
                format!("{name} {addr}")
            } else {
                name.to_string()
            };
            let id = st.next_id;
            st.next_id += 1;
            st.hosts.push(Host {
                id,
                name: host_name,
                address: addr,
                interval_ms: self.new_interval,
                timeout_ms: self.new_timeout,
                enabled: true,
                fail_threshold: default_threshold(),
                stats: Stats::default(),
            });
            self.rt.spawn(monitor(
                id,
                self.shared.clone(),
                self.pingers.clone(),
                stagger(added, self.new_interval),
            ));
            if !multi {
                self.selected = Some(id);
            }
            added += 1;
        }
        if added > 0 {
            save_hosts(&st.hosts);
        }

        let mut msg = if added == 0 && dups == 0 && ex.errors.is_empty() {
            "Nothing to add — enter a host, IP, subnet, range or list.".to_string()
        } else {
            format!("Added {added} host{}", if added == 1 { "" } else { "s" })
        };
        if dups > 0 {
            msg += &format!(", {dups} already monitored");
        }
        if !ex.errors.is_empty() {
            let shown: Vec<&str> = ex.errors.iter().take(3).map(String::as_str).collect();
            msg += &format!(
                ". {} rejected: {}{}",
                ex.errors.len(),
                shown.join("; "),
                if ex.errors.len() > 3 { "; …" } else { "" }
            );
        }
        let ok = ex.errors.is_empty() && (added > 0 || dups > 0);
        self.set_message(msg, added == 0 && !ex.errors.is_empty());
        ok
    }

    /// Ask for a CSV / text file and add every host listed in it.
    fn import_csv(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Import hosts from CSV")
            .add_filter("CSV / text", &["csv", "txt"])
            .pick_file()
        else {
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                self.set_message(format!("Cannot read {}: {e}", path.display()), true);
                return;
            }
        };
        let ex = targets::expand_csv(&String::from_utf8_lossy(&bytes));
        let shared = self.shared.clone();
        let mut st = shared.lock().unwrap();
        self.add_targets(&mut st, ex, "");
    }
}

fn fmt_ms(v: Option<f64>) -> String {
    v.map_or("—".to_string(), |v| format!("{v:.1} ms"))
}

impl eframe::App for PingMonApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(500));

        // The file dialog blocks, so run it before taking the lock (otherwise
        // all monitoring would stall while the dialog is open).
        if std::mem::take(&mut self.pending_import) {
            self.import_csv();
        }

        let shared = self.shared.clone();
        let mut guard = shared.lock().unwrap();
        let st: &mut State = &mut guard;

        // ── Top bar: add hosts ──
        egui::TopBottomPanel::top("add_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_name)
                        .desired_width(100.0)
                        .hint_text("optional"),
                );
                ui.label("Target(s)");
                let targets_box = ui.add(
                    egui::TextEdit::singleline(&mut self.new_targets)
                        .desired_width(330.0)
                        .hint_text("host, IP, 10.0.0.0/24, 10.1.1.1-254, or a comma list"),
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
                let enter =
                    targets_box.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("➕ Add").clicked() || enter {
                    let ex = targets::expand_input(&self.new_targets);
                    let name = self.new_name.trim().to_string();
                    if self.add_targets(st, ex, &name) {
                        self.new_targets.clear();
                        self.new_name.clear();
                    }
                }
                if ui
                    .button("📂 Import CSV…")
                    .on_hover_text("Add every IP / host / subnet / range listed in a CSV or text file")
                    .clicked()
                {
                    self.pending_import = true;
                }
                ui.separator();
                ui.checkbox(&mut st.paused, "Pause all");
                if ui
                    .add_enabled(!st.hosts.is_empty(), egui::Button::new("🗑 Remove all"))
                    .clicked()
                {
                    self.confirm_clear = true;
                }
            });
            if !self.message.is_empty() {
                let color = if self.message_is_error {
                    RED
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.colored_label(color, &self.message);
            }
            ui.add_space(4.0);
        });

        // ── Bottom: event log (outermost) ──
        egui::TopBottomPanel::bottom("events")
            .resizable(true)
            .default_height(110.0)
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
                            let color = if msg.contains("DOWN") { RED } else { GREEN };
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
                .default_height(230.0)
                .show(ctx, |ui| {
                    let Some(h) = st.hosts.iter_mut().find(|h| h.id == sel) else {
                        ui.label("Host no longer exists.");
                        return;
                    };
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        let mut title = format!("{}  ({})", h.name, h.address);
                        if let Some(ip) = h.stats.resolved {
                            if h.address.parse::<IpAddr>().is_err() {
                                title += &format!("  →  {ip}");
                            }
                        }
                        ui.strong(title);
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
                        ui.colored_label(RED, &h.stats.last_error);
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
                                    p.line(Line::new(PlotPoints::from(s)).color(GREEN));
                                }
                            }
                            if !ok_pts.is_empty() {
                                p.points(Points::new(ok_pts).radius(2.0_f32).color(GREEN));
                            }
                            if !lost_pts.is_empty() {
                                p.points(
                                    Points::new(lost_pts)
                                        .shape(MarkerShape::Cross)
                                        .radius(5.0_f32)
                                        .color(RED),
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

        // ── Confirm "Remove all" ──
        if self.confirm_clear {
            egui::Window::new("Remove all hosts?")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "This stops monitoring and removes all {} hosts.",
                        st.hosts.len()
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Remove all").clicked() {
                            st.hosts.clear();
                            self.selected = None;
                            save_hosts(&st.hosts);
                            self.confirm_clear = false;
                            self.set_message("All hosts removed.".to_string(), false);
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_clear = false;
                        }
                    });
                });
        }

        // ── Centre: host table (virtualised, so thousands of rows are fine) ──
        egui::CentralPanel::default().show(ctx, |ui| {
            if st.hosts.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No hosts yet — add one using the bar above.");
                });
                return;
            }
            let paused = st.paused;
            let hosts = &st.hosts;
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(22.0))
                .column(Column::initial(150.0).at_least(60.0).clip(true))
                .column(Column::initial(150.0).at_least(60.0).clip(true))
                .column(Column::initial(140.0).at_least(60.0).clip(true))
                .column(Column::initial(80.0).at_least(40.0))
                .column(Column::initial(80.0).at_least(40.0))
                .column(Column::initial(80.0).at_least(40.0))
                .column(Column::initial(80.0).at_least(40.0))
                .column(Column::initial(60.0).at_least(40.0))
                .column(Column::remainder().at_least(50.0))
                .header(22.0, |mut header| {
                    for title in [
                        "", "Name", "Address", "Resolved IP", "Last", "Avg", "Min", "Max", "Sent",
                        "Loss",
                    ] {
                        header.col(|ui| {
                            ui.strong(title);
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, hosts.len(), |mut row| {
                        let h = &hosts[row.index()];
                        let s = &h.stats;
                        let (color, tip) = if !h.enabled || paused {
                            (Color32::GRAY, "Paused")
                        } else {
                            match s.status {
                                Status::Unknown => (Color32::GRAY, "Waiting…"),
                                Status::Up => (GREEN, "Up"),
                                Status::Down => (RED, "Down"),
                            }
                        };
                        row.col(|ui| {
                            ui.label(RichText::new("●").color(color).size(16.0))
                                .on_hover_text(tip);
                        });
                        row.col(|ui| {
                            let mut name = RichText::new(&h.name);
                            if s.status == Status::Down {
                                name = name.color(RED).strong();
                            }
                            if ui
                                .selectable_label(self.selected == Some(h.id), name)
                                .clicked()
                            {
                                self.selected = Some(h.id);
                            }
                        });
                        row.col(|ui| {
                            ui.label(&h.address);
                        });
                        row.col(|ui| {
                            // Only meaningful when a DNS name was entered.
                            if h.address.parse::<IpAddr>().is_err() {
                                match s.resolved {
                                    Some(ip) => ui.label(ip.to_string()),
                                    None => ui.weak("—"),
                                };
                            }
                        });
                        row.col(|ui| {
                            ui.label(fmt_ms(s.last));
                        });
                        row.col(|ui| {
                            let avg = (s.recv > 0).then(|| s.sum / s.recv as f64);
                            ui.label(fmt_ms(avg));
                        });
                        row.col(|ui| {
                            ui.label(fmt_ms(s.min));
                        });
                        row.col(|ui| {
                            ui.label(fmt_ms(s.max));
                        });
                        row.col(|ui| {
                            ui.label(s.sent.to_string());
                        });
                        row.col(|ui| {
                            let loss = if s.sent > 0 {
                                100.0 * (s.sent - s.recv) as f64 / s.sent as f64
                            } else {
                                0.0
                            };
                            ui.label(format!("{loss:.1}%"));
                        });
                    });
                });
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1060.0, 700.0])
            .with_title("Ping Monitor"),
        ..Default::default()
    };
    eframe::run_native(
        "Ping Monitor",
        options,
        Box::new(|_cc| Ok(Box::new(PingMonApp::new()))),
    )
}