//! Owned winit + egui + wgpu window host.
//!
//! Illness creates its own window so it can opt into
//! `WS_EX_NOREDIRECTIONBITMAP` and the DXGI/DirectComposition presentation
//! path, which a transparent native backdrop requires. Everything else — the
//! surface, textures, resizing and paint callbacks — is delegated to
//! [`egui_wgpu::winit::Painter`].

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{ViewportCommand, ViewportEvent, ViewportId, ViewportInfo};
use egui_wgpu::{RendererOptions, WgpuConfiguration};
#[cfg(target_os = "windows")]
use egui_wgpu::{SurfaceConfig, WgpuSetup, WgpuSetupCreateNew};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::app;
use illness_term::MouseButton;

const INITIAL_INNER_SIZE: [f32; 2] = [1180.0, 760.0];
const MIN_INNER_SIZE: [f32; 2] = [720.0, 480.0];

/// Wake-up messages from egui's repaint scheduler.
#[derive(Debug)]
enum UserEvent {
    Repaint { when: Instant },
}

pub fn run(
    icon: Option<Arc<egui::IconData>>,
    startup_folder: Option<PathBuf>,
) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let ctx = egui::Context::default();
    let proxy = event_loop.create_proxy();
    let wgpu_config = wgpu_configuration(event_loop.owned_display_handle());
    let painter = pollster::block_on(egui_wgpu::winit::Painter::new(
        ctx.clone(),
        wgpu_config,
        true,
        // Text is alpha-blended at 1:1 physical pixels; dithering adds noise
        // to glyph edges.
        RendererOptions {
            dithering: false,
            ..Default::default()
        },
    ));

    let mut host = Host::new(ctx, painter, proxy, icon, startup_folder);
    event_loop.run_app(&mut host)?;
    Ok(())
}

fn wgpu_configuration(display_handle: winit::event_loop::OwnedDisplayHandle) -> WgpuConfiguration {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = display_handle;
        WgpuConfiguration::default()
    }

    #[cfg(target_os = "windows")]
    {
        let mut setup = WgpuSetupCreateNew::from_display_handle(display_handle);
        setup.instance_descriptor.backends = wgpu::Backends::DX12;
        // A DXGI swapchain created from a DirectComposition visual is the only
        // DX12 presentation path that supports a transparent swapchain.
        setup
            .instance_descriptor
            .backend_options
            .dx12
            .presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;

        let mut config =
            WgpuConfiguration::default().with_surface_config(SurfaceConfig::LOW_LATENCY);
        config.wgpu_setup = WgpuSetup::CreateNew(setup);
        config
    }
}

/// Buttons winit currently reports as down, one bit per terminal button.
///
/// The pointer gate needs physical state, not egui's: `egui_winit` drops button
/// events while it has no pointer position, and egui keeps `any_down` latched
/// after `PointerGone`. Every press and release winit delivers updates this set,
/// so a drag that leaves the window and returns still reads as held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DownButtons(u8);

impl DownButtons {
    /// Apply one press or release. Buttons the terminal encoder never reports
    /// have no bit, so they cannot latch the gate.
    fn set(&mut self, button: MouseButton, pressed: bool) {
        let Some(bit) = button_bit(button) else {
            return;
        };
        if pressed {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    fn any(&self) -> bool {
        self.0 != 0
    }
}

fn button_bit(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0b001),
        MouseButton::Middle => Some(0b010),
        MouseButton::Right => Some(0b100),
        MouseButton::None | MouseButton::WheelUp | MouseButton::WheelDown => None,
    }
}

struct Host {
    ctx: egui::Context,
    painter: egui_wgpu::winit::Painter,
    proxy: EventLoopProxy<UserEvent>,
    icon: Option<Arc<egui::IconData>>,
    /// Directory passed on the command line; the first shell and new tabs
    /// start here.
    startup_folder: Option<PathBuf>,
    state: Option<egui_winit::State>,
    window: Option<Arc<Window>>,
    app: Option<app::IllnessApp>,
    info: ViewportInfo,
    start: Instant,
    /// Earliest delay requested through egui's repaint callback, used to tell
    /// an immediate repaint from one that is already scheduled.
    requested_repaint: Arc<Mutex<Option<Instant>>>,
    /// Earliest wake-up delivered through [`UserEvent::Repaint`].
    repaint_at: Option<Instant>,
    close_requested: bool,
    /// Set once the close path has saved and asked the loop to exit. Buffered
    /// winit events such as `RedrawRequested` must not run after this point.
    closing: bool,
    /// Physical size currently applied to the wgpu surface. Buffered `WM_SIZE`
    /// replays must not resize or repaint an unchanged surface.
    last_surface_size: Option<(u32, u32)>,
    /// Static surface capability: the swapchain exposes a transparent alpha
    /// mode. The live availability also depends on Windows transparency
    /// effects, which the host re-checks on focus/theme/occlusion changes.
    surface_alpha: bool,
    /// Set by a zero-sized (minimized) resize. The first non-zero restore must
    /// repaint even when Windows reports the same size as before minimizing.
    restore_pending: bool,
    first_frame: bool,
    /// Shared with the app: whether a close request would currently show the
    /// confirmation card. Kept in an atomic so a close arriving for a window
    /// that cannot paint still knows the app would guard it.
    close_guarded: Arc<AtomicBool>,
    /// Pixels per point used for the last painted pass. A move is converted
    /// with this rather than the live window scale, so a `ScaleFactorChanged`
    /// cannot misclassify a move against a rect recorded in older points.
    pointer_pass_ppp: Option<f32>,
    /// Ctrl state from the last painted pass, so a move needs no extra
    /// context-input lock. It is one pass old by design.
    pointer_pass_ctrl: bool,
    /// Shift state from the last painted pass, for the link-hover Shift
    /// override. One pass old, like `pointer_pass_ctrl`.
    pointer_pass_shift: bool,
    /// Physical pointer presence, straight from winit. egui's pointer position
    /// is only updated at `begin_pass`, so a release queued right after
    /// `CursorLeft` still looks inside to egui.
    pointer_inside: bool,
    /// Buttons winit currently reports down. egui can drop a transition while
    /// it has no pointer position, and its `any_down` latches after
    /// `PointerGone`, so the gate reads this physical set instead.
    buttons_down: DownButtons,
}

impl Host {
    fn new(
        ctx: egui::Context,
        painter: egui_wgpu::winit::Painter,
        proxy: EventLoopProxy<UserEvent>,
        icon: Option<Arc<egui::IconData>>,
        startup_folder: Option<PathBuf>,
    ) -> Self {
        Self {
            ctx,
            painter,
            proxy,
            icon,
            startup_folder,
            state: None,
            window: None,
            app: None,
            info: ViewportInfo::default(),
            start: Instant::now(),
            requested_repaint: Arc::new(Mutex::new(None)),
            repaint_at: None,
            close_requested: false,
            closing: false,
            last_surface_size: None,
            surface_alpha: false,
            restore_pending: false,
            first_frame: true,
            close_guarded: Arc::new(AtomicBool::new(false)),
            pointer_pass_ppp: None,
            pointer_pass_ctrl: false,
            pointer_pass_shift: false,
            pointer_inside: false,
            buttons_down: DownButtons::default(),
        }
    }

    fn viewport_builder(&self) -> egui::ViewportBuilder {
        let mut builder = egui::ViewportBuilder::default()
            .with_title("Illness")
            .with_inner_size(INITIAL_INNER_SIZE)
            .with_min_inner_size(MIN_INNER_SIZE)
            .with_decorations(false)
            .with_transparent(true)
            .with_has_shadow(false)
            .with_resizable(true)
            // Hidden until the first frame is painted, to avoid a white flash.
            .with_visible(false);
        if let Some(icon) = &self.icon {
            builder = builder.with_icon(Arc::clone(icon));
        }
        builder
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let builder = self.viewport_builder();
        let attributes = egui_winit::create_winit_window_attributes(&self.ctx, builder.clone());
        #[cfg(target_os = "windows")]
        let attributes = {
            use winit::platform::windows::WindowAttributesExtWindows as _;
            attributes.with_no_redirection_bitmap(true)
        };

        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                log::error!("failed to create window: {error}");
                crate::platform::show_startup_error(&format!(
                    "The application window could not be created.\n\n{error}"
                ));
                event_loop.exit();
                return;
            }
        };
        egui_winit::apply_viewport_builder_to_window(&self.ctx, &window, &builder);

        if let Err(error) = pollster::block_on(
            self.painter
                .set_window(ViewportId::ROOT, Some(window.clone())),
        ) {
            log::error!("failed to create the wgpu surface: {error}");
            crate::platform::show_startup_error(&format!(
                "The GPU surface could not be created.\n\n{error}"
            ));
            event_loop.exit();
            return;
        }
        let (surface_alpha, effects_enabled) = log_gpu(&window, &self.painter);
        self.surface_alpha = surface_alpha;
        let native_transparency = surface_alpha && effects_enabled;

        // `set_window` configured the surface at the window's current size.
        // Record it so a buffered `WM_SIZE` replay of that same size does not
        // resize or repaint again.
        let size = window.inner_size();
        self.last_surface_size = Some((size.width, size.height));

        let mut state = egui_winit::State::new(
            self.ctx.clone(),
            ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            event_loop.system_theme(),
            self.painter.max_texture_side(),
        );
        // Keep Ctrl+C/V/X as terminal input. The app handles Ctrl+Shift+C/V
        // itself; letting egui-winit intercept them would swallow Ctrl+C
        // (0x03) and Ctrl+V (0x16) before the session sees them.
        state.set_clipboard_shortcuts(false);

        egui_winit::update_viewport_info(&mut self.info, &self.ctx, &window, true);
        // `IllnessApp::new` reads `Context::pixels_per_point` for the initial
        // glyph atlas and PTY cell metrics. The viewport information has not
        // reached egui's input state yet, so seed it with the real scale first.
        // Read the scale before taking the input write lock: querying it from
        // inside `input_mut` would deadlock on the context lock.
        let pixels_per_point = egui_winit::pixels_per_point(&self.ctx, &window);
        self.ctx.input_mut(|input| {
            input
                .raw
                .viewports
                .insert(ViewportId::ROOT, self.info.clone());
            input.pixels_per_point = pixels_per_point;
        });
        let render_state = self.painter.render_state();
        let app = app::IllnessApp::new(
            &self.ctx,
            render_state.as_ref(),
            window.clone(),
            native_transparency,
            self.startup_folder.clone(),
            Arc::clone(&self.close_guarded),
        );

        self.state = Some(state);
        self.window = Some(window);
        self.app = Some(app);
        self.install_repaint_callback();

        // The window is hidden, so Windows does not deliver `RedrawRequested`.
        // Paint the first frame synchronously, then show the window.
        self.redraw(event_loop);
    }

    fn install_repaint_callback(&self) {
        let proxy = self.proxy.clone();
        let requested = Arc::clone(&self.requested_repaint);
        self.ctx.set_request_repaint_callback(move |info| {
            let when = Instant::now() + info.delay;
            if let Ok(mut deadline) = requested.lock() {
                let current = *deadline;
                *deadline = Some(current.map_or(when, |current| current.min(when)));
            }
            if proxy.send_event(UserEvent::Repaint { when }).is_err() {
                log::debug!("event loop closed; dropped repaint request");
            }
        });
    }

    /// Re-sample the live Windows transparency-effects setting while keeping
    /// the static surface-alpha capability. Theme, focus, activation and
    /// un-occlusion events are cheap and cover the user toggling Settings >
    /// Personalization > Colors without a restart.
    fn refresh_native_transparency(&mut self) {
        let available = self.surface_alpha && crate::platform::transparency_effects_enabled();
        let Some(app) = self.app.as_mut() else {
            return;
        };
        if app.set_native_transparency(available)
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if self.closing {
            log::debug!("redraw ignored after close");
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        // A buffered `RedrawRequested` can still arrive after minimize. A
        // paint then sees the 160x28 minimized client area and reflows the
        // terminal to a 1-row grid. The forced resize/redraw on restore paints
        // the restored size instead.
        let live_size = window.inner_size();
        if window.is_minimized().unwrap_or(false) || live_size.width == 0 || live_size.height == 0 {
            log::debug!(
                "redraw ignored while minimized or zero-sized ({}x{})",
                live_size.width,
                live_size.height
            );
            return;
        }
        // A paint at a size the surface already has proves the restore pass
        // happened. Do not let a stale flag force another identical resize.
        if self.restore_pending
            && self.last_surface_size == Some((live_size.width, live_size.height))
        {
            self.restore_pending = false;
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let Some(app) = self.app.as_mut() else {
            return;
        };

        egui_winit::update_viewport_info(&mut self.info, &self.ctx, &window, false);
        let mut input = state.take_egui_input(&window);
        input.time = Some(self.start.elapsed().as_secs_f64());
        input.viewports = egui::ViewportIdMap::default();
        input.viewports.insert(ViewportId::ROOT, self.info.clone());

        let mut output = self.ctx.run_ui(input, |ui| app.ui(ui));
        // Remember the scale this pass's rects were laid out in, and its Ctrl
        // state, so the next move is classified with matching values.
        self.pointer_pass_ppp = Some(output.pixels_per_point);
        self.pointer_pass_ctrl = self.ctx.input(|input| input.modifiers.ctrl);
        self.pointer_pass_shift = self.ctx.input(|input| input.modifiers.shift);

        let commands = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        let cancel_close = cancel_close_requested(&commands);
        let mut actions = Vec::new();
        egui_winit::process_viewport_commands(
            &self.ctx,
            &mut self.info,
            commands,
            &window,
            &mut actions,
        );
        if cancel_close {
            self.close_requested = false;
            self.info
                .events
                .retain(|event| *event != ViewportEvent::Close);
        }
        if self.info.close_requested() {
            self.close_requested = true;
        }
        // Events are input for one pass only; egui-winit does not clear them.
        self.info.events.clear();

        state.handle_platform_output(&window, output.platform_output);
        if handle_actions(state, actions) {
            window.request_redraw();
        }

        let clipped_primitives = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        self.painter.paint_and_update_textures(
            ViewportId::ROOT,
            output.pixels_per_point,
            app.clear_color(),
            &clipped_primitives,
            &mut output.textures_delta,
            vec![],
            &window,
        );

        if self.first_frame {
            self.first_frame = false;
            window.set_visible(true);
            window.request_redraw();
        }

        if self.close_requested {
            self.finish_close(event_loop);
            return;
        }

        if self.ctx.has_requested_repaint() {
            let now = Instant::now();
            let deadline = take_repaint_deadline(&self.requested_repaint);
            // A future deadline already has a `UserEvent` pending; requesting
            // a redraw here would defeat delayed repaints and 75 Hz pacing.
            if deadline.is_none_or(|when| when <= now) {
                window.request_redraw();
            }
        }
    }

    /// Save the session and stop the loop. `shutdown_and_save` is idempotent,
    /// so every close path can route here without double-saving.
    fn finish_close(&mut self, event_loop: &ActiveEventLoop) {
        self.closing = true;
        if let Some(app) = self.app.as_mut() {
            app.shutdown_and_save();
        }
        log::debug!("close path finished; stopping the event loop");
        event_loop.exit();
    }
}

impl ApplicationHandler<UserEvent> for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.create_window(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Repaint { when } => {
                let when = self.repaint_at.map_or(when, |current| current.min(when));
                self.repaint_at = Some(when);
                event_loop.set_control_flow(ControlFlow::WaitUntil(when));
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        // Once the close path ran, buffered events (a queued
        // `RedrawRequested`, a late `Resized`, ...) must not touch a shut-down
        // app. `Destroyed` still routes to the idempotent finish path.
        if self.closing && !matches!(event, WindowEvent::Destroyed) {
            log::debug!("window event ignored after close: {event:?}");
            return;
        }

        match event {
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::Resized(event_size) => {
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(&window, &event);
                }
                // winit reports `Resized(0, 0)` when Windows minimizes or
                // hides the window, but `inner_size()` on Windows 11 reports
                // the 160x28 client area of the minimized window. Use the
                // event for the zero/minimized test; query the live client
                // area only for a real resize. Windows can also replay a
                // buffered `WM_SIZE` for the current size: resizing and
                // repainting then would multiply paints during a resize
                // storm.
                let minimized = window.is_minimized().unwrap_or(false);
                let live = window.inner_size();
                match surface_resize(
                    self.last_surface_size,
                    self.restore_pending,
                    minimized,
                    (event_size.width, event_size.height),
                    (live.width, live.height),
                ) {
                    SurfaceResize::Deferred => {
                        self.restore_pending = true;
                        log::debug!(
                            "surface minimized or hidden: event {}x{}, live {}x{} (surface {:?})",
                            event_size.width,
                            event_size.height,
                            live.width,
                            live.height,
                            self.last_surface_size
                        );
                    }
                    SurfaceResize::Ignore => {
                        log::debug!(
                            "resize event ignored: {}x{} (surface {:?})",
                            live.width,
                            live.height,
                            self.last_surface_size
                        );
                    }
                    SurfaceResize::Resize(width, height) => {
                        // A restore after minimize may report the
                        // pre-minimize size. That still needs one
                        // resize/repaint pass: otherwise the unchanged-size
                        // dedupe would swallow the only restore paint.
                        let restored = std::mem::take(&mut self.restore_pending);
                        log::debug!(
                            "surface resized {:?} -> {}x{} (restore={restored})",
                            self.last_surface_size,
                            width.get(),
                            height.get()
                        );
                        self.painter
                            .on_window_resized(ViewportId::ROOT, width, height);
                        self.last_surface_size = Some((width.get(), height.get()));
                        // Paint once now so the swapchain is never presented
                        // at the old size while a redraw is still pending.
                        self.redraw(event_loop);
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(&window, &event);
                }
                window.request_redraw();
            }
            WindowEvent::CloseRequested => {
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(&window, &event);
                }
                // Windows withholds `RedrawRequested` for a minimized or hidden
                // window, so a close in that state cannot run the app's veto
                // pass. The app publishes whether it would confirm; when it
                // would, restore and show the window, queue the Close viewport
                // event for the next paint, and return without closing. That
                // paint runs the ordinary pass: the app cancels the close and
                // the confirmation card appears. Unguarded closes still save
                // and exit immediately.
                let can_paint = self.state.is_some()
                    && self.app.is_some()
                    && !window.is_minimized().unwrap_or(false)
                    && window.is_visible().unwrap_or(true);
                match close_decision(can_paint, self.close_guarded.load(Ordering::Relaxed)) {
                    CloseDecision::Paint => {
                        self.info.events.push(ViewportEvent::Close);
                        self.close_requested = true;
                        self.redraw(event_loop);
                    }
                    CloseDecision::RestoreThenPrompt => {
                        log::debug!(
                            "guarded close requested while the window cannot paint; restoring it"
                        );
                        window.set_minimized(false);
                        window.set_visible(true);
                        window.request_redraw();
                        self.info.events.push(ViewportEvent::Close);
                        self.close_requested = true;
                    }
                    CloseDecision::Finish => self.finish_close(event_loop),
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Whether egui-winit will see this transition: it drops button
                // events while it has no pointer position, which is exactly the
                // release of a drag that ended outside the window.
                let seen_by_egui = self
                    .state
                    .as_ref()
                    .is_some_and(egui_winit::State::is_pointer_in_window);
                let button = terminal_button(button);
                let pressed = state == ElementState::Pressed;
                // Physical state first: the app folds the same transition into
                // its gate and needs to know whether a button is still held.
                self.buttons_down.set(button, pressed);
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(&window, &event);
                }
                // Windows still delivers the transition here, so the pointer
                // gate must fold it in: an ignored press would leave a returned
                // drag unpainted, and an ignored release would keep a selection
                // latched. A release also repaints so the view can write the
                // mouse-up it still owes the child.
                if let Some(app) = self.app.as_mut() {
                    app.pointer_button(button, pressed, seen_by_egui, self.pointer_inside);
                }
                window.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                self.info.occluded = Some(occluded);
                if let Some(state) = self.state.as_mut()
                    && state.on_window_event(&window, &event).repaint
                {
                    window.request_redraw();
                }
                if !occluded {
                    self.refresh_native_transparency();
                }
            }
            WindowEvent::ThemeChanged(_) => {
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(&window, &event);
                }
                self.refresh_native_transparency();
                window.request_redraw();
            }
            WindowEvent::Focused(true) => {
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(&window, &event);
                }
                self.refresh_native_transparency();
                window.request_redraw();
            }
            WindowEvent::Destroyed => self.finish_close(event_loop),
            _ => {
                if let Some(state) = self.state.as_mut() {
                    let response = state.on_window_event(&window, &event);
                    // A move only changes the window position; client-relative
                    // content is unchanged. Hover/focus bookkeeping above still
                    // runs, but a repaint here would duplicate the last frame.
                    if response.repaint && event_needs_redraw(&event) {
                        let pointer_paints = match &event {
                            WindowEvent::CursorMoved { position, .. } => {
                                // Cursor moves reach egui's input only on the
                                // next pass, so convert this event's physical
                                // position to the points the last pass used.
                                // Reusing its ppp (rather than the live window
                                // scale) keeps the comparison against a rect
                                // from the same pass across a scale change.
                                let scale = self.pointer_pass_ppp.unwrap_or_else(|| {
                                    ((window.scale_factor() as f32) * self.ctx.zoom_factor())
                                        .max(f32::EPSILON)
                                });
                                let pos = pointer_position_points(*position, f64::from(scale));
                                // Ctrl/Shift from the last pass; no extra input
                                // lock.
                                let ctrl = self.pointer_pass_ctrl;
                                let shift = self.pointer_pass_shift;
                                self.app.as_mut().is_none_or(|app| {
                                    app.pointer_needs_repaint(Some(pos), ctrl, shift)
                                })
                            }
                            _ => true,
                        };
                        let skipped_move =
                            !pointer_paints && matches!(event, WindowEvent::CursorMoved { .. });
                        if pointer_event_needs_redraw(&event, pointer_paints) {
                            window.request_redraw();
                        } else if skipped_move {
                            // A skipped move still sits in egui-winit's queue
                            // until a paint drains it. Keep only the newest so a
                            // long sweep cannot replay as a burst on the next
                            // paint; nothing was painted, so no state is lost.
                            coalesce_pointer_moves(&mut state.egui_input_mut().events);
                        }
                    }
                }
                // Presence and drag memory are tracked from winit, not from
                // egui's one-pass-old pointer state. A button still held on
                // re-entry keeps the drag alive; a gesture that ended outside
                // stays latched so its stale `any_down` cannot paint forever.
                match event {
                    // A move also proves physical presence, which covers a
                    // window that never delivered `CursorEntered`.
                    WindowEvent::CursorEntered { .. } | WindowEvent::CursorMoved { .. } => {
                        self.pointer_inside = true;
                        if let Some(app) = self.app.as_mut() {
                            app.pointer_entered_window(self.buttons_down.any());
                        }
                    }
                    WindowEvent::CursorLeft { .. } => {
                        self.pointer_inside = false;
                        if let Some(app) = self.app.as_mut() {
                            app.pointer_left_window(self.buttons_down.any());
                        }
                    }
                    WindowEvent::Focused(false) => {
                        if let Some(app) = self.app.as_mut() {
                            app.pointer_focus_lost();
                        }
                        // The views just dropped their press state; paint the
                        // result even if egui-winit saw no repaint-worthy
                        // change, so a latched button or autoscroll cannot
                        // outlive the focus change.
                        window.request_redraw();
                    }
                    _ => {}
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(when) = self.repaint_at else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };

        if when <= Instant::now() {
            self.repaint_at = None;
            clear_repaint_deadline(&self.requested_repaint);
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Wait);
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(when));
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(app) = &mut self.app {
            app.shutdown_and_save();
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(app) = &mut self.app {
            app.shutdown_and_save();
        }
        // `Painter::destroy` is a no-op; releasing the surface first lets the
        // swapchain and its DirectComposition visual go away with the window.
        let _ = pollster::block_on(self.painter.set_window(ViewportId::ROOT, None));
        self.app = None;
        self.state = None;
        self.window = None;
        self.painter.destroy();
    }
}

fn handle_actions(
    state: &mut egui_winit::State,
    actions: Vec<egui_winit::ActionRequested>,
) -> bool {
    let mut redraw = false;
    for action in actions {
        match action {
            egui_winit::ActionRequested::Cut => {
                state.egui_input_mut().events.push(egui::Event::Cut);
                redraw = true;
            }
            egui_winit::ActionRequested::Copy => {
                state.egui_input_mut().events.push(egui::Event::Copy);
                redraw = true;
            }
            egui_winit::ActionRequested::Paste => {
                if let Some(contents) = state.clipboard_text() {
                    let contents = contents.replace("\r\n", "\n");
                    if !contents.is_empty() {
                        state
                            .egui_input_mut()
                            .events
                            .push(egui::Event::Paste(contents));
                        redraw = true;
                    }
                }
            }
            egui_winit::ActionRequested::Screenshot(_)
            | egui_winit::ActionRequested::PaintWhileHidden => {}
        }
    }
    redraw
}

fn take_repaint_deadline(deadline: &Mutex<Option<Instant>>) -> Option<Instant> {
    deadline
        .lock()
        .ok()
        .and_then(|mut deadline| deadline.take())
}

/// Whether an event handled by the generic window-event arm needs a paint.
///
/// `Moved` is the only event in that arm that cannot change client-relative
/// content: Windows emits it for every step of a modal window move, and the
/// compositor already shifts the existing frame. Cursor, resize, scale-factor,
/// focus and theme events must still repaint (the latter two also have their
/// own arms).
fn event_needs_redraw(event: &WindowEvent) -> bool {
    !matches!(event, WindowEvent::Moved(_))
}

/// Map a winit mouse button to the terminal's button identity.
///
/// The host only needs this to tell a view which button came up: a release is
/// paid to the button whose press the child was told about. Buttons the
/// terminal encoder never reports map to [`MouseButton::None`], which no view
/// tracks.
fn terminal_button(button: winit::event::MouseButton) -> MouseButton {
    match button {
        winit::event::MouseButton::Left => MouseButton::Left,
        winit::event::MouseButton::Middle => MouseButton::Middle,
        winit::event::MouseButton::Right => MouseButton::Right,
        winit::event::MouseButton::Back
        | winit::event::MouseButton::Forward
        | winit::event::MouseButton::Other(_) => MouseButton::None,
    }
}

/// Whether a generic window event that egui marked for a repaint actually
/// needs one.
///
/// A `CursorMoved` is the only event here whose paint can be pointless: over a
/// bare grid cell nothing on screen depends on the pointer. `pointer_paints`
/// carries the app's decision for that move; every other event keeps painting.
fn pointer_event_needs_redraw(event: &WindowEvent, pointer_paints: bool) -> bool {
    if !event_needs_redraw(event) {
        return false;
    }
    !matches!(event, WindowEvent::CursorMoved { .. }) || pointer_paints
}

/// Keep only the newest queued [`egui::Event::PointerMoved`], in place.
///
/// A pointer move that changes no pixels still sits in egui-winit's pending
/// input until the next paint drains it, so a long sweep would otherwise replay
/// as one burst. Dropping all but the last loses nothing: no pass ran for the
/// skipped moves, and only the final position describes the pointer. Other
/// events keep their order and count. Returns whether a move was queued.
fn coalesce_pointer_moves(events: &mut Vec<egui::Event>) -> bool {
    let Some(last) = events
        .iter()
        .rposition(|event| matches!(event, egui::Event::PointerMoved(_)))
    else {
        return false;
    };
    let mut index = 0;
    events.retain(|event| {
        let keep = !matches!(event, egui::Event::PointerMoved(_)) || index == last;
        index += 1;
        keep
    });
    true
}

/// Convert a physical winit cursor position to egui points with the scale a
/// past pass used. A non-positive scale falls back to the smallest positive
/// value so the result stays finite.
fn pointer_position_points(position: winit::dpi::PhysicalPosition<f64>, scale: f64) -> egui::Pos2 {
    let scale = scale.max(f64::from(f32::EPSILON));
    egui::pos2((position.x / scale) as f32, (position.y / scale) as f32)
}

/// Interpret a pass's viewport commands. A `Close` always wins, so an app that
/// closes its last tab cannot be kept alive by a `CancelClose` sent in the same
/// pass; otherwise `CancelClose` vetoes the OS close frame.
fn cancel_close_requested(commands: &[ViewportCommand]) -> bool {
    !commands
        .iter()
        .any(|command| matches!(command, ViewportCommand::Close))
        && commands
            .iter()
            .any(|command| matches!(command, ViewportCommand::CancelClose))
}

/// What a `CloseRequested` event should do to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseDecision {
    /// Run the app's pass synchronously; it may cancel the close.
    Paint,
    /// The app would confirm, but this window cannot paint. Restore it and
    /// queue the close event so the confirmation appears on the next frame.
    RestoreThenPrompt,
    /// Nothing guards the close; save and stop immediately.
    Finish,
}

/// Decide how to handle a window close.
///
/// A minimized or hidden window gets no `RedrawRequested`, so the confirmation
/// card could never be shown. Without the guard hint the host must fall back to
/// closing directly (single tab or the setting disabled); with it, the window
/// is restored instead of losing its sessions unasked.
fn close_decision(can_paint: bool, guarded: bool) -> CloseDecision {
    if can_paint {
        CloseDecision::Paint
    } else if guarded {
        CloseDecision::RestoreThenPrompt
    } else {
        CloseDecision::Finish
    }
}

/// What a `Resized` event should do to the swapchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceResize {
    /// A zero-sized or minimized event. Do not touch the surface; remember
    /// that the next real event must repaint.
    Deferred,
    /// A buffered `WM_SIZE` replay of the size already applied.
    Ignore,
    /// Resize to these non-zero live client-area dimensions and repaint.
    Resize(NonZeroU32, NonZeroU32),
}

/// Decide whether a `Resized` event resizes the surface.
///
/// `event` is winit's reported size and `live` is `Window::inner_size()`.
/// winit sends `(0, 0)` on minimize, while `inner_size()` can still report a
/// non-zero minimized client area (160x28 on Windows 11), so the event and
/// [`Window::is_minimized`] decide whether the surface may be resized. A
/// real resize uses the live size. The first non-zero event after a
/// minimize/hide always resizes and repaints, even at an unchanged size.
fn surface_resize(
    last: Option<(u32, u32)>,
    restore_pending: bool,
    minimized: bool,
    event: (u32, u32),
    live: (u32, u32),
) -> SurfaceResize {
    if minimized || event.0 == 0 || event.1 == 0 {
        return SurfaceResize::Deferred;
    }
    let (Some(width), Some(height)) = (NonZeroU32::new(live.0), NonZeroU32::new(live.1)) else {
        return SurfaceResize::Deferred;
    };
    if restore_pending || last != Some((width.get(), height.get())) {
        SurfaceResize::Resize(width, height)
    } else {
        SurfaceResize::Ignore
    }
}

fn clear_repaint_deadline(deadline: &Mutex<Option<Instant>>) {
    if let Ok(mut deadline) = deadline.lock() {
        *deadline = None;
    }
}

/// Log the surface alpha capabilities and report `(surface_alpha,
/// transparency_effects_enabled)`. The static alpha capability is kept
/// separately from the live effects setting so the host can re-check effects
/// without recreating the surface. `egui-wgpu` prefers `PreMultiplied` when it
/// is available; this makes the choice visible in the startup log.
fn log_gpu(window: &Arc<Window>, painter: &egui_wgpu::winit::Painter) -> (bool, bool) {
    let Some(render_state) = painter.render_state() else {
        return (false, false);
    };
    let info = render_state.adapter.get_info();
    log::info!("wgpu adapter: {} ({:?})", info.name, info.backend);

    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        // Without the Windows DWM paths, native Mica/Acrylic are unavailable
        // and the app must stay on an opaque Solid surface.
        (false, false)
    }

    // `Painter` does not expose its chosen alpha mode. A throwaway surface
    // reports the same capabilities, and creating it without configuring it
    // does not attach a DirectComposition visual.
    #[cfg(target_os = "windows")]
    match render_state.instance.create_surface(Arc::clone(window)) {
        Ok(surface) => {
            let modes = surface.get_capabilities(&render_state.adapter).alpha_modes;
            let (chosen, alpha_transparent) =
                if modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
                    ("PreMultiplied", true)
                } else if modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
                    ("PostMultiplied", true)
                } else {
                    ("Auto (no transparent mode)", false)
                };
            // wgpu-hal reports PreMultiplied from the DXGI format list, not
            // from whether DWM will composite the result. A transparent
            // backbuffer with transparency effects off (or composition
            // disabled) still presents an opaque window, so both are needed.
            let effects_enabled = crate::platform::transparency_effects_enabled();
            log::info!(
                "surface alpha modes {modes:?}; transparent backbuffer: {chosen}; \
                 Windows transparency effects: {effects_enabled}"
            );
            (alpha_transparent, effects_enabled)
        }
        Err(error) => {
            log::warn!("failed to query surface alpha modes: {error}");
            (false, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CloseDecision, DownButtons, SurfaceResize, cancel_close_requested, close_decision,
        coalesce_pointer_moves, event_needs_redraw, pointer_event_needs_redraw,
        pointer_position_points, surface_resize, terminal_button,
    };
    use egui::ViewportCommand;
    use winit::dpi::{PhysicalPosition, PhysicalSize};
    use winit::event::{DeviceId, WindowEvent};

    fn resized(width: u32, height: u32) -> SurfaceResize {
        SurfaceResize::Resize(
            std::num::NonZeroU32::new(width).expect("width"),
            std::num::NonZeroU32::new(height).expect("height"),
        )
    }

    fn pointer_sample(pos: egui::Pos2, in_chrome: bool) -> crate::app::PointerSample {
        crate::app::PointerSample {
            pos: Some(pos),
            in_chrome,
            on_scrollbar: false,
            scrollbar_changed: false,
            entered_grid: false,
            motion_wanted: false,
            ime_preedit: false,
            ctrl: false,
            overlay_open: false,
            pointing: false,
        }
    }

    #[test]
    fn winit_buttons_map_to_the_terminal_buttons() {
        use illness_term::MouseButton;
        use winit::event::MouseButton as WinitButton;

        assert_eq!(terminal_button(WinitButton::Left), MouseButton::Left);
        assert_eq!(terminal_button(WinitButton::Middle), MouseButton::Middle);
        assert_eq!(terminal_button(WinitButton::Right), MouseButton::Right);
        // Buttons the terminal encoder never reports stay untracked.
        for other in [
            WinitButton::Back,
            WinitButton::Forward,
            WinitButton::Other(7),
        ] {
            assert_eq!(terminal_button(other), MouseButton::None);
        }
    }

    #[test]
    fn cursor_moves_over_the_grid_skip_the_repaint() {
        let moved = WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(400.0, 400.0),
        };
        // A grid hover with no overlay, no Ctrl and no held button paints the
        // same pixels, so the host must not request a redraw.
        assert!(!pointer_event_needs_redraw(
            &moved,
            crate::app::pointer_repaints(&pointer_sample(egui::pos2(400.0, 400.0), false))
        ));
        // A titlebar hover has hover states and must paint.
        assert!(pointer_event_needs_redraw(
            &moved,
            crate::app::pointer_repaints(&pointer_sample(egui::pos2(40.0, 12.0), true))
        ));
        // Non-pointer events keep their existing behavior.
        assert!(pointer_event_needs_redraw(
            &WindowEvent::Focused(false),
            false
        ));
        assert!(!pointer_event_needs_redraw(
            &WindowEvent::Moved(PhysicalPosition::new(10, 20)),
            true
        ));
    }

    #[test]
    fn a_motion_wanting_grid_still_requests_a_redraw() {
        // DECSET 1002/1003 children receive motion reports during a paint, so
        // the gate must not starve them.
        let moved = WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(400.0, 400.0),
        };
        let mut sample = pointer_sample(egui::pos2(400.0, 400.0), false);
        sample.motion_wanted = true;
        assert!(pointer_event_needs_redraw(
            &moved,
            crate::app::pointer_repaints(&sample)
        ));
    }

    #[test]
    fn window_moves_do_not_request_a_paint() {
        assert!(!event_needs_redraw(&WindowEvent::Moved(
            PhysicalPosition::new(10, 20)
        )));
        assert!(event_needs_redraw(&WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(10.0, 20.0),
        }));
        assert!(event_needs_redraw(&WindowEvent::Resized(
            PhysicalSize::new(800, 600)
        )));
        // ScaleFactorChanged cannot be constructed outside winit (its
        // InnerSizeWriter is private), but it shares this default-true branch.
        assert!(event_needs_redraw(&WindowEvent::Focused(false)));
    }

    #[test]
    fn leaving_the_window_or_losing_focus_clears_pointer_memory() {
        // A move leaving the window still paints; the reset itself is not a
        // reason to skip the clearing paint.
        let left = WindowEvent::CursorLeft {
            device_id: DeviceId::dummy(),
        };
        assert!(pointer_event_needs_redraw(&left, false));
    }

    #[test]
    fn winit_reports_each_physical_button_independently() {
        let mut buttons = DownButtons::default();
        assert!(!buttons.any());
        // A press anywhere in the window latches the drag, even one egui
        // drops for want of a pointer position.
        buttons.set(illness_term::MouseButton::Left, true);
        assert!(buttons.any());
        // A second button down keeps the drag alive after the first release.
        buttons.set(illness_term::MouseButton::Right, true);
        buttons.set(illness_term::MouseButton::Left, false);
        assert!(buttons.any());
        buttons.set(illness_term::MouseButton::Right, false);
        assert!(!buttons.any());
        // A release with no matching press cannot underflow the set.
        buttons.set(illness_term::MouseButton::Middle, false);
        assert!(!buttons.any());
        // Buttons the terminal encoder never reports have no bit.
        buttons.set(illness_term::MouseButton::None, true);
        assert!(!buttons.any());
    }

    #[test]
    fn skipped_moves_leave_only_the_newest_queued_move() {
        use egui::Event;

        // Nothing queued: nothing to coalesce.
        assert!(!coalesce_pointer_moves(&mut Vec::new()));

        // A sweep of skipped moves collapses to its final position, in place,
        // and keeps every non-move event around it in order.
        let mut events = vec![
            Event::PointerMoved(egui::pos2(1.0, 1.0)),
            Event::Text("a".to_string()),
            Event::PointerMoved(egui::pos2(2.0, 2.0)),
            Event::PointerMoved(egui::pos2(3.0, 3.0)),
        ];
        assert!(coalesce_pointer_moves(&mut events));
        assert_eq!(
            events,
            vec![
                Event::Text("a".to_string()),
                Event::PointerMoved(egui::pos2(3.0, 3.0)),
            ]
        );

        // A single move is already coalesced.
        let mut one = vec![Event::PointerMoved(egui::pos2(4.0, 4.0))];
        assert!(coalesce_pointer_moves(&mut one));
        assert_eq!(one, vec![Event::PointerMoved(egui::pos2(4.0, 4.0))]);

        // Events without a move are untouched.
        let mut other = vec![
            Event::PointerButton {
                pos: egui::pos2(5.0, 5.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            Event::PointerMoved(egui::pos2(6.0, 6.0)),
            Event::PointerGone,
        ];
        assert!(coalesce_pointer_moves(&mut other));
        assert_eq!(
            other,
            vec![
                Event::PointerButton {
                    pos: egui::pos2(5.0, 5.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                Event::PointerMoved(egui::pos2(6.0, 6.0)),
                Event::PointerGone,
            ]
        );
    }

    #[test]
    fn raw_button_transitions_repair_the_stale_gate() {
        // A drag whose pointer left the window keeps its stale latch until the
        // host sees the next press, which proves the button is held again.
        let mut stale = true;
        let mut down = false;
        assert!(!crate::app::host_button_transition(
            &mut stale, &mut down, true, false
        ));
        assert!(!stale, "a raw press clears the stale latch");
        assert!(down, "a raw press latches the drag");

        // A release ends the gesture and asks for the paint that delivers the
        // mouse-up. egui saw it, so its `any_down` is trustworthy again.
        let mut stale = false;
        let mut down = true;
        assert!(crate::app::host_button_transition(
            &mut stale, &mut down, false, true
        ));
        assert!(!stale);
        assert!(!down, "a raw release ends the drag");

        // A release egui-winit dropped (it had no pointer position) must keep
        // the stale latch: its `any_down` would otherwise re-latch the gate.
        let mut stale = false;
        let mut down = true;
        assert!(crate::app::host_button_transition(
            &mut stale, &mut down, false, false
        ));
        assert!(stale, "a swallowed release stays distrusted");
        assert!(!down);
        // The next real press clears it again.
        assert!(!crate::app::host_button_transition(
            &mut stale, &mut down, true, false
        ));
        assert!(!stale);
        assert!(down);
    }

    #[test]
    fn cursor_positions_convert_with_the_recorded_scale() {
        assert_eq!(
            pointer_position_points(PhysicalPosition::new(200.0, 100.0), 2.0),
            egui::pos2(100.0, 50.0)
        );
        let scaled = pointer_position_points(PhysicalPosition::new(300.0, 150.0), 1.5);
        assert!((scaled.x - 200.0).abs() < 1.0e-4);
        assert!((scaled.y - 100.0).abs() < 1.0e-4);
        // A non-positive scale cannot divide by zero.
        let clamped = pointer_position_points(PhysicalPosition::new(10.0, 10.0), 0.0);
        assert!(clamped.x.is_finite() && clamped.y.is_finite());
        assert!(clamped.x > 0.0 && clamped.y > 0.0);
    }

    #[test]
    fn close_wins_over_cancel_close_in_the_same_pass() {
        assert!(cancel_close_requested(&[ViewportCommand::CancelClose]));
        assert!(!cancel_close_requested(&[ViewportCommand::Close]));
        assert!(!cancel_close_requested(&[
            ViewportCommand::CancelClose,
            ViewportCommand::Close,
        ]));
        assert!(!cancel_close_requested(&[]));
    }

    #[test]
    fn hidden_or_minimized_guarded_close_restores_before_prompting() {
        // A window that can paint always runs the app pass, which decides
        // whether to prompt.
        assert_eq!(close_decision(true, false), CloseDecision::Paint);
        assert_eq!(close_decision(true, true), CloseDecision::Paint);
        // Cannot paint and the app would confirm: restore, do not close.
        assert_eq!(
            close_decision(false, true),
            CloseDecision::RestoreThenPrompt
        );
        // Cannot paint and nothing guards the close: the old fast path.
        assert_eq!(close_decision(false, false), CloseDecision::Finish);
    }

    #[test]
    fn surface_size_change_dedupes_replays_and_skips_zero() {
        assert_eq!(
            surface_resize(None, false, false, (1180, 760), (1180, 760)),
            resized(1180, 760)
        );
        assert_eq!(
            surface_resize(Some((1180, 760)), false, false, (1180, 760), (1180, 760)),
            SurfaceResize::Ignore
        );
        assert_eq!(
            surface_resize(Some((1180, 760)), false, false, (0, 0), (160, 28)),
            SurfaceResize::Deferred
        );
        assert_eq!(
            surface_resize(Some((1180, 760)), false, false, (0, 400), (0, 400)),
            SurfaceResize::Deferred
        );
        assert_eq!(
            surface_resize(Some((1180, 760)), false, false, (800, 600), (800, 600)),
            resized(800, 600)
        );
    }

    #[test]
    fn minimized_events_never_resize_to_the_live_client_area() {
        // winit says 0x0 while Windows 11's inner_size() reports 160x28.
        assert_eq!(
            surface_resize(Some((1180, 760)), false, true, (0, 0), (160, 28)),
            SurfaceResize::Deferred
        );
        // Even a non-zero event must not resize while IsIconic is true.
        assert_eq!(
            surface_resize(Some((1180, 760)), false, true, (1180, 760), (160, 28)),
            SurfaceResize::Deferred
        );
    }

    #[test]
    fn real_resizes_use_the_live_size_not_the_event_size() {
        assert_eq!(
            surface_resize(Some((1180, 760)), false, false, (100, 100), (800, 600)),
            resized(800, 600),
            "the buffered event size is stale; the client area is authoritative"
        );
    }

    #[test]
    fn restore_after_minimize_repaints_at_an_unchanged_size() {
        assert_eq!(
            surface_resize(Some((1180, 760)), true, false, (1180, 760), (1180, 760)),
            resized(1180, 760)
        );
        assert_eq!(
            surface_resize(Some((1180, 760)), true, false, (800, 600), (800, 600)),
            resized(800, 600)
        );
        // A zero-size marker is still not applicable.
        assert_eq!(
            surface_resize(Some((1180, 760)), true, false, (0, 760), (0, 760)),
            SurfaceResize::Deferred
        );
    }
}
