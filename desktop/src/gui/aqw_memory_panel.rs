//! AQW control panel (F6): a small draggable window with the smooth-motion and
//! fast-effects settings, a live drawn-FPS counter, memory readings and a
//! "Clean memory" button.

use egui::{Align2, Color32, CornerRadius, Frame, Id, Margin, RichText, vec2};
use ruffle_core::Player;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const TREND_WINDOW: usize = 60; // samples (≈ one minute)
const REPORT_AFTER: Duration = Duration::from_millis(1500);
const MESSAGE_FOR: Duration = Duration::from_secs(8);
const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

struct PendingReport {
    at: Instant,
    ram_before: Option<u64>,
    gpu_before: Option<u64>,
    released_internal: u64,
}

pub struct AqwMemoryPanel {
    visible: bool,
    last_sample: Option<Instant>,
    /// Physical RAM in use (working set).
    ram: Option<u64>,
    /// Committed memory, including Windows' reservation backing GPU memory.
    commit: Option<u64>,
    gpu: Option<(u64, u64)>,
    assets: usize,
    peak_ram: u64,
    history: VecDeque<u64>,
    message: Option<(String, Instant)>,
    pending: Option<PendingReport>,
    /// Frames drawn since `fps_window_start`, and the last full second's count.
    frames_in_window: u32,
    fps_window_start: Instant,
    drawn_fps: u32,
}

/// Smooth-motion choices shown as buttons (None = off).
const SMOOTH_CHOICES: [(Option<f64>, &str); 5] = [
    (None, "Off"),
    (Some(30.0), "30"),
    (Some(45.0), "45"),
    (Some(60.0), "60"),
    (Some(ruffle_core::AQW_SMOOTH_MAX), "MAX"),
];

impl Default for AqwMemoryPanel {
    fn default() -> Self {
        Self {
            visible: std::env::var("RUFFLE_AQW_MEMORY_PANEL")
                .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false" | "off")),
            last_sample: None,
            ram: None,
            commit: None,
            gpu: None,
            assets: 0,
            peak_ram: 0,
            history: VecDeque::with_capacity(TREND_WINDOW + 1),
            message: None,
            pending: None,
            frames_in_window: 0,
            fps_window_start: Instant::now(),
            drawn_fps: 0,
        }
    }
}

fn fmt_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

fn fmt_delta(delta: i64) -> String {
    let sign = if delta < 0 { "-" } else { "+" };
    format!("{sign}{:.0} MB", delta.unsigned_abs() as f64 / MB)
}

impl AqwMemoryPanel {
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        self.last_sample = None;
    }

    fn sample(&mut self, now: Instant) {
        self.ram = ruffle_render_wgpu::backend::aqw_process_working_set_bytes();
        self.commit = ruffle_render_wgpu::backend::aqw_process_memory_bytes();
        self.gpu = ruffle_render_wgpu::backend::aqw_gpu_memory_bytes();
        self.assets = ruffle_frontend_utils::backends::navigator::aqw_asset_memory_bytes();
        if let Some(ram) = self.ram {
            self.peak_ram = self.peak_ram.max(ram);
            self.history.push_back(ram);
            while self.history.len() > TREND_WINDOW {
                self.history.pop_front();
            }
        }
        self.last_sample = Some(now);

        if let Some(pending) = &self.pending
            && now >= pending.at
        {
            let pending = self.pending.take().expect("checked above");
            let mut parts = Vec::new();
            if let (Some(before), Some(after)) = (pending.ram_before, self.ram) {
                parts.push(format!("RAM {}", fmt_delta(after as i64 - before as i64)));
            }
            if let (Some(before), Some((after, _))) = (pending.gpu_before, self.gpu) {
                parts.push(format!(
                    "graphics {}",
                    fmt_delta(after as i64 - before as i64)
                ));
            }
            let measured = if parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", parts.join(", "))
            };
            self.message = Some((
                format!(
                    "Released {} internally{measured}",
                    fmt_bytes(pending.released_internal)
                ),
                now,
            ));
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, mut player: Option<&mut Player>) {
        let now = Instant::now();
        // This runs once per drawn frame, so counting calls gives the real
        // on-screen frame rate.
        self.frames_in_window += 1;
        if now.duration_since(self.fps_window_start) >= Duration::from_secs(1) {
            self.drawn_fps = self.frames_in_window;
            self.frames_in_window = 0;
            self.fps_window_start = now;
        }
        if !self.visible {
            return;
        }
        if self
            .last_sample
            .is_none_or(|at| now.duration_since(at) >= SAMPLE_EVERY)
        {
            self.sample(now);
        }
        if self
            .message
            .as_ref()
            .is_some_and(|(_, at)| now.duration_since(*at) > MESSAGE_FOR)
        {
            self.message = None;
        }

        let text = |s: String| RichText::new(s).color(Color32::WHITE).size(13.0);
        let dim = |s: &str| RichText::new(s).color(Color32::GRAY).size(11.0);
        let heading = |s: &str| RichText::new(s).color(Color32::WHITE).strong();

        let current_smooth = player.as_deref().and_then(|p| p.aqw_smooth_fps());
        let game_fps = player.as_deref().map(|p| p.frame_rate());
        let loaded_files = player.as_deref().map(|p| p.aqw_loaded_movie_count());
        let breakdown = player.as_deref().map(|p| p.aqw_memory_breakdown());
        let (mesh_count, mesh_bytes) = ruffle_render_wgpu::aqw_mesh_stats();
        let mut new_smooth: Option<Option<f64>> = None;
        let mut fast_effects = ruffle_render::backend::aqw_fast_overlay_enabled();
        let mut fast_filters = ruffle_render::backend::aqw_fast_filters_enabled();
        let mut static_map = player
            .as_deref()
            .is_some_and(|p| p.aqw_static_map_enabled());
        let mut static_map_changed = false;
        let mut static_pets = player
            .as_deref()
            .is_some_and(|p| p.aqw_static_pets_enabled());
        let mut static_pets_changed = false;
        let mut flames = player
            .as_deref()
            .map_or(0, |p| p.aqw_loading_flames_mode());
        let mut flames_changed = false;
        let mut spread = player.as_deref().is_some_and(|p| p.aqw_spread_snapshots());
        let mut spread_changed = false;
        let mut filters_changed = false;
        let mut fast_changed = false;
        let mut clean_clicked = false;

        egui::Window::new(heading("AQW"))
            .id(Id::new("aqw_control_panel"))
            .default_pos(ctx.content_rect().right_top() + vec2(-250.0, 10.0))
            .pivot(Align2::LEFT_TOP)
            .resizable(false)
            .collapsible(true)
            .constrain(true)
            .frame(
                Frame::NONE
                    .fill(Color32::from_black_alpha(200))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::same(8)),
            )
            .show(ctx, |ui| {
                ui.set_max_width(240.0);

                // --- Smooth motion -------------------------------------
                ui.label(heading("Smooth motion (F8)"));
                ui.horizontal_wrapped(|ui| {
                    for (choice, label) in SMOOTH_CHOICES {
                        let selected = match (choice, current_smooth) {
                            (None, None) => true,
                            (Some(a), Some(b)) => (a - b).abs() < 0.5,
                            _ => false,
                        };
                        if ui.selectable_label(selected, label).clicked() && !selected {
                            new_smooth = Some(choice);
                        }
                    }
                });
                if let Some(game_fps) = game_fps {
                    ui.label(dim(&format!(
                        "Drawing {} FPS · game runs at {game_fps}",
                        self.drawn_fps
                    )));
                }

                ui.add_space(4.0);
                // --- Fast effects ---------------------------------------
                if ui
                    .checkbox(&mut fast_effects, text("Fast effects (F7)".to_string()))
                    .changed()
                {
                    fast_changed = true;
                }
                if ui
                    .checkbox(&mut fast_filters, text("Fast glow/blur filters".to_string()))
                    .on_hover_text("One blur pass instead of up to three. Big help in crowded rooms with animated players; glows look very slightly less smooth.")
                    .changed()
                {
                    filters_changed = true;
                }
                if ui
                    .checkbox(&mut static_map, text("Static map art".to_string()))
                    .on_hover_text("Freezes the map's looping decorations (flags, fire, water, lights) after they have played once. Doors, fades, cutscenes and room changes still play.")
                    .changed()
                {
                    static_map_changed = true;
                }
                if ui
                    .checkbox(&mut static_pets, text("Static pets".to_string()))
                    .on_hover_text("Pets hold a still pose a few seconds after each animation starts. They still follow their owner.")
                    .changed()
                {
                    static_pets_changed = true;
                }
                if ui
                    .checkbox(&mut spread, text("Spread player snapshots".to_string()))
                    .on_hover_text("With the game's Static Player Art on, players turn into pictures one per frame instead of all at once, so entering a busy room doesn't hitch. Players may stay animated a moment longer.")
                    .changed()
                {
                    spread_changed = true;
                }
                ui.horizontal(|ui| {
                    ui.label(text("Loading flames:".to_string()))
                        .on_hover_text("The flame shown on players whose gear is still loading when you enter a room. It is animated under two glow effects, so a room full of loading players costs FPS. Still = one frozen flame; Hidden = nothing until the player appears.");
                    for (mode, label) in [(0u8, "Normal"), (1, "Still"), (2, "Hidden")] {
                        if ui
                            .selectable_label(flames == mode, text(label.to_string()))
                            .clicked()
                            && flames != mode
                        {
                            flames = mode;
                            flames_changed = true;
                        }
                    }
                });

                ui.separator();
                // --- Memory ---------------------------------------------
                ui.label(heading("Memory (F6 hides this)"));
                match self.ram {
                    Some(ram) => {
                        ui.label(text(format!(
                            "RAM in use: {}  (peak {})",
                            fmt_bytes(ram),
                            fmt_bytes(self.peak_ram)
                        )));
                        if self.history.len() >= 10
                            && let Some(oldest) = self.history.front()
                        {
                            let delta = ram as i64 - *oldest as i64;
                            let secs = self.history.len();
                            let color = if delta > 50 * 1024 * 1024 {
                                Color32::from_rgb(255, 170, 90)
                            } else {
                                Color32::from_rgb(140, 220, 140)
                            };
                            ui.label(
                                RichText::new(format!("{} in the last {secs}s", fmt_delta(delta)))
                                    .color(color)
                                    .size(11.0),
                            );
                        }
                    }
                    None => {
                        ui.label(dim("RAM in use: not available"));
                    }
                }
                if let Some(commit) = self.commit {
                    ui.label(dim(&format!(
                        "Reserved: {} (includes Windows' backup for graphics memory, not all in RAM)",
                        fmt_bytes(commit)
                    )));
                }
                if let Some((used, budget)) = self.gpu {
                    ui.label(text(format!(
                        "Graphics: {} / {}",
                        fmt_bytes(used),
                        fmt_bytes(budget)
                    )));
                }
                ui.label(text(format!(
                    "Download cache: {}",
                    fmt_bytes(self.assets as u64)
                )));
                if let Some(count) = loaded_files {
                    ui.label(text(format!("Loaded game files: {count}")));
                }
                if let Some((gc, swf)) = breakdown {
                    ui.label(dim(&format!(
                        "  game objects {} · file data {} · shapes {} ({mesh_count})",
                        fmt_bytes(gc as u64),
                        fmt_bytes(swf as u64),
                        fmt_bytes(mesh_bytes)
                    )));
                    ui.label(dim("  (kept until you restart - Clean memory can't free these)"));
                }

                ui.add_space(4.0);
                let busy = self.pending.is_some();
                if ui
                    .add_enabled(!busy, egui::Button::new("Clean memory"))
                    .clicked()
                {
                    clean_clicked = true;
                }
                if busy {
                    ui.label(dim("Cleaning..."));
                } else if let Some((message, _)) = &self.message {
                    ui.label(
                        RichText::new(message)
                            .color(Color32::from_rgb(140, 220, 140))
                            .size(11.0),
                    );
                }
            });

        if let Some(smooth) = new_smooth
            && let Some(player) = player.as_deref_mut()
        {
            player.aqw_set_smooth_fps(smooth);
            tracing::info!("AQW smooth motion (panel): {smooth:?}");
        }
        if static_pets_changed && let Some(player) = player.as_deref_mut() {
            player.aqw_set_static_pets(static_pets);
            tracing::info!("AQW static pets (panel): {static_pets}");
        }
        if spread_changed && let Some(player) = player.as_deref_mut() {
            player.aqw_set_spread_snapshots(spread);
            tracing::info!("AQW spread snapshots (panel): {spread}");
        }
        if flames_changed && let Some(player) = player.as_deref_mut() {
            player.aqw_set_loading_flames_mode(flames);
            tracing::info!("AQW loading flames (panel): {flames}");
        }
        if static_map_changed && let Some(player) = player.as_deref_mut() {
            player.aqw_set_static_map(static_map);
            tracing::info!("AQW static map art (panel): {static_map}");
        }
        if filters_changed {
            ruffle_render::backend::set_aqw_fast_filters(fast_filters);
            tracing::info!("AQW fast filters (panel): {fast_filters}");
            if let Some(player) = player.as_deref_mut() {
                player.set_needs_render();
            }
        }
        if fast_changed {
            ruffle_render::backend::set_aqw_fast_overlay(fast_effects);
            tracing::info!("AQW fast overlay (panel): {fast_effects}");
            if let Some(player) = player.as_deref_mut() {
                player.set_needs_render();
            }
        }

        if clean_clicked {
            let ram_before = ruffle_render_wgpu::backend::aqw_process_working_set_bytes();
            let gpu_before =
                ruffle_render_wgpu::backend::aqw_gpu_memory_bytes().map(|(used, _)| used);
            let mut released = 0u64;
            if let Some(player) = player {
                let (caches, pools) = player.aqw_clean_memory();
                released += caches + pools;
            }
            released += ruffle_frontend_utils::backends::navigator::clear_aqw_asset_memory() as u64;
            self.pending = Some(PendingReport {
                at: now + REPORT_AFTER,
                ram_before,
                gpu_before,
                released_internal: released,
            });
            self.message = None;
            // Re-sample right after the report delay.
            self.last_sample = Some(now + REPORT_AFTER - SAMPLE_EVERY);
        }

        ctx.request_repaint_after(SAMPLE_EVERY);
    }
}
