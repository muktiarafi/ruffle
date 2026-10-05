use crate::custom_event::{OpenType, RuffleEvent};
use crate::gui::{GuiController, MENU_HEIGHT};
use crate::player::{LaunchOptions, PlayerController};
use crate::preferences::GlobalPreferences;
use crate::util::{
    get_screen_size, gilrs_button_to_gamepad_button, plot_stats_in_tracy,
    winit_input_to_ruffle_key_descriptor, winit_to_ruffle_text_control,
};
use anyhow::Error;
use gilrs::{Event, EventType, Gilrs};
use ruffle_core::FloatDuration;
use ruffle_core::PlayerEvent;
use ruffle_core::events::{ImeEvent, ImeNotification, PlayerNotification};
use ruffle_core::swf::HeaderExt;
use ruffle_frontend_utils::content::ContentDescriptor;
use ruffle_render::backend::ViewportDimensions;
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize, Size};
use winit::event::{ElementState, Ime, Modifiers, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Icon, WindowAttributes, WindowId};

struct MainWindow {
    preferences: GlobalPreferences,
    gui: GuiController,
    player: PlayerController,
    minimized: bool,
    mouse_pos: PhysicalPosition<f64>,
    mouse_move_pending: bool,
    modifiers: Modifiers,
    min_window_size: LogicalSize<u32>,
    max_window_size: PhysicalSize<u32>,
    no_gui: bool,
    preferred_width: Option<f64>,
    preferred_height: Option<f64>,
    start_fullscreen: bool,
    loaded: LoadingState,
    time: Instant,
    next_frame_time: Option<Instant>,
    event_loop_proxy: EventLoopProxy<RuffleEvent>,
    /// AQW smooth motion: when the last in-between redraw was requested.
    aqw_last_smooth_redraw: Instant,
    /// AQW load spreading: async tasks (mostly finished downloads being turned
    /// into avatar parts) waiting for the next event-loop pass, and how much
    /// task time the current pass has used.
    aqw_task_queue: std::collections::VecDeque<crate::player::PlayerRunnable>,
    aqw_task_time: std::time::Duration,
    aqw_tasks_run: u32,
    aqw_pass_start: Instant,
    aqw_last_mem_log: Instant,
}

/// Per event-loop pass, run async tasks for at most this long before letting
/// a frame be drawn. One task can still exceed it; it just won't be followed
/// by more in the same pass.
const AQW_TASK_BUDGET: std::time::Duration = std::time::Duration::from_millis(6);

fn aqw_task_spreading_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        !std::env::var("RUFFLE_AQW_NO_TASK_SPREAD")
            .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false" | "off"))
    })
}

fn aqw_diagnostics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RUFFLE_AQW_DIAGNOSTICS")
            .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false" | "off"))
    })
}

/// Smooth-motion redraw rates F8 cycles through (after "off").
/// The game itself always keeps running at its own frame rate.
const AQW_SMOOTH_CHOICES: [f64; 4] = [30.0, 45.0, 60.0, ruffle_core::AQW_SMOOTH_MAX];

fn aqw_smooth_label(fps: f64, game_fps: f64) -> String {
    if fps >= ruffle_core::AQW_SMOOTH_MAX {
        format!("Smooth MAX (game {game_fps})")
    } else {
        format!("Smooth {fps} FPS (game {game_fps})")
    }
}

fn window_icon() -> Icon {
    let icon_bytes = crate::artix::window_icon_rgba();
    Icon::from_rgba(icon_bytes.to_vec(), 32, 32).expect("App icon should be correct")
}

impl MainWindow {
    /// Runs a task now if this pass still has task budget, otherwise queues it
    /// for the next pass so the frame in between can be drawn.
    fn aqw_run_or_queue_task(&mut self, task: crate::player::PlayerRunnable) {
        if !aqw_task_spreading_enabled() {
            self.player.poll(task);
            return;
        }
        if self.aqw_task_queue.is_empty() && self.aqw_task_time < AQW_TASK_BUDGET {
            self.aqw_run_task(task);
        } else {
            self.aqw_task_queue.push_back(task);
        }
    }

    fn aqw_run_task(&mut self, task: crate::player::PlayerRunnable) {
        let started = Instant::now();
        self.player.poll(task);
        self.aqw_task_time += started.elapsed();
        self.aqw_tasks_run += 1;
    }

    /// Start of an event-loop pass: fresh budget, then run queued tasks.
    fn aqw_begin_pass(&mut self) {
        self.aqw_pass_start = Instant::now();
        self.aqw_task_time = Default::default();
        self.aqw_tasks_run = 0;
        while self.aqw_task_time < AQW_TASK_BUDGET {
            let Some(task) = self.aqw_task_queue.pop_front() else {
                break;
            };
            self.aqw_run_task(task);
        }
        if !self.aqw_task_queue.is_empty() {
            // Make sure a frame gets drawn before the next batch.
            self.check_redraw();
        }
    }

    /// End of an event-loop pass: report long passes when diagnosing.
    fn aqw_end_pass(&mut self) {
        if !aqw_diagnostics_enabled() {
            return;
        }
        if self.aqw_last_mem_log.elapsed().as_secs() >= 2 {
            self.aqw_last_mem_log = Instant::now();
            if let Some(player) = self.player.get() {
                let (gc, swf) = player.aqw_memory_breakdown();
                let files = player.aqw_loaded_movie_count();
                drop(player);
                let (meshes, mesh_bytes) = ruffle_render_wgpu::aqw_mesh_stats();
                let mb = |b: u64| b / (1024 * 1024);
                tracing::info!(
                    target: "aqw_diag",
                    "AQW mem: ws_mb={} commit_mb={} gc_mb={} swf_mb={} mesh_mb={} meshes={meshes} files={files}",
                    ruffle_render_wgpu::backend::aqw_process_working_set_bytes().map_or(0, mb),
                    ruffle_render_wgpu::backend::aqw_process_memory_bytes().map_or(0, mb),
                    mb(gc as u64),
                    mb(swf as u64),
                    mb(mesh_bytes)
                );
            }
        }
        let pass = self.aqw_pass_start.elapsed();
        if pass.as_millis() >= 40 {
            tracing::info!(
                target: "aqw_diag",
                "AQW hitch: pass_ms={} task_ms={} tasks={} queued={}",
                pass.as_millis(),
                self.aqw_task_time.as_millis(),
                self.aqw_tasks_run,
                self.aqw_task_queue.len()
            );
        }
    }

    /// F8 = next smooth-motion setting, Shift+F8 = previous one.
    /// Cycle: off -> 30 -> 45 -> 60 -> off ...
    fn aqw_cycle_fps(&mut self, backwards: bool) {
        let Some(mut player) = self.player.get() else {
            return;
        };
        let count = AQW_SMOOTH_CHOICES.len() + 1;
        // Index 0 = off, 1.. = AQW_SMOOTH_CHOICES. Derive it from the player so
        // a value set via RUFFLE_AQW_SMOOTH at launch is picked up.
        let current = match player.aqw_smooth_fps() {
            None => 0,
            Some(fps) => AQW_SMOOTH_CHOICES
                .iter()
                .position(|choice| (choice - fps).abs() < 0.5)
                .map_or(count - 1, |i| i + 1),
        };
        let next = if backwards {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        };
        let smooth = (next > 0).then(|| AQW_SMOOTH_CHOICES[next - 1]);
        player.aqw_set_smooth_fps(smooth);
        let game_fps = player.frame_rate();
        drop(player);
        let label = match smooth {
            Some(fps) => aqw_smooth_label(fps, game_fps),
            None => format!("{game_fps} FPS"),
        };
        tracing::info!("AQW smooth motion: {label}");
        self.next_frame_time = Some(Instant::now());
        self.gui.window().request_redraw();
    }

    /// F7: fast overlay effects on/off.
    fn aqw_toggle_fast_overlay(&mut self) {
        let enabled = !ruffle_render::backend::aqw_fast_overlay_enabled();
        ruffle_render::backend::set_aqw_fast_overlay(enabled);
        tracing::info!("AQW fast overlay: {enabled}");
        if let Some(mut player) = self.player.get() {
            player.set_needs_render();
        }
        self.gui.window().request_redraw();
    }

    pub fn window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        if matches!(event, WindowEvent::RedrawRequested) {
            // Don't render when minimized to avoid potential swap chain errors in `wgpu`.
            if !self.minimized {
                let mut player = self.player.get();
                let mut display_rate_smooth = false;
                if let Some(ref mut player) = player {
                    // AQW smooth motion, MAX mode: advance time right before
                    // drawing so the in-between position matches this exact
                    // refresh, instead of whenever the last timer fired.
                    if player.aqw_smooth_is_display_rate() {
                        display_rate_smooth = true;
                        let now = Instant::now();
                        let dt = FloatDuration::from_std(now.duration_since(self.time));
                        if dt.as_millis() > 0.0 {
                            self.time = now;
                            player.tick(dt);
                            self.next_frame_time = Some(now + player.time_til_next_frame());
                        }
                    }
                    // Even if the movie is paused, user interaction with debug tools can change the render output
                    player.render();
                }

                self.gui.render(player);
                plot_stats_in_tracy(&self.gui.descriptors().wgpu_instance);

                // Keep drawing every refresh while something is moving; the
                // swap chain's vsync paces this to the display (60/120 Hz).
                if display_rate_smooth
                    && self
                        .player
                        .get()
                        .is_some_and(|player| player.aqw_smooth_moving())
                {
                    self.gui.window().request_redraw();
                }
            }

            // Important that we return here, or we'll get a feedback loop with egui
            // (winit says redraw, egui hears redraw and says redraw, we hear redraw and tell winit to redraw...)
            return;
        }

        if self.gui.handle_event(&event) {
            // Event consumed by GUI.
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                // TODO: Change this when winit adds a `Window::minimized` or `WindowEvent::Minimize`.
                self.minimized = size.width == 0 && size.height == 0;

                if let Some(mut player) = self.player.get() {
                    let viewport_scale_factor = self.gui.window().scale_factor();
                    player.set_viewport_dimensions(ViewportDimensions {
                        width: size.width,
                        height: size.height.saturating_sub(self.gui.height_offset() as u32),
                        scale_factor: viewport_scale_factor,
                    });
                }
                self.gui.window().request_redraw();
                if matches!(self.loaded, LoadingState::WaitingForResize) {
                    self.loaded = LoadingState::Loaded;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.gui.is_context_menu_visible() {
                    return;
                }

                // This is needed because some platforms (like macOS)
                // will fire this event at the same time as WindowEvent::MouseInput,
                // which may cause inaccurate behavior in the movie
                if position == self.mouse_pos {
                    return;
                }

                self.mouse_pos = position;
                self.mouse_move_pending = true;
            }
            WindowEvent::DroppedFile(file) => {
                if let Some(content_descriptor) = ContentDescriptor::new_local(&file, None) {
                    self.gui.create_movie(
                        &mut self.player,
                        LaunchOptions::from(&self.preferences),
                        content_descriptor,
                    );
                }
            }
            WindowEvent::Focused(true) => {
                self.player.handle_event(PlayerEvent::FocusGained);
            }
            WindowEvent::Focused(false) => {
                self.player.handle_event(PlayerEvent::FocusLost);
            }
            WindowEvent::MouseInput { button, state, .. } => {
                if self.gui.is_context_menu_visible() {
                    return;
                }

                self.flush_mouse_move();

                use ruffle_core::events::MouseButton as RuffleMouseButton;
                use winit::event::MouseButton;
                let (x, y) = self.gui.window_to_movie_position(self.mouse_pos);
                let button = match button {
                    MouseButton::Left => RuffleMouseButton::Left,
                    MouseButton::Right => RuffleMouseButton::Right,
                    MouseButton::Middle => RuffleMouseButton::Middle,
                    _ => RuffleMouseButton::Unknown,
                };
                let event = match state {
                    // TODO We should get information about click index from the OS,
                    //   but winit does not support that yet.
                    ElementState::Pressed => PlayerEvent::MouseDown {
                        x,
                        y,
                        button,
                        index: None,
                    },
                    ElementState::Released => PlayerEvent::MouseUp { x, y, button },
                };
                let handled = self.player.handle_event(event);
                if !handled && state == ElementState::Pressed && button == RuffleMouseButton::Right
                {
                    // Show context menu.
                    if let Some(mut player) = self.player.get() {
                        let context_menu = player.prepare_context_menu();

                        // MouseUp event will be ignored when the context menu is shown,
                        // but it has to be dispatched when the menu closes.
                        let close_event = PlayerEvent::MouseUp {
                            x,
                            y,
                            button: RuffleMouseButton::Right,
                        };
                        self.gui.show_context_menu(context_menu, close_event);
                    }
                }
                self.check_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.gui.is_context_menu_visible() {
                    return;
                }

                use ruffle_core::events::MouseWheelDelta;
                use winit::event::MouseScrollDelta;
                let delta = match delta {
                    MouseScrollDelta::LineDelta(_, dy) => MouseWheelDelta::Lines(dy.into()),
                    MouseScrollDelta::PixelDelta(pos) => MouseWheelDelta::Pixels(pos.y),
                };
                let event = PlayerEvent::MouseWheel { delta };
                self.player.handle_event(event);
                self.check_redraw();
            }
            WindowEvent::CursorEntered { .. } => {
                if let Some(mut player) = self.player.get() {
                    player.set_mouse_in_stage(true);
                    if player.needs_render() {
                        self.gui.window().request_redraw();
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.mouse_move_pending = false;
                if let Some(mut player) = self.player.get() {
                    player.set_mouse_in_stage(false);
                }
                self.player.handle_event(PlayerEvent::MouseLeave);
                self.check_redraw();
            }
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.gui.is_context_menu_visible() {
                    return;
                }

                let key = winit_input_to_ruffle_key_descriptor(&event);
                match event.state {
                    ElementState::Pressed => {
                        if event.physical_key == PhysicalKey::Code(KeyCode::F9) {
                            ruffle_core::aqw_crt_toggle_external();
                        }
                        if event.physical_key == PhysicalKey::Code(KeyCode::F6) && !event.repeat {
                            self.gui.toggle_aqw_memory_panel();
                        }
                        if event.physical_key == PhysicalKey::Code(KeyCode::F7) && !event.repeat {
                            self.aqw_toggle_fast_overlay();
                        }
                        if event.physical_key == PhysicalKey::Code(KeyCode::F8) && !event.repeat {
                            self.aqw_cycle_fps(self.modifiers.state().shift_key());
                        }
                        self.player.handle_event(PlayerEvent::KeyDown { key });
                        if let Some(control_code) =
                            winit_to_ruffle_text_control(&event, self.modifiers)
                        {
                            self.player
                                .handle_event(PlayerEvent::TextControl { code: control_code });
                        } else if let Some(text) = event.text {
                            for codepoint in text.chars() {
                                self.player
                                    .handle_event(PlayerEvent::TextInput { codepoint });
                            }
                        }
                    }
                    ElementState::Released => {
                        self.player.handle_event(PlayerEvent::KeyUp { key });
                    }
                };
                self.check_redraw();
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Enabled => {}
                Ime::Preedit(text, cursor) => {
                    self.player
                        .handle_event(PlayerEvent::Ime(ImeEvent::Preedit(text, cursor)));
                }
                Ime::Commit(text) => {
                    self.player
                        .handle_event(PlayerEvent::Ime(ImeEvent::Commit(text)));
                }
                Ime::Disabled => {}
            },
            _ => (),
        }
    }

    fn on_metadata(&mut self, swf_header: HeaderExt) {
        let height_offset = if self.gui.window().fullscreen().is_some() || self.no_gui {
            0.0
        } else {
            MENU_HEIGHT as f64
        };

        // To prevent issues like waiting on resize indefinitely (#11364) or desyncing the window state on Windows,
        // do not resize while window is maximized.
        let should_resize = !self.gui.window().is_maximized();

        let (viewport_size, state) = if should_resize {
            let movie_width = swf_header.stage_size().width().to_pixels();
            let movie_height = swf_header.stage_size().height().to_pixels();

            let window_size: Size = match (self.preferred_width, self.preferred_height) {
                (None, None) => LogicalSize::new(movie_width, movie_height + height_offset).into(),
                (Some(width), None) => {
                    let scale = width / movie_width;
                    let height = movie_height * scale;
                    PhysicalSize::new(
                        width.max(1.0),
                        height.max(1.0) + height_offset * self.gui.window().scale_factor(),
                    )
                    .into()
                }
                (None, Some(height)) => {
                    let scale = height / movie_height;
                    let width = movie_width * scale;
                    PhysicalSize::new(
                        width.max(1.0),
                        height.max(1.0) + height_offset * self.gui.window().scale_factor(),
                    )
                    .into()
                }
                (Some(width), Some(height)) => PhysicalSize::new(
                    width.max(1.0),
                    height.max(1.0) + height_offset * self.gui.window().scale_factor(),
                )
                .into(),
            };

            let window_size = Size::clamp(
                window_size,
                self.min_window_size.into(),
                self.max_window_size.into(),
                self.gui.window().scale_factor(),
            );

            let viewport_size = self.gui.window().inner_size();
            let mut window_resize_denied = false;

            if let Some(new_viewport_size) = self.gui.window().request_inner_size(window_size) {
                if new_viewport_size != viewport_size {
                    self.gui.resize(new_viewport_size);
                } else {
                    tracing::warn!("Unable to resize window");
                    window_resize_denied = true;
                }
            }

            let viewport_size = self.gui.window().inner_size();

            // On X11 (and possibly other platforms), the window size is not updated immediately.
            // On a successful resize request, wait for the window to be resized to the requested size
            // before we start running the SWF (which can observe the viewport size in "noScale" mode)
            let state = if !window_resize_denied && window_size != viewport_size.into() {
                LoadingState::WaitingForResize
            } else {
                LoadingState::Loaded
            };

            (viewport_size, state)
        } else {
            (self.gui.window().inner_size(), LoadingState::Loaded)
        };

        self.loaded = state;

        self.gui.window().set_fullscreen(if self.start_fullscreen {
            Some(Fullscreen::Borderless(None))
        } else {
            None
        });
        self.gui.window().set_visible(true);

        let viewport_scale_factor = self.gui.window().scale_factor();
        if let Some(mut player) = self.player.get() {
            player.set_viewport_dimensions(ViewportDimensions {
                width: viewport_size.width,
                height: viewport_size.height - (height_offset * viewport_scale_factor) as u32,
                scale_factor: viewport_scale_factor,
            });
        }
    }

    fn about_to_wait(&mut self, gilrs: Option<&mut Gilrs>) {
        if let Some(Event { event, .. }) = gilrs.and_then(|gilrs| gilrs.next_event()) {
            match event {
                EventType::ButtonPressed(button, _) => {
                    if let Some(button) = gilrs_button_to_gamepad_button(button) {
                        self.player
                            .handle_event(PlayerEvent::GamepadButtonDown { button });
                        self.check_redraw();
                    }
                }
                EventType::ButtonReleased(button, _) => {
                    if let Some(button) = gilrs_button_to_gamepad_button(button) {
                        self.player
                            .handle_event(PlayerEvent::GamepadButtonUp { button });
                        self.check_redraw();
                    }
                }
                _ => {}
            }
        }

        self.flush_mouse_move();

        // Core loop
        // [NA] This used to be called `MainEventsCleared`, but I think the behaviour is different now.
        // We should look at changing our tick to happen somewhere else if we see any behavioural problems.
        if matches!(self.loaded, LoadingState::Loaded) {
            let new_time = Instant::now();
            let dt = FloatDuration::from_std(new_time.duration_since(self.time));
            if dt.as_millis() > 0.0 {
                self.time = new_time;
                let mut smooth_interval = None;
                self.next_frame_time = self.player.get().map(|mut player| {
                    player.tick(dt);
                    let mut wait = player.time_til_next_frame();
                    // AQW smooth motion: wake up for in-between redraws too
                    // (not while minimized: nothing is shown, so the game only
                    // needs to wake for its own frames).
                    smooth_interval = if self.minimized {
                        None
                    } else {
                        player.aqw_time_til_next_smooth_redraw()
                    };
                    if let Some(interval) = smooth_interval {
                        wait = wait.min(interval);
                    }
                    new_time + wait
                });
                if !self.minimized
                    && self.player.get().is_some_and(|player| {
                        player.aqw_smooth_is_display_rate() && player.aqw_smooth_moving()
                    })
                {
                    self.gui.window().request_redraw();
                }
                if let Some(interval) = smooth_interval
                    && new_time.duration_since(self.aqw_last_smooth_redraw) + interval / 8
                        >= interval
                {
                    self.aqw_last_smooth_redraw = new_time;
                    self.gui.window().request_redraw();
                }
                self.check_redraw();
            }
        }
    }

    fn flush_mouse_move(&mut self) {
        if !self.mouse_move_pending {
            return;
        }
        self.mouse_move_pending = false;
        let (x, y) = self.gui.window_to_movie_position(self.mouse_pos);
        self.player.handle_event(PlayerEvent::MouseMove { x, y });
        self.check_redraw();
    }

    fn check_redraw(&self) {
        // Nothing is drawn while minimized, so a redraw request would never be
        // satisfied and would just be re-issued every loop, keeping the CPU busy.
        if self.minimized {
            return;
        }
        let player = self.player.get();
        if player.map(|p| p.needs_render()).unwrap_or_default() || self.gui.needs_render() {
            self.gui.window().request_redraw();
        }
    }
}

pub struct App {
    main_window: Option<MainWindow>,
    runtime: Option<tokio::runtime::Runtime>,
    gilrs: Option<Gilrs>,
    event_loop_proxy: EventLoopProxy<RuffleEvent>,
    preferences: GlobalPreferences,
    font_database: fontdb::Database,
}

/// Enters the tokio runtime context.
/// This cannot be a method, as the borrow-checker would complain.
macro_rules! enter_runtime {
    ($this:expr) => {
        let _guard = $this.runtime.as_ref().map(|runtime| runtime.enter());
    };
}

impl App {
    pub fn new(preferences: GlobalPreferences) -> Result<(Self, EventLoop<RuffleEvent>), Error> {
        let event_loop = EventLoop::with_user_event().build()?;

        let mut font_database = fontdb::Database::default();
        font_database.load_system_fonts();

        let gilrs = Gilrs::new()
            .inspect_err(|err| {
                tracing::warn!("Gamepad support could not be initialized: {err}");
            })
            .ok();
        let event_loop_proxy = event_loop.create_proxy();
        let runtime = tokio::runtime::Runtime::new()?;

        Ok((
            Self {
                main_window: None,
                runtime: Some(runtime),
                gilrs,
                event_loop_proxy,
                font_database,
                preferences,
            },
            event_loop,
        ))
    }
}

impl ApplicationHandler<RuffleEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        enter_runtime!(self);

        if let Some(main_window) = &mut self.main_window {
            main_window.aqw_begin_pass();
        }

        if cause == StartCause::Init {
            let movie_url = self.preferences.cli.movie_url.clone();
            let icon = window_icon();

            let no_gui = self.preferences.cli.no_gui;
            let min_window_size = if no_gui {
                (16, 16)
            } else {
                (350, MENU_HEIGHT + 16)
            }
            .into();
            let preferred_width = self.preferences.cli.width;
            let preferred_height = self.preferences.cli.height;
            let start_fullscreen = self.preferences.cli.fullscreen;

            let window_title = crate::artix::window_title();
            #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
            let mut window_attributes = WindowAttributes::default()
                .with_visible(false)
                .with_title(window_title)
                .with_window_icon(Some(icon))
                .with_min_inner_size(min_window_size);

            #[cfg(target_os = "linux")]
            {
                use winit::platform::startup_notify::{
                    self, EventLoopExtStartupNotify, WindowAttributesExtStartupNotify,
                };
                use winit::platform::wayland::WindowAttributesExtWayland;
                window_attributes = window_attributes.with_name("rs.ruffle.Ruffle", "main");
                if let Some(token) = event_loop.read_token_from_env() {
                    startup_notify::reset_activation_token_env();
                    window_attributes = window_attributes.with_activation_token(token);
                }
            }

            let event_loop_proxy = self.event_loop_proxy.clone();
            let preferences = self.preferences.clone();
            let window = event_loop
                .create_window(window_attributes)
                .expect("Window should be created");
            let max_window_size = get_screen_size(&window);
            window.set_max_inner_size(Some(max_window_size));
            let window = Arc::new(window);
            let font_database = self.font_database.clone();

            let mut gui = GuiController::new(
                window.clone(),
                event_loop_proxy.clone(),
                preferences.clone(),
                &font_database,
                movie_url.clone(),
                no_gui,
            )
            .expect("GUI controller should be created");
            gui.init_custom_cursors(event_loop);

            let mut player = PlayerController::new(
                event_loop_proxy.clone(),
                window.clone(),
                gui.descriptors().clone(),
                font_database,
                preferences.clone(),
                gui.file_picker(),
            );

            if let Some(movie_url) = &movie_url {
                gui.create_movie(
                    &mut player,
                    LaunchOptions::from(&preferences),
                    ContentDescriptor {
                        url: movie_url.clone(),
                        root_content_path: None,
                    },
                );
            } else {
                gui.show_open_dialog();
            }

            let mut loaded = LoadingState::Loading;

            if movie_url.is_none() {
                // No SWF provided on command line; show window with dummy movie immediately.
                window.set_visible(true);
                loaded = LoadingState::Loaded;
            }

            self.main_window = Some(MainWindow {
                preferences,
                gui,
                player,
                min_window_size,
                max_window_size,
                no_gui,
                preferred_width,
                preferred_height,
                start_fullscreen,
                loaded,
                minimized: false,
                mouse_pos: PhysicalPosition::new(0.0, 0.0),
                mouse_move_pending: false,
                modifiers: Modifiers::default(),
                time: Instant::now(),
                next_frame_time: None,
                event_loop_proxy,
                aqw_last_smooth_redraw: Instant::now(),
                aqw_task_queue: Default::default(),
                aqw_task_time: Default::default(),
                aqw_tasks_run: 0,
                aqw_pass_start: Instant::now(),
                aqw_last_mem_log: Instant::now(),
            });
        }
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RuffleEvent) {
        enter_runtime!(self);

        match (&mut self.main_window, event) {
            (Some(main_window), RuffleEvent::TaskPoll(task)) => {
                main_window.aqw_run_or_queue_task(task)
            }

            (Some(main_window), RuffleEvent::OnMetadata(swf_header)) => {
                main_window.on_metadata(swf_header)
            }

            (Some(main_window), RuffleEvent::ContextMenuItemClicked(index)) => {
                if let Some(mut player) = main_window.player.get() {
                    player.run_context_menu_callback(index);
                }
            }

            (Some(main_window), RuffleEvent::BrowseAndOpen(options, open_type)) => {
                let event_loop = main_window.event_loop_proxy.clone();
                let picker = main_window.gui.file_picker();
                tokio::spawn(async move {
                    let picked = match open_type {
                        OpenType::File => {
                            picker.pick_ruffle_file(None).await.map(|file| (file, None))
                        }
                        OpenType::Directory => picker
                            .pick_ruffle_directory_and_content(None)
                            .await
                            .map(|(dir, file)| (file, Some(dir))),
                    };

                    if let Some(desc) =
                        picked.and_then(|(file, dir)| ContentDescriptor::new_local(&file, dir))
                    {
                        let _ = event_loop.send_event(RuffleEvent::Open(desc, options));
                    }
                });
            }

            (Some(main_window), RuffleEvent::Open(desc, options)) => {
                main_window
                    .gui
                    .create_movie(&mut main_window.player, *options, desc);
            }

            (Some(main_window), RuffleEvent::OpenDialog(descriptor)) => {
                main_window.gui.open_dialog(descriptor);
            }

            (Some(main_window), RuffleEvent::CloseFile) => {
                main_window.gui.window().set_title("Ruffle"); // Reset title since file has been closed.
                main_window.gui.close_movie(&mut main_window.player);
            }

            (Some(main_window), RuffleEvent::ExportBundle) => {
                main_window.gui.export_bundle();
            }

            (Some(main_window), RuffleEvent::EnterFullScreen) => {
                if let Some(mut player) = main_window.player.get()
                    && player.is_playing()
                {
                    player.set_fullscreen(true);
                }
            }

            (Some(main_window), RuffleEvent::ExitFullScreen) => {
                if let Some(mut player) = main_window.player.get()
                    && player.is_playing()
                {
                    player.set_fullscreen(false);
                }
            }

            (Some(main_window), RuffleEvent::PlayerNotification(notification)) => {
                match notification {
                    PlayerNotification::ImeNotification(ImeNotification::ImeReady {
                        purpose,
                        cursor_area,
                    }) => {
                        let ime_enabled = main_window.preferences.ime_enabled().unwrap_or(false);
                        main_window.gui.set_ime_allowed(ime_enabled);
                        main_window.gui.set_ime_purpose(purpose);
                        main_window.gui.set_ime_cursor_area(cursor_area);
                    }
                    PlayerNotification::ImeNotification(ImeNotification::ImePurposeUpdated(
                        purpose,
                    )) => {
                        main_window.gui.set_ime_purpose(purpose);
                    }
                    PlayerNotification::ImeNotification(ImeNotification::ImeCursorAreaUpdated(
                        cursor_area,
                    )) => {
                        main_window.gui.set_ime_cursor_area(cursor_area);
                    }
                    PlayerNotification::ImeNotification(ImeNotification::ImeNotReady) => {
                        main_window.gui.set_ime_allowed(false);
                    }
                }
            }

            (_, RuffleEvent::ExitRequested) => {
                event_loop.exit();
            }

            _ => {}
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        enter_runtime!(self);

        if let Some(main_window) = &mut self.main_window {
            main_window.window_event(event_loop, event);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        enter_runtime!(self);

        if let Some(main_window) = &mut self.main_window {
            main_window.about_to_wait(self.gilrs.as_mut());

            // The event loop is finished; let's find out how long we need to wait for.
            // We don't need to worry about earlier update requests, as it's the
            // only place where we're setting control flow, and events cancel wait.
            // Note: the control flow might be set to `ControlFlow::WaitUntil` with a
            // timestamp in the past! Take that into consideration when changing this code.
            if let Some(next_frame_time) = main_window.next_frame_time {
                event_loop.set_control_flow(ControlFlow::WaitUntil(next_frame_time));
            }
            // AQW load spreading: come straight back for queued tasks.
            if !main_window.aqw_task_queue.is_empty() {
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now()));
            }
            main_window.aqw_end_pass();
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // `MainWindow` needs a tokio context to properly drop.
        {
            enter_runtime!(self);
            let _ = self.main_window.take();
        }

        // Manually stop the tokio runtime: this makes sure that any pending Player-bound futures
        // are properly cancelled and put back on the winit event loop before it closes, preventing
        // them from being dropped on the wrong thread and causing a panic.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        }
    }
}

enum LoadingState {
    Loading,
    WaitingForResize,
    Loaded,
}
