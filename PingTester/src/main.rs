// Hide the console window in release builds on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

use eframe::egui;
use std::net::IpAddr;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const PINGS_PER_CYCLE: u32 = 3;
const CYCLE_SECONDS: u64 = 5;
const PING_TIMEOUT_MS: u32 = 1000;
const MAX_LINES: usize = 5000;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------------------
// Shared session state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    sent: u32,
    lost: u32,
    replies: u32,
    total_ms: u64,
    min_ms: Option<u32>,
    max_ms: Option<u32>,
}

impl Stats {
    fn avg_ms(&self) -> Option<f64> {
        (self.replies > 0).then(|| self.total_ms as f64 / self.replies as f64)
    }

    fn loss_pct(&self) -> f64 {
        if self.sent == 0 {
            0.0
        } else {
            self.lost as f64 * 100.0 / self.sent as f64
        }
    }
}

#[derive(Default)]
struct Shared {
    lines: Vec<String>,
    stats: Stats,
}

// ---------------------------------------------------------------------------
// Ping logic
// ---------------------------------------------------------------------------

enum Outcome {
    Reply { ms: u32, raw: String },
    Lost { raw: String },
}

/// Extracts the latency from a Windows ping reply, e.g. "time=12ms" or "time<1ms".
fn parse_latency(output: &str) -> Option<u32> {
    let idx = output.find("time")?;
    let rest = &output[idx + 4..];
    let mut chars = rest.chars();
    match chars.next()? {
        '=' => {
            let digits: String = chars.take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        '<' => Some(1), // "time<1ms"
        _ => None,
    }
}

fn ping_once(ip: IpAddr) -> Outcome {
    let mut cmd = Command::new("ping");
    cmd.args(["-n", "1", "-w", &PING_TIMEOUT_MS.to_string(), &ip.to_string()]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    match cmd.output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            // A genuine echo reply includes a TTL. "Destination host unreachable"
            // lines do not, so they correctly count as lost.
            if text.contains("TTL=") {
                if let Some(ms) = parse_latency(&text) {
                    let raw = text
                        .lines()
                        .find(|l| l.contains("TTL="))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    return Outcome::Reply { ms, raw };
                }
            }
            let raw = text
                .lines()
                .map(str::trim)
                .find(|l| l.starts_with("Request timed out") || l.starts_with("Reply from") || l.starts_with("General failure") || l.starts_with("Destination"))
                .unwrap_or("No reply")
                .to_string();
            Outcome::Lost { raw }
        }
        Err(e) => Outcome::Lost {
            raw: format!("Failed to run ping: {e}"),
        },
    }
}

fn run_session(ip: IpAddr, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, ctx: egui::Context) {
    let session_start = Instant::now();

    let push = |line: String| {
        let mut s = shared.lock().unwrap();
        s.lines.push(line);
        if s.lines.len() > MAX_LINES {
            let excess = s.lines.len() - MAX_LINES;
            s.lines.drain(0..excess);
        }
    };

    push(format!("Pinging {ip} ({PINGS_PER_CYCLE}x every {CYCLE_SECONDS}s)..."));
    ctx.request_repaint();

    while !stop.load(Ordering::Relaxed) {
        let cycle_start = Instant::now();

        for _ in 0..PINGS_PER_CYCLE {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let outcome = ping_once(ip);
            let t = session_start.elapsed().as_secs();
            let stamp = format!("[{:02}:{:02}:{:02}]", t / 3600, (t / 60) % 60, t % 60);

            {
                let mut s = shared.lock().unwrap();
                s.stats.sent += 1;
                let line = match outcome {
                    Outcome::Reply { ms, raw } => {
                        s.stats.replies += 1;
                        s.stats.total_ms += ms as u64;
                        s.stats.min_ms = Some(s.stats.min_ms.map_or(ms, |m| m.min(ms)));
                        s.stats.max_ms = Some(s.stats.max_ms.map_or(ms, |m| m.max(ms)));
                        format!("{stamp} {raw}  | latency: {ms} ms")
                    }
                    Outcome::Lost { raw } => {
                        s.stats.lost += 1;
                        format!("{stamp} PACKET LOSS: {raw}")
                    }
                };
                s.lines.push(line);
                if s.lines.len() > MAX_LINES {
                    let excess = s.lines.len() - MAX_LINES;
                    s.lines.drain(0..excess);
                }
            }
            ctx.request_repaint();
        }

        // Wait out the remainder of the 5 second cycle, staying responsive to Stop.
        while cycle_start.elapsed() < Duration::from_secs(CYCLE_SECONDS) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

struct PingTester {
    ip_input: String,
    error: Option<String>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    running: bool,
}

impl Default for PingTester {
    fn default() -> Self {
        Self {
            ip_input: "8.8.8.8".to_string(),
            error: None,
            shared: Arc::new(Mutex::new(Shared::default())),
            stop: Arc::new(AtomicBool::new(true)),
            running: false,
        }
    }
}

impl PingTester {
    fn start(&mut self, ctx: &egui::Context) {
        // Parsing as IpAddr also guarantees nothing odd reaches the command line.
        match self.ip_input.trim().parse::<IpAddr>() {
            Ok(ip) => {
                self.error = None;
                self.stop.store(true, Ordering::Relaxed); // retire any old worker
                self.shared = Arc::new(Mutex::new(Shared::default())); // fresh session
                self.stop = Arc::new(AtomicBool::new(false));
                self.running = true;

                let shared = Arc::clone(&self.shared);
                let stop = Arc::clone(&self.stop);
                let ctx = ctx.clone();
                thread::spawn(move || run_session(ip, shared, stop, ctx));
            }
            Err(_) => self.error = Some("Please enter a valid IPv4 or IPv6 address.".into()),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.running = false;
    }
}

impl eframe::App for PingTester {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("IP address:");
                let edit = ui.add_enabled(
                    !self.running,
                    egui::TextEdit::singleline(&mut self.ip_input).desired_width(180.0),
                );
                let enter_pressed = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if self.running {
                    if ui.button("Stop").clicked() {
                        self.stop();
                    }
                } else if ui.button("Start").clicked() || enter_pressed {
                    self.start(ctx);
                }
                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
            });
            ui.add_space(6.0);
        });

        // Snapshot shared data so the lock is not held while drawing.
        let (lines, sent, lost, replies, avg, min, max, loss_pct) = {
            let s = self.shared.lock().unwrap();
            (
                s.lines.clone(),
                s.stats.sent,
                s.stats.lost,
                s.stats.replies,
                s.stats.avg_ms(),
                s.stats.min_ms,
                s.stats.max_ms,
                s.stats.loss_pct(),
            )
        };

        egui::SidePanel::right("stats")
            .resizable(false)
            .exact_width(220.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Session");
                ui.separator();

                let loss_color = if lost > 0 {
                    egui::Color32::LIGHT_RED
                } else {
                    egui::Color32::LIGHT_GREEN
                };
                ui.label("Packet loss incidents");
                ui.label(egui::RichText::new(lost.to_string()).size(28.0).color(loss_color));
                ui.label(format!("{loss_pct:.1}% of {sent} sent"));

                ui.add_space(14.0);
                ui.label("Average ping");
                let avg_text = avg.map_or("--".to_string(), |a| format!("{a:.1} ms"));
                ui.label(egui::RichText::new(avg_text).size(28.0));

                ui.add_space(14.0);
                ui.label(format!("Replies: {replies}"));
                ui.label(format!("Min: {}", min.map_or("--".into(), |m| format!("{m} ms"))));
                ui.label(format!("Max: {}", max.map_or("--".into(), |m| format!("{m} ms"))));
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &lines {
                        let text = egui::RichText::new(line).monospace();
                        if line.contains("PACKET LOSS") {
                            ui.label(text.color(egui::Color32::LIGHT_RED));
                        } else {
                            ui.label(text);
                        }
                    }
                });
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 460.0])
            .with_min_inner_size([560.0, 300.0])
            .with_title("Ping Tester"),
        ..Default::default()
    };
    eframe::run_native(
        "Ping Tester",
        options,
        Box::new(|_cc| Ok(Box::new(PingTester::default()))),
    )
}
