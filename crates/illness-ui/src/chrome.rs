use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Id, Key, LayerId, Layout, Modifiers, Order,
    PointerButton, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, ViewportCommand,
    text::{LayoutJob, TextFormat},
};
use illness_core::{Material, Profile, Rgb};

use crate::design::Tokens;
use crate::icons::{self, Icon};
use crate::overlays::SearchRequest;

const TITLEBAR_HEIGHT: f32 = 40.0;
const TAB_START_X: f32 = 8.0;
const MIN_TAB_WIDTH: f32 = 120.0;
const MAX_TAB_WIDTH: f32 = 220.0;
const TAB_GAP: f32 = 2.0;
const TAB_SCROLL_STEP: f32 = 72.0;
const DRAG_THRESHOLD: f32 = 4.0;
const DOUBLE_CLICK_SECS: f64 = 0.4;
const PLUS_WIDTH: f32 = 30.0;
const CHEVRON_WIDTH: f32 = 22.0;
const NEW_TAB_HEIGHT: f32 = 28.0;
const CLOSE_SIZE: f32 = 16.0;
const MENU_WIDTH: f32 = 228.0;
const EMPTY_BAR: usize = usize::MAX;
const WINDOW_CLOSE_HOVER: Color32 = Color32::from_rgb(0xc4, 0x2b, 0x1c);
const MICA_BAR_ALPHA: u8 = 244;
const ACRYLIC_BAR_ALPHA: u8 = 236;

#[derive(Debug, Clone, PartialEq)]
pub struct TabInfo {
    pub title: String,
    pub profile: String,
    pub accent: Rgb,
    pub exited: bool,
}

impl Default for TabInfo {
    fn default() -> Self {
        Self {
            title: String::new(),
            profile: String::new(),
            accent: Rgb::new(0x4c, 0x9a, 0xff),
            exited: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    NewTab(Option<String>),
    CloseTab(usize),
    ActivateTab(usize),
    MoveTab(usize, usize),
    OpenPalette,
    OpenSearch,
    OpenSettings,
    Minimize,
    Maximize,
    CloseWindow,
    ConfigChanged,
    ToggleMaterial,
    Copy,
    Paste,
    SelectAll,
    Clear,
    Search(SearchRequest),
}

#[derive(Debug, Default)]
pub struct ChromeState {
    pub tab_scroll: f32,
    pub dragging: Option<(usize, f32)>,
    pub dropdown_open: bool,
    pub last_click: Option<(f64, usize)>,
    pub hover_close: Option<usize>,
}

/// Draw the frameless titlebar, tab strip, new-tab split button and caption
/// buttons. The caller owns window commands and acts on the returned actions.
pub fn show(
    ctx: &egui::Context,
    state: &mut ChromeState,
    tokens: &Tokens,
    tabs: &[TabInfo],
    profiles: &[Profile],
    active: usize,
    material: Material,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let screen = ctx.content_rect();
    if screen.width() <= 0.0 {
        return actions;
    }

    let bar_height = if tokens.titlebar_h > 0.0 {
        tokens.titlebar_h
    } else {
        TITLEBAR_HEIGHT
    };
    let bar = Rect::from_min_size(screen.min, Vec2::new(screen.width(), bar_height));
    let layer = LayerId::new(Order::Middle, Id::unique("illness_chrome"));
    let ui = Ui::new(
        ctx.clone(),
        Id::unique("illness_chrome_ui"),
        UiBuilder::new()
            .max_rect(bar)
            .layer_id(layer)
            .layout(Layout::top_down(Align::Min)),
    );
    let painter = ui.painter().clone();
    let bar_fill = bar_fill(tokens, material);

    painter.rect_filled(bar, 0, bar_fill);
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(bar.left(), bar.bottom() - 1.0), bar.max),
        0,
        tokens.border,
    );

    let caption_width = if tokens.caption_w > 0.0 {
        tokens.caption_w
    } else {
        46.0
    };
    let captions = caption_rects(bar.right(), bar.top(), caption_width, bar_height);
    let captions_left = captions[0].left();
    let plus_left = (captions_left - PLUS_WIDTH - CHEVRON_WIDTH - 6.0).max(bar.left());
    let tabs_left = bar.left() + TAB_START_X;
    let tabs_right = (plus_left - 4.0).max(tabs_left);
    let tab_viewport = Rect::from_min_max(
        Pos2::new(tabs_left, bar.top()),
        Pos2::new(tabs_right, bar.bottom()),
    );
    let tab_height = if tokens.tabbar_h > 0.0 && tokens.tabbar_h <= bar_height {
        tokens.tabbar_h
    } else {
        bar_height
    };
    let tab_y = bar.bottom() - tab_height;

    let slots = layout_tabs(tab_viewport.width(), tabs.len());
    let content_width = tabs_content_width(&slots);
    let max_scroll = (content_width - tab_viewport.width()).max(0.0);
    let pointer = ctx.input(|i| i.pointer.hover_pos());
    let now = ctx.input(|i| i.time);
    let primary_down = ctx.input(|i| i.pointer.primary_down());
    let pointer_delta = ctx.input(|i| i.pointer.delta());

    if pointer.is_some_and(|pos| tab_viewport.contains(pos)) {
        let wheel_notches = ctx.input(|i| {
            i.events.iter().fold(0.0, |sum, event| {
                if let egui::Event::MouseWheel { unit, delta, .. } = event {
                    sum + match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Point => delta.y / 50.0,
                        egui::MouseWheelUnit::Page => delta.y * 5.0,
                    }
                } else {
                    sum
                }
            })
        });
        if wheel_notches.abs() > f32::EPSILON {
            state.tab_scroll -= TAB_SCROLL_STEP * wheel_notches;
        }
    }
    state.tab_scroll = state.tab_scroll.clamp(0.0, max_scroll);
    let scroll = state.tab_scroll;

    if state.dragging.is_some_and(|(index, _)| index >= tabs.len()) {
        state.dragging = None;
    }
    if let Some((from, accumulated)) = state.dragging {
        if primary_down && !tabs.is_empty() {
            let accumulated = accumulated + pointer_delta.x;
            let step = slots
                .first()
                .map_or(MIN_TAB_WIDTH + TAB_GAP, |(_, width)| width + TAB_GAP);
            let steps = reorder_steps(accumulated, step);
            if steps == 0 {
                state.dragging = Some((from, accumulated));
            } else {
                let target = (from as isize + steps).clamp(0, tabs.len().saturating_sub(1) as isize)
                    as usize;
                state.dragging = Some((target, 0.0));
                if target != from {
                    state.last_click = None;
                    actions.push(Action::MoveTab(from, target));
                }
            }
        } else {
            state.dragging = None;
        }
    }

    let hovered_index = pointer.and_then(|pos| {
        if !tab_viewport.contains(pos) {
            return None;
        }
        let tab_width = slots.first().map_or(MIN_TAB_WIDTH, |(_, width)| *width);
        tab_index_at(pos.x - tab_viewport.left(), tab_width, scroll, tabs.len())
    });

    state.hover_close = None;
    let top_radius = top_tab_radius(tokens);
    let tabs_painter = painter.with_clip_rect(tab_viewport);
    let title_font = FontId::proportional(13.0);

    for (index, tab) in tabs.iter().enumerate() {
        let Some(&(offset, width)) = slots.get(index) else {
            break;
        };
        let rect = Rect::from_min_size(
            Pos2::new(tab_viewport.left() + offset - scroll, tab_y),
            Vec2::new(width, tab_height),
        );
        let visible = rect.intersect(tab_viewport);
        if visible.width() <= 0.0 || visible.height() <= 0.0 {
            continue;
        }

        let is_active = index == active;
        let hovered = hovered_index == Some(index);
        let fill = if is_active {
            tokens.bg
        } else if hovered {
            tokens.hover
        } else {
            Color32::TRANSPARENT
        };
        if fill != Color32::TRANSPARENT {
            tabs_painter.rect_filled(rect, top_radius, fill);
        }
        if is_active {
            tabs_painter.rect_stroke(
                rect,
                top_radius,
                Stroke::new(1.0, tokens.border),
                StrokeKind::Inside,
            );
        }

        let dot_color = if tab.exited {
            tokens.danger
        } else {
            crate::design::color(tab.accent)
        };
        tabs_painter.circle_filled(
            Pos2::new(rect.left() + 13.0, rect.center().y),
            3.0,
            dot_color,
        );

        let close_rect = Rect::from_center_size(
            Pos2::new(rect.right() - 18.0, rect.center().y),
            Vec2::splat(CLOSE_SIZE),
        );
        let close_hovered = pointer.is_some_and(|pos| close_rect.contains(pos));
        if close_hovered {
            state.hover_close = Some(index);
        }
        let show_close = is_active || hovered || close_hovered;
        let title_left = rect.left() + 23.0;
        let title_right = (close_rect.left() - 4.0).max(title_left);
        let title_color = if tab.exited {
            tokens.text_dim
        } else if is_active {
            tokens.text
        } else {
            tokens.text_dim
        };
        paint_ellipsized(
            &tabs_painter,
            &tab.title,
            Rect::from_min_max(
                Pos2::new(title_left, rect.top() + 4.0),
                Pos2::new(title_right, rect.bottom() - 4.0),
            ),
            title_font.clone(),
            title_color,
        );

        if show_close {
            if close_hovered {
                tabs_painter.rect_filled(close_rect, 4.0, tokens.bg_sunken);
            }
            icons::paint_icon(
                &tabs_painter,
                Rect::from_center_size(close_rect.center(), Vec2::splat(8.0)),
                Icon::Close,
                if close_hovered {
                    tokens.text
                } else {
                    tokens.text_dim
                },
            );
        }

        let body = ui
            .interact(
                visible,
                Id::unique(("illness_tab", index)),
                Sense::click_and_drag(),
            )
            .on_hover_text(if tab.profile.is_empty() {
                tab.title.clone()
            } else {
                format!("{} · {}", tab.profile, tab.title)
            });

        if body.middle_clicked() {
            actions.push(Action::CloseTab(index));
        }
        if body.drag_started_by(PointerButton::Primary) && !close_hovered {
            state.dragging = Some((index, 0.0));
        }

        let mut closed = false;
        if show_close {
            let close = ui.interact(
                close_rect.intersect(visible),
                Id::unique(("illness_tab_close", index)),
                Sense::click(),
            );
            if close.clicked() {
                closed = true;
                state.last_click = None;
                actions.push(Action::CloseTab(index));
            }
        }

        if !closed && body.clicked_by(PointerButton::Primary) {
            if let Some(Action::Maximize) = double_click_action(now, state.last_click, index) {
                actions.push(Action::Maximize);
                state.last_click = None;
            } else {
                state.last_click = Some((now, index));
                if index != active {
                    actions.push(Action::ActivateTab(index));
                }
            }
        }
    }

    if content_width > tab_viewport.width() {
        let fade_width = 18.0_f32.min(tab_viewport.width() / 3.0);
        if scroll > 0.0 {
            paint_edge_fade(
                &tabs_painter,
                Rect::from_min_max(
                    tab_viewport.left_top(),
                    Pos2::new(tab_viewport.left() + fade_width, tab_viewport.bottom()),
                ),
                bar_fill,
                true,
            );
        }
        if scroll < max_scroll {
            paint_edge_fade(
                &tabs_painter,
                Rect::from_min_max(
                    Pos2::new(tab_viewport.right() - fade_width, tab_viewport.top()),
                    tab_viewport.right_bottom(),
                ),
                bar_fill,
                false,
            );
        }
    }

    // Merge the active tab into the terminal by erasing the bar's bottom border
    // underneath it. Painting after the tabs keeps hover fades from covering it.
    if let Some(&(offset, width)) = slots.get(active) {
        let rect = Rect::from_min_size(
            Pos2::new(tab_viewport.left() + offset - scroll, tab_y),
            Vec2::new(width, tab_height),
        );
        let visible = rect.intersect(tab_viewport);
        if visible.width() > 0.0 {
            tabs_painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(visible.left(), bar.bottom() - 1.0),
                    Pos2::new(visible.right(), bar.bottom()),
                ),
                0,
                tokens.bg,
            );
        }
    }

    let plus_rect = Rect::from_min_size(
        Pos2::new(plus_left, bar.top() + (bar_height - NEW_TAB_HEIGHT) / 2.0),
        Vec2::new(PLUS_WIDTH, NEW_TAB_HEIGHT),
    );
    let chevron_rect = Rect::from_min_size(
        Pos2::new(plus_rect.right(), plus_rect.top()),
        Vec2::new(CHEVRON_WIDTH, NEW_TAB_HEIGHT),
    );

    let plus = ui.interact(plus_rect, Id::unique("illness_new_tab"), Sense::click());
    if plus.hovered() {
        painter.rect_filled(plus_rect, 6.0, tokens.hover);
    }
    icons::paint_icon(
        &painter,
        Rect::from_center_size(plus_rect.center(), Vec2::splat(12.0)),
        Icon::Plus,
        tokens.text,
    );
    if plus.clicked() {
        actions.push(Action::NewTab(None));
        state.dropdown_open = false;
    }

    let chevron = ui.interact(
        chevron_rect,
        Id::unique("illness_new_tab_menu_button"),
        Sense::click(),
    );
    if chevron.hovered() || state.dropdown_open {
        painter.rect_filled(chevron_rect, 6.0, tokens.hover);
    }
    icons::paint_icon(
        &painter,
        Rect::from_center_size(chevron_rect.center(), Vec2::splat(10.0)),
        Icon::ChevronDown,
        tokens.text_dim,
    );
    if chevron.clicked() {
        state.dropdown_open = !state.dropdown_open;
    }

    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    for (index, rect) in captions.iter().enumerate() {
        let response = ui.interact(
            *rect,
            Id::unique(("illness_caption", index)),
            Sense::click(),
        );
        let close = index == 2;
        if response.hovered() {
            painter.rect_filled(
                *rect,
                0,
                if close {
                    WINDOW_CLOSE_HOVER
                } else {
                    tokens.hover
                },
            );
        }
        let icon = match index {
            0 => Icon::Minimize,
            1 if maximized => Icon::Restore,
            1 => Icon::Maximize,
            _ => Icon::Close,
        };
        let icon_color = if close && response.hovered() {
            Color32::WHITE
        } else {
            tokens.text
        };
        icons::paint_icon(
            &painter,
            Rect::from_center_size(rect.center(), Vec2::splat(10.0)),
            icon,
            icon_color,
        );
        if response.clicked() {
            actions.push(match index {
                0 => Action::Minimize,
                1 => Action::Maximize,
                _ => Action::CloseWindow,
            });
        }
        let label = match index {
            0 => "Minimize",
            1 if maximized => "Restore",
            1 => "Maximize",
            _ => "Close",
        };
        response.on_hover_text(label);
    }

    let mut empty_regions = Vec::new();
    empty_regions.push(Rect::from_min_max(
        bar.min,
        Pos2::new(tabs_left, bar.bottom()),
    ));
    if tabs.is_empty() && plus_left > tabs_left {
        empty_regions.push(Rect::from_min_max(
            Pos2::new(tabs_left, bar.top()),
            Pos2::new(plus_left, bar.bottom()),
        ));
    } else if !tabs.is_empty() {
        if tab_y > bar.top() {
            empty_regions.push(Rect::from_min_max(
                Pos2::new(tabs_left, bar.top()),
                Pos2::new(tabs_right, tab_y),
            ));
        }
        let content_right = tab_viewport.left() + content_width - scroll;
        if content_right < plus_left {
            empty_regions.push(Rect::from_min_max(
                Pos2::new(content_right, tab_y),
                Pos2::new(plus_left, bar.bottom()),
            ));
        }
    }
    if plus_rect.top() > bar.top() {
        empty_regions.push(Rect::from_min_max(
            Pos2::new(tabs_right, bar.top()),
            Pos2::new(captions_left, plus_rect.top()),
        ));
    }
    if chevron_rect.right() < captions_left {
        empty_regions.push(Rect::from_min_max(
            Pos2::new(chevron_rect.right(), plus_rect.top()),
            Pos2::new(captions_left, bar.bottom()),
        ));
    }

    for (index, rect) in empty_regions.iter().enumerate() {
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            continue;
        }
        let response = ui.interact(
            *rect,
            Id::unique(("illness_bar_empty", index)),
            Sense::click_and_drag(),
        );
        if response.drag_started_by(PointerButton::Primary) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if response.clicked_by(PointerButton::Primary) {
            if let Some(Action::Maximize) = double_click_action(now, state.last_click, EMPTY_BAR) {
                actions.push(Action::Maximize);
                state.last_click = None;
            } else {
                state.last_click = Some((now, EMPTY_BAR));
            }
        }
    }

    if state.dropdown_open {
        let menu_x = menu_left(chevron_rect, bar);
        let menu = egui::Area::new(Id::unique("illness_new_tab_dropdown"))
            .order(Order::Foreground)
            .fixed_pos(Pos2::new(menu_x, bar.bottom() + 4.0))
            .constrain_to(ctx.content_rect())
            .show(ctx, |ui| {
                let framed = egui::Frame::NONE
                    .fill(tokens.bg_elev)
                    .corner_radius(tokens.radius_md)
                    .inner_margin(6)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                        ui.set_width(MENU_WIDTH - 12.0);
                        ui.set_min_width(MENU_WIDTH - 12.0);

                        let mut chosen = None;
                        for profile in profiles {
                            if menu_item(ui, tokens, &profile.name, Icon::Terminal, None).clicked()
                            {
                                chosen = Some(Action::NewTab(Some(profile.name.clone())));
                            }
                        }

                        if !profiles.is_empty() {
                            let (separator, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 7.0),
                                Sense::hover(),
                            );
                            ui.painter().line_segment(
                                [
                                    Pos2::new(separator.left() + 6.0, separator.center().y),
                                    Pos2::new(separator.right() - 6.0, separator.center().y),
                                ],
                                Stroke::new(1.0, tokens.border),
                            );
                        }

                        if menu_item(ui, tokens, "Settings", Icon::Settings, Some("Ctrl+,"))
                            .clicked()
                        {
                            chosen = Some(Action::OpenSettings);
                        }
                        if menu_item(
                            ui,
                            tokens,
                            "Command Palette",
                            Icon::Palette,
                            Some("Ctrl+Shift+P"),
                        )
                        .clicked()
                        {
                            chosen = Some(Action::OpenPalette);
                        }
                        chosen
                    });
                // Frame counts stroke width as layout margin; drawing the
                // border inside keeps the outer width exactly MENU_WIDTH.
                ui.painter().rect_stroke(
                    framed.response.rect,
                    tokens.radius_md,
                    Stroke::new(1.0, tokens.border),
                    StrokeKind::Inside,
                );
                framed.inner
            });

        if let Some(action) = menu.inner {
            actions.push(action);
            state.dropdown_open = false;
        }

        let menu_rect = menu.response.rect;
        let clicked_outside = ctx.input(|i| {
            i.pointer.any_click()
                && i.pointer.interact_pos().is_some_and(|pos| {
                    !menu_rect.contains(pos)
                        && !chevron_rect.contains(pos)
                        && !plus_rect.contains(pos)
                })
        });
        let escape = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        if clicked_outside || escape {
            state.dropdown_open = false;
        }
    }

    actions
}

fn top_tab_radius(tokens: &Tokens) -> CornerRadius {
    let radius = tokens.radius_md.clamp(0.0, 16.0).round() as u8;
    CornerRadius {
        nw: radius,
        ne: radius,
        sw: 0,
        se: 0,
    }
}

/// Opaque for Solid, lightly translucent over Mica/Acrylic so the bar reads as
/// elevated while the window material still shows through.
fn bar_fill(tokens: &Tokens, material: Material) -> Color32 {
    let color = tokens.bg_elev;
    let alpha = match material {
        Material::Solid => return color,
        Material::Mica => MICA_BAR_ALPHA,
        Material::Acrylic => ACRYLIC_BAR_ALPHA,
    };
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Left edge for the new-tab menu: its right edge sits on the chevron's right
/// edge, clamped to the bar on narrow windows.
fn menu_left(chevron: Rect, bar: Rect) -> f32 {
    let min = bar.left() + 4.0;
    let max = (bar.right() - MENU_WIDTH - 4.0).max(min);
    (chevron.right() - MENU_WIDTH).clamp(min, max)
}

/// Tab slots as `(x_offset, width)`, relative to the tab strip origin.
/// Widths clamp to 120..=220 and the strip is allowed to overflow.
fn layout_tabs(width: f32, count: usize) -> Vec<(f32, f32)> {
    if count == 0 {
        return Vec::new();
    }
    let width = if width.is_finite() {
        width.max(0.0)
    } else {
        0.0
    };
    let tab_width = (width / count as f32).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH);
    (0..count)
        .map(|index| (index as f32 * (tab_width + TAB_GAP), tab_width))
        .collect()
}

fn tabs_content_width(tabs: &[(f32, f32)]) -> f32 {
    tabs.last().map_or(0.0, |(x, width)| x + width)
}

/// Index of the tab under `x` (relative to the strip), accounting for scroll.
fn tab_index_at(x: f32, tab_width: f32, scroll: f32, count: usize) -> Option<usize> {
    if count == 0 || tab_width <= 0.0 {
        return None;
    }
    let content_x = x + scroll;
    if content_x < 0.0 {
        return None;
    }
    let step = tab_width + TAB_GAP;
    let index = (content_x / step).floor() as usize;
    if index >= count {
        return None;
    }
    let inside = content_x - index as f32 * step;
    (inside <= tab_width).then_some(index)
}

fn drag_threshold(delta: f32) -> bool {
    delta.abs() >= DRAG_THRESHOLD
}

fn reorder_steps(delta: f32, step: f32) -> isize {
    if !drag_threshold(delta) || step <= 0.0 {
        return 0;
    }
    (delta / step).round() as isize
}

fn double_click_action(now: f64, last: Option<(f64, usize)>, index: usize) -> Option<Action> {
    match last {
        Some((then, last_index))
            if last_index == index && now >= then && now - then <= DOUBLE_CLICK_SECS =>
        {
            Some(Action::Maximize)
        }
        _ => None,
    }
}

/// Minimize, maximize/restore and close rectangles, anchored at `right`.
fn caption_rects(right: f32, top: f32, width: f32, height: f32) -> [Rect; 3] {
    let close = Rect::from_min_size(Pos2::new(right - width, top), Vec2::new(width, height));
    let maximize = close.translate(Vec2::new(-width, 0.0));
    let minimize = maximize.translate(Vec2::new(-width, 0.0));
    [minimize, maximize, close]
}

fn paint_ellipsized(
    painter: &egui::Painter,
    text: &str,
    rect: Rect,
    font_id: FontId,
    color: Color32,
) {
    if rect.width() <= 1.0 || rect.height() <= 1.0 || text.is_empty() {
        return;
    }
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat::simple(font_id, color));
    job.break_on_newline = false;
    job.wrap.max_width = rect.width();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.fonts_mut(|fonts| fonts.layout_job(job));
    painter.galley(
        Pos2::new(rect.left(), rect.center().y - galley.size().y * 0.5),
        galley,
        color,
    );
}

fn paint_edge_fade(painter: &egui::Painter, rect: Rect, color: Color32, from_left: bool) {
    if rect.width() <= 0.0 {
        return;
    }
    let (left, right) = if from_left {
        (color, Color32::TRANSPARENT)
    } else {
        (Color32::TRANSPARENT, color)
    };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

fn menu_item(
    ui: &mut Ui,
    tokens: &Tokens,
    label: &str,
    icon: Icon,
    shortcut: Option<&str>,
) -> egui::Response {
    let width = ui.available_width().max(MENU_WIDTH - 12.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect.shrink2(Vec2::new(2.0, 1.0)), 6.0, tokens.hover);
    }
    icons::paint_icon(
        painter,
        Rect::from_center_size(
            Pos2::new(rect.left() + 18.0, rect.center().y),
            Vec2::splat(14.0),
        ),
        icon,
        tokens.text_dim,
    );
    painter.text(
        Pos2::new(rect.left() + 34.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.5),
        tokens.text,
    );
    if let Some(shortcut) = shortcut {
        painter.text(
            Pos2::new(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            shortcut,
            FontId::proportional(10.5),
            tokens.text_dim,
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_rects_never_overlap() {
        let tabs = layout_tabs(600.0, 8);
        assert_eq!(tabs.len(), 8);
        for pair in tabs.windows(2) {
            assert!(
                pair[1].0 >= pair[0].0 + pair[0].1,
                "tabs overlap: {:?} {:?}",
                pair[0],
                pair[1]
            );
        }
        for (_, width) in &tabs {
            assert!((*width - MIN_TAB_WIDTH).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn tabs_clamp_and_overflow() {
        let narrow = layout_tabs(600.0, 8);
        assert_eq!(narrow[0].1, MIN_TAB_WIDTH);
        assert!(tabs_content_width(&narrow) > 600.0);

        let wide = layout_tabs(4000.0, 2);
        assert_eq!(wide[0].1, MAX_TAB_WIDTH);
        assert!(tabs_content_width(&wide) <= 4000.0);

        assert!(layout_tabs(600.0, 0).is_empty());
    }

    #[test]
    fn tab_index_accounts_for_scroll_and_gaps() {
        assert_eq!(tab_index_at(0.0, 120.0, 0.0, 8), Some(0));
        assert_eq!(tab_index_at(119.0, 120.0, 0.0, 8), Some(0));
        assert_eq!(tab_index_at(121.0, 120.0, 0.0, 8), None);
        assert_eq!(tab_index_at(122.0, 120.0, 0.0, 8), Some(1));
        assert_eq!(tab_index_at(0.0, 120.0, 122.0, 8), Some(1));
        assert_eq!(tab_index_at(100.0, 120.0, 0.0, 8), Some(0));
        assert_eq!(tab_index_at(10_000.0, 120.0, 0.0, 8), None);
    }

    #[test]
    fn four_pixels_does_not_reorder() {
        assert!(!drag_threshold(3.9));
        assert!(drag_threshold(4.0));
        assert!(drag_threshold(-4.0));
        assert_eq!(reorder_steps(4.0, MIN_TAB_WIDTH + TAB_GAP), 0);
        assert_eq!(reorder_steps(-4.0, MIN_TAB_WIDTH + TAB_GAP), 0);
        assert_eq!(reorder_steps(200.0, MIN_TAB_WIDTH + TAB_GAP), 2);
    }

    #[test]
    fn captions_end_at_window_edge() {
        let rects = caption_rects(1180.0, 0.0, 46.0, 40.0);
        assert_eq!(rects[2].right(), 1180.0);
        assert_eq!(rects[2].width(), 46.0);
        assert_eq!(rects[2].height(), 40.0);
        assert_eq!(rects[0].left(), 1180.0 - 3.0 * 46.0);
        assert_eq!(rects[0].right(), rects[1].left());
        assert_eq!(rects[1].right(), rects[2].left());
    }

    #[test]
    fn double_click_emits_maximize() {
        assert_eq!(double_click_action(1.0, None, 2), None);
        assert_eq!(
            double_click_action(1.3, Some((1.0, 2)), 2),
            Some(Action::Maximize)
        );
        assert_eq!(double_click_action(1.5, Some((1.0, 2)), 2), None);
        assert_eq!(double_click_action(1.3, Some((1.0, 2)), 3), None);
    }

    #[test]
    fn empty_bar_double_click_emits_maximize() {
        assert_eq!(
            double_click_action(10.2, Some((10.0, EMPTY_BAR)), EMPTY_BAR),
            Some(Action::Maximize)
        );
        assert_eq!(
            double_click_action(10.5, Some((10.0, EMPTY_BAR)), EMPTY_BAR),
            None
        );
    }

    #[test]
    fn show_renders_tabs_headlessly() {
        let ctx = egui::Context::default();
        let tokens = Tokens::default();
        let tabs = vec![
            TabInfo {
                title: "A very long PowerShell tab title that must ellipsize".to_owned(),
                profile: "PowerShell".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
                exited: false,
            },
            TabInfo {
                title: "Command Prompt".to_owned(),
                profile: "Command Prompt".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
                exited: true,
            },
        ];
        let mut state = ChromeState::default();
        let mut actions = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 760.0))),
            ..Default::default()
        };

        let mut output = ctx.run_ui(input, |_| {
            actions = show(&ctx, &mut state, &tokens, &tabs, &[], 0, Material::Solid);
        });
        output.textures_delta.clear();

        assert!(actions.is_empty());
    }

    #[test]
    fn bar_fill_is_opaque_for_solid_and_tinted_otherwise() {
        let tokens = Tokens::default();
        assert_eq!(bar_fill(&tokens, Material::Solid), tokens.bg_elev);
        let [red, green, blue, alpha] = bar_fill(&tokens, Material::Mica).to_srgba_unmultiplied();
        assert_eq!(alpha, 244);
        assert_eq!(
            (red, green, blue),
            (tokens.bg_elev.r(), tokens.bg_elev.g(), tokens.bg_elev.b())
        );
        assert_eq!(
            bar_fill(&tokens, Material::Acrylic).to_srgba_unmultiplied()[3],
            236
        );
    }

    #[test]
    fn menu_right_edge_anchors_to_chevron() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let chevron = Rect::from_min_size(Pos2::new(900.0, 6.0), Vec2::new(22.0, 28.0));
        let left = menu_left(chevron, bar);
        assert!((left + MENU_WIDTH - chevron.right()).abs() < f32::EPSILON);
        assert!(left >= bar.left());

        let left_chevron = Rect::from_min_size(Pos2::new(18.0, 6.0), Vec2::new(22.0, 28.0));
        assert!((menu_left(left_chevron, bar) - (bar.left() + 4.0)).abs() < f32::EPSILON);

        let narrow = Rect::from_min_size(Pos2::ZERO, Vec2::new(240.0, 40.0));
        let right_chevron = Rect::from_min_size(Pos2::new(216.0, 6.0), Vec2::new(22.0, 28.0));
        let left = menu_left(right_chevron, narrow);
        assert!(left + MENU_WIDTH <= narrow.right());
    }

    #[test]
    fn dropdown_keeps_menu_width() {
        let ctx = egui::Context::default();
        let tokens = Tokens::default();
        let tabs = vec![TabInfo::default()];
        let profiles = [Profile {
            name: "PowerShell".to_owned(),
            program: "pwsh.exe".to_owned(),
            args: Vec::new(),
            accent: Rgb::new(0x7e, 0xe7, 0x87),
        }];
        let mut state = ChromeState {
            dropdown_open: true,
            ..ChromeState::default()
        };
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 760.0))),
            ..Default::default()
        };

        let mut output = ctx.run_ui(input, |_| {
            let _ = show(
                &ctx,
                &mut state,
                &tokens,
                &tabs,
                &profiles,
                0,
                Material::Solid,
            );
        });
        output.textures_delta.clear();

        let rect = ctx
            .memory(|memory| memory.area_rect(Id::unique("illness_new_tab_dropdown")))
            .expect("dropdown area");
        assert!(
            (rect.width() - MENU_WIDTH).abs() < 0.5,
            "dropdown width {} should be {MENU_WIDTH}",
            rect.width()
        );
    }
}
