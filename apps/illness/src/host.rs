//! Owned winit + egui + wgpu window host.
//!
//! Illness creates its own window so it can opt into
//! `WS_EX_NOREDIRECTIONBITMAP` and the DXGI/DirectComposition presentation
//! path, which a transparent native backdrop requires. Everything else — the
//! surface, textures, resizing and paint callbacks — is delegated to
//! [`egui_wgpu::winit::Painter`].

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{ViewportCommand, ViewportEvent, ViewportId, ViewportInfo};
use egui_wgpu::{RendererOptions, SurfaceConfig, WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::app;

const INITIAL_INNER_SIZE: [f32; 2] = [1180.0, 760.0];
const MIN_INNER_SIZE: [f32; 2] = [720.0, 480.0];

/// Wake-up messages from egui's repaint scheduler.
#[derive(Debug)]
enum UserEvent {
    Repaint { when: Instant },
}

pub fn run(icon: Option<Arc<egui::IconData>>) -> anyhow::Result<()> {
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

    let mut host = Host::new(ctx, painter, proxy, icon);
    event_loop.run_app(&mut host)?;
    Ok(())
}

fn wgpu_configuration(display_handle: winit::event_loop::OwnedDisplayHandle) -> WgpuConfiguration {
    let mut setup = WgpuSetupCreateNew::from_display_handle(display_handle);
    setup.instance_descriptor.backends = wgpu::Backends::DX12;
    // A DXGI swapchain created from a DirectComposition visual is the only
    // DX12 presentation path that supports a transparent swapchain.
    setup
        .instance_descriptor
        .backend_options
        .dx12
        .presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;

    let mut config = WgpuConfiguration::default().with_surface_config(SurfaceConfig::LOW_LATENCY);
    config.wgpu_setup = WgpuSetup::CreateNew(setup);
    config
}

struct Host {
    ctx: egui::Context,
    painter: egui_wgpu::winit::Painter,
    proxy: EventLoopProxy<UserEvent>,
    icon: Option<Arc<egui::IconData>>,
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
    first_frame: bool,
}

impl Host {
    fn new(
        ctx: egui::Context,
        painter: egui_wgpu::winit::Painter,
        proxy: EventLoopProxy<UserEvent>,
        icon: Option<Arc<egui::IconData>>,
    ) -> Self {
        Self {
            ctx,
            painter,
            proxy,
            icon,
            state: None,
            window: None,
            app: None,
            info: ViewportInfo::default(),
            start: Instant::now(),
            requested_repaint: Arc::new(Mutex::new(None)),
            repaint_at: None,
            close_requested: false,
            first_frame: true,
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
            event_loop.exit();
            return;
        }
        log_gpu(&window, &self.painter);

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
        let render_state = self.painter.render_state();
        let app = app::IllnessApp::new(&self.ctx, render_state.as_ref(), window.clone());

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

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
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

        let commands = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map(|viewport| viewport.commands.clone())
            .unwrap_or_default();
        let cancel_close = commands
            .iter()
            .any(|command| matches!(command, ViewportCommand::CancelClose));
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
            app.shutdown_and_save();
            event_loop.exit();
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

        match event {
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::Resized(size) => {
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(&window, &event);
                }
                // A zero size is winit's minimize marker on Windows; there is
                // no surface to resize in that case.
                if let (Some(width), Some(height)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    self.painter
                        .on_window_resized(ViewportId::ROOT, width, height);
                    window.request_redraw();
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
                // Give the app one last pass so it can save before the loop
                // exits. `IllnessApp::shutdown_and_save` is idempotent.
                self.info.events.push(ViewportEvent::Close);
                self.close_requested = true;
                window.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                self.info.occluded = Some(occluded);
                if let Some(state) = self.state.as_mut()
                    && state.on_window_event(&window, &event).repaint
                {
                    window.request_redraw();
                }
            }
            WindowEvent::Destroyed => {
                if let Some(app) = self.app.as_mut() {
                    app.shutdown_and_save();
                }
                event_loop.exit();
            }
            _ => {
                if let Some(state) = self.state.as_mut()
                    && state.on_window_event(&window, &event).repaint
                {
                    window.request_redraw();
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

fn clear_repaint_deadline(deadline: &Mutex<Option<Instant>>) {
    if let Ok(mut deadline) = deadline.lock() {
        *deadline = None;
    }
}

/// Log the surface alpha capabilities. `egui-wgpu` prefers `PreMultiplied`
/// when it is available; this makes the choice visible in the startup log.
fn log_gpu(window: &Arc<Window>, painter: &egui_wgpu::winit::Painter) {
    let Some(render_state) = painter.render_state() else {
        return;
    };
    let info = render_state.adapter.get_info();
    log::info!("wgpu adapter: {} ({:?})", info.name, info.backend);

    // `Painter` does not expose its chosen alpha mode. A throwaway surface
    // reports the same capabilities, and creating it without configuring it
    // does not attach a DirectComposition visual.
    match render_state.instance.create_surface(Arc::clone(window)) {
        Ok(surface) => {
            let modes = surface.get_capabilities(&render_state.adapter).alpha_modes;
            let chosen = if modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
                "PreMultiplied"
            } else if modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
                "PostMultiplied"
            } else {
                "Auto (no transparent mode)"
            };
            log::info!("surface alpha modes {modes:?}; transparent backbuffer: {chosen}");
        }
        Err(error) => log::warn!("failed to query surface alpha modes: {error}"),
    }
}
