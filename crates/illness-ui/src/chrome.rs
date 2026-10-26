use egui::{
    Align, Color32, CornerRadius, FontId, Id, Key, LayerId, Layout, Modifiers, Order,
    PointerButton, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, ViewportCommand,
    text::{LayoutJob, TextFormat},
};
use illness_core::{Material, Profile, Rgb};

use crate::design::{self, Tokens};
use crate::icons::{self, Icon};
use crate::overlays::SearchRequest;

const TITLEBAR_HEIGHT: f32 = 40.0;
const TAB_START_X: f32 = 8.0;
const MIN_TAB_WIDTH: f32 = 120.0;
const MAX_TAB_WIDTH: f32 = 220.0;
const TAB_GAP: f32 = 2.0;
const TAB_SCROLL_STEP: f32 = 72.0;
const DRAG_THRESHOLD: f32 = 4.0;
const EDGE_SCROLL_ZONE: f32 = 28.0;
const EDGE_SCROLL_SPEED: f32 = 900.0;
const GHOST_LIFT: f32 = 3.0;
const DROP_SLOP: f32 = 12.0;
const DOUBLE_CLICK_SECS: f64 = 0.4;
const PLUS_WIDTH: f32 = 30.0;
const CHEVRON_WIDTH: f32 = 26.0;
const NEW_TAB_HEIGHT: f32 = 28.0;
const TAB_CLOSE_SIZE: f32 = 20.0;
const TAB_CLOSE_GLYPH: f32 = 12.0;
const TAB_CLOSE_STROKE: f32 = 1.5;
const PLUS_GLYPH: f32 = 16.0;
const CHEVRON_GLYPH: f32 = 16.0;
const CONTROL_GAP: f32 = 4.0;
const CAPTION_GAP: f32 = 2.0;
const CAPTION_WIDTH_FALLBACK: f32 = 48.0;
const CAPTION_ICON_SIZE: f32 = 12.0;
const CAPTION_ICON_STROKE: f32 = 1.0;
const MENU_WIDTH: f32 = 280.0;
const EMPTY_BAR: u64 = u64::MAX - 1;
const WINDOW_CLOSE_HOVER: Color32 = Color32::from_rgb(0xc4, 0x2b, 0x1c);
const HOVER_SECS: f32 = 0.12;
/// No visible label can show more characters than this, so layout work stays
/// bounded even when a shell sets an enormous OSC title.
const MAX_LAYOUT_CHARS: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct TabInfo {
    /// Stable tab identity. Slot indices move when tabs reorder or close, so a
    /// drag tracks this value to avoid grabbing a different tab mid-gesture.
    pub id: u64,
    pub title: String,
    pub profile: String,
    pub accent: Rgb,
}

impl Default for TabInfo {
    fn default() -> Self {
        Self {
            id: 0,
            title: String::new(),
            profile: String::new(),
            accent: Rgb::new(0x4c, 0x9a, 0xff),
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

/// A tab drag in progress. `index` is the tab's current slot; `original` is
/// restored when the pointer is released outside the strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabDrag {
    pub id: u64,
    pub original: usize,
    pub index: usize,
    pub grab_offset: f32,
    pub start_x: f32,
    pub start_y: f32,
    pub pointer_x: f32,
    pub pointer_y: f32,
    /// False for a long press that egui resolves as a drag without movement.
    pub moved: bool,
    pub tab_count: usize,
}

#[derive(Debug, Default)]
pub struct ChromeState {
    pub tab_scroll: f32,
    pub drag: Option<TabDrag>,
    pub dropdown_open: bool,
    pub last_click: Option<(f64, u64)>,
    pub hover_close: Option<usize>,
}

/// Draw the frameless titlebar, tab strip, unified new-tab split button, and caption buttons.
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

    let ppp = ctx.pixels_per_point();
    let layout = chrome_layout(bar, tokens, tabs.len());
    let captions = layout.captions;
    let captions_left = layout.captions_left;
    let tabs_left = layout.tab_viewport.left();
    let tab_viewport = layout.tab_viewport;
    let tab_height = layout.tab_height;
    let tab_y = layout.tab_y;
    let slots = &layout.slots;
    let content_width = tabs_content_width(slots);

    let max_scroll = (content_width - tab_viewport.width()).max(0.0);
    let pointer = ctx.input(|i| i.pointer.hover_pos());
    let now = ctx.input(|i| i.time);
    let primary_down = ctx.input(|i| i.pointer.primary_down());
    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
    let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.05);

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

    // A tab list that changed underneath the gesture (shell exit, close,
    // new tab) no longer has a meaningful original index. Drop the drag
    // rather than moving or activating whichever tab now occupies the slot.
    if state.drag.is_some_and(|drag| {
        drag.tab_count != tabs.len() || tabs.get(drag.index).map(|tab| tab.id) != Some(drag.id)
    }) {
        state.drag = None;
        state.last_click = None;
    }

    // Losing focus or the pointer cancels the gesture: the mouse-up may never
    // reach egui, and a stale drag would eat the next click.
    if state.drag.is_some() && (!focused || pointer.is_none()) {
        if let Some(drag) = state.drag
            && drag.index != drag.original
            && tabs.get(drag.index).map(|tab| tab.id) == Some(drag.id)
        {
            actions.push(Action::MoveTab(drag.index, drag.original));
            ctx.request_repaint();
        }
        state.drag = None;
        state.last_click = None;
    }

    if state.drag.is_some() && take_escape(ctx) {
        if let Some(drag) = state.drag
            && drag.index != drag.original
        {
            actions.push(Action::MoveTab(drag.index, drag.original));
            ctx.request_repaint();
        }
        state.drag = None;
        state.last_click = None;
    }

    if let Some(mut drag) = state.drag {
        if let Some(pos) = pointer {
            drag.pointer_x = pos.x;
            drag.pointer_y = pos.y;
            if !drag.moved
                && (pos - Pos2::new(drag.start_x, drag.start_y)).length() >= DRAG_THRESHOLD
            {
                drag.moved = true;
            }
        }
        if primary_down && drag.moved && !tabs.is_empty() {
            let pointer = Pos2::new(drag.pointer_x, drag.pointer_y);
            if drop_allowed(pointer, bar) {
                let before = state.tab_scroll;
                state.tab_scroll = edge_scroll(
                    drag.pointer_x,
                    tab_viewport,
                    dt,
                    state.tab_scroll,
                    max_scroll,
                );
                if state.tab_scroll != before {
                    ctx.request_repaint();
                }
            }
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        // The release is committed after painting so the drop frame still
        // shows the floating tab instead of snapping it back to its old slot.
        state.drag = Some(drag);
    }

    let scroll = state.tab_scroll;
    let dragging = state.drag.is_some_and(|drag| drag.moved);
    let drag_index = state.drag.filter(|drag| drag.moved).map(|drag| drag.index);

    let hovered_index = pointer.and_then(|pos| {
        if dragging || !tab_viewport.contains(pos) {
            return None;
        }
        let tab_width = slots.first().map_or(MIN_TAB_WIDTH, |(_, width)| *width);
        tab_index_at(pos.x - tab_viewport.left(), tab_width, scroll, tabs.len())
    });

    state.hover_close = None;
    let top_radius = top_tab_radius(tokens);
    let tabs_painter = painter.with_clip_rect(tab_viewport);
    let title_font = FontId::proportional(tokens.font_body);
    let mut drag_started_this_frame = false;
    let mut closed_this_frame = false;

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

        if drag_index == Some(index) {
            // Leave a visible landing slot while the tab itself floats under
            // the pointer.
            tabs_painter.rect_filled(rect, top_radius, design::fade(tokens.hover, 0.5));
            tabs_painter.rect_stroke(
                rect,
                top_radius,
                Stroke::new(1.0, tokens.border),
                StrokeKind::Inside,
            );
            continue;
        }

        let is_active = index == active;
        let hovered = hovered_index == Some(index);
        let hover_t = ctx.animate_bool_with_time(
            Id::unique(("illness_tab_hover", index)),
            hovered && !is_active,
            HOVER_SECS,
        );
        let fill = if is_active {
            active_tab_fill(tokens, material)
        } else {
            design::fade(tokens.hover, hover_t)
        };
        if fill.a() > 0 {
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

        let icon_color = crate::design::color(tab.accent);
        icons::paint_icon(
            &tabs_painter,
            Rect::from_min_size(
                Pos2::new(rect.left() + 8.0, rect.center().y - 6.0),
                Vec2::splat(12.0),
            ),
            Icon::Terminal,
            icon_color,
        );

        let close_rect = Rect::from_center_size(
            Pos2::new(rect.right() - 20.0, rect.center().y),
            Vec2::splat(TAB_CLOSE_SIZE),
        );
        let close_hovered = !dragging && pointer.is_some_and(|pos| close_rect.contains(pos));
        if close_hovered {
            state.hover_close = Some(index);
        }
        let show_close = is_active || hovered || close_hovered;
        let title_left = rect.left() + 24.0;
        let title_right = (close_rect.left() - 4.0).max(title_left);
        let title_color = if is_active {
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
            let close_t = ctx.animate_bool_with_time(
                Id::unique(("illness_tab_close_bg", index)),
                close_hovered,
                HOVER_SECS,
            );
            if close_t > 0.0 {
                tabs_painter.rect_filled(close_rect, 4.0, design::fade(tokens.bg_sunken, close_t));
            }
            paint_tab_close(
                &tabs_painter,
                close_rect.center(),
                crate::design::mix(tokens.text_dim, tokens.text, close_t),
            );
        }

        let body = ui.interact(
            visible,
            Id::unique(("illness_tab", index)),
            Sense::click_and_drag(),
        );
        // Build the tooltip string only while it can be shown. Doing this for
        // every tab every frame allocates a full title copy per tab.
        let body = if body.hovered() {
            body.on_hover_text(if tab.profile.is_empty() {
                tab.title.clone()
            } else {
                format!("{} · {}", tab.profile, tab.title)
            })
        } else {
            body
        };

        if body.middle_clicked() {
            closed_this_frame = true;
            state.last_click = None;
            actions.push(Action::CloseTab(index));
        }
        if body.drag_started_by(PointerButton::Primary) {
            let pos = body
                .interact_pointer_pos()
                .or(pointer)
                .unwrap_or(rect.center());
            // The press decides whether this gesture belongs to the close
            // button. Checking the current pointer would cancel a drag that
            // merely passes over the close glyph.
            let press = ctx.input(|i| i.pointer.press_origin()).unwrap_or(pos);
            if !close_rect.contains(press) {
                drag_started_this_frame = true;
                state.drag = Some(TabDrag {
                    id: tab.id,
                    original: index,
                    index,
                    grab_offset: press.x - rect.left(),
                    start_x: press.x,
                    start_y: press.y,
                    pointer_x: pos.x,
                    pointer_y: pos.y,
                    moved: (pos - press).length() >= DRAG_THRESHOLD,
                    tab_count: tabs.len(),
                });
                state.last_click = None;
            }
        }

        let mut closed = false;
        if show_close {
            let close = ui.interact(
                close_rect.intersect(visible),
                Id::unique(("illness_tab_close", index)),
                Sense::click(),
            );
            // egui's click slop allows a press that slides a few pixels off
            // the X. A pointer release must still be on the glyph; keyboard and
            // accesskit activation has no pointer release and stays allowed.
            let release_on_close =
                primary_release_pos(ctx).is_none_or(|pos| close_rect.contains(pos));
            if close.clicked() && release_on_close {
                closed = true;
                closed_this_frame = true;
                state.last_click = None;
                actions.push(Action::CloseTab(index));
            }
        }

        if !closed && body.clicked_by(PointerButton::Primary) {
            if let Some(Action::Maximize) = double_click_action(now, state.last_click, tab.id) {
                actions.push(Action::Maximize);
                state.last_click = None;
            } else {
                state.last_click = Some((now, tab.id));
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

    // Merge active tab bottom border, unless that tab is currently floating.
    if let Some(&(offset, width)) = slots.get(active) {
        let rect = Rect::from_min_size(
            Pos2::new(tab_viewport.left() + offset - scroll, tab_y),
            Vec2::new(width, tab_height),
        );
        let visible = rect.intersect(tab_viewport);
        if visible.width() > 0.0 && drag_index != Some(active) {
            tabs_painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(visible.left(), bar.bottom() - 1.0),
                    Pos2::new(visible.right(), bar.bottom()),
                ),
                0,
                active_tab_fill(tokens, material),
            );
        }
    }

    // The dragged tab floats above the strip, following the pointer. Skip the
    // start frame: the loop above already painted it normally, and the ghost
    // would duplicate it.
    if !drag_started_this_frame
        && let Some(drag) = state.drag
        && drag.moved
        && let Some(&(_, width)) = slots.get(drag.index)
    {
        let ghost = ghost_rect(
            width,
            drag.pointer_x,
            drag.grab_offset,
            tab_viewport,
            tab_y,
            tab_height,
        );
        let ghost_painter = painter.with_clip_rect(bar);
        let shadow = ghost.translate(Vec2::new(0.0, 2.0)).expand(2.0);
        ghost_painter.rect_filled(shadow, top_radius, design::fade(Color32::BLACK, 0.28));
        ghost_painter.rect_filled(ghost, top_radius, active_tab_fill(tokens, material));
        ghost_painter.rect_stroke(
            ghost,
            top_radius,
            Stroke::new(1.0, tokens.border),
            StrokeKind::Inside,
        );

        icons::paint_icon(
            &ghost_painter,
            Rect::from_min_size(
                Pos2::new(ghost.left() + 8.0, ghost.center().y - 6.0),
                Vec2::splat(12.0),
            ),
            Icon::Terminal,
            crate::design::color(
                tabs.get(drag.index)
                    .map_or(Rgb::new(0x4c, 0x9a, 0xff), |tab| tab.accent),
            ),
        );
        paint_ellipsized(
            &ghost_painter,
            tabs.get(drag.index).map_or("", |tab| tab.title.as_str()),
            Rect::from_min_max(
                Pos2::new(ghost.left() + 24.0, ghost.top() + 4.0),
                Pos2::new(ghost.right() - 8.0, ghost.bottom() - 4.0),
            ),
            title_font.clone(),
            tokens.text,
        );
    }

    // Unified New-Tab & Profile-Dropdown split cluster
    let plus_rect = layout.plus;
    let chevron_rect = layout.chevron;

    let plus = ui.interact(plus_rect, Id::unique("illness_new_tab"), Sense::click());
    let plus_hover = ctx.animate_bool_with_time(
        Id::unique("illness_new_tab_hover"),
        plus.hovered(),
        HOVER_SECS,
    );
    if plus_hover > 0.0 {
        painter.rect_filled(
            plus_rect,
            CornerRadius {
                nw: 6,
                ne: 0,
                sw: 6,
                se: 0,
            },
            design::fade(tokens.hover, plus_hover),
        );
    }
    icons::paint_icon(
        &painter,
        Rect::from_center_size(plus_rect.center(), Vec2::splat(PLUS_GLYPH)),
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
    let chevron_hover = ctx.animate_bool_with_time(
        Id::unique("illness_new_tab_menu_hover"),
        chevron.hovered() || state.dropdown_open,
        HOVER_SECS,
    );
    if chevron_hover > 0.0 {
        painter.rect_filled(
            chevron_rect,
            CornerRadius {
                nw: 0,
                ne: 6,
                sw: 0,
                se: 6,
            },
            design::fade(tokens.hover, chevron_hover),
        );
    }
    icons::paint_icon(
        &painter,
        Rect::from_center_size(chevron_rect.center(), Vec2::splat(CHEVRON_GLYPH)),
        Icon::ChevronDown,
        tokens.text_dim,
    );
    if chevron.clicked() {
        state.dropdown_open = !state.dropdown_open;
    }

    // Caption buttons (Minimize, Maximize, Close)
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    for (index, rect) in captions.iter().enumerate() {
        let response = ui.interact(
            *rect,
            Id::unique(("illness_caption", index)),
            Sense::click(),
        );
        let close = index == 2;
        let hover = ctx.animate_bool_with_time(
            Id::unique(("illness_caption_hover", index)),
            response.hovered(),
            HOVER_SECS,
        );
        if hover > 0.0 {
            painter.rect_filled(
                *rect,
                0,
                if close {
                    design::fade(WINDOW_CLOSE_HOVER, hover)
                } else {
                    design::fade(tokens.hover, hover)
                },
            );
        }
        let icon_color = if close {
            crate::design::mix(tokens.text, Color32::WHITE, hover)
        } else {
            tokens.text
        };
        paint_caption_glyph(&painter, rect.center(), index, maximized, icon_color, ppp);
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

    // Empty regions available for window dragging
    let mut empty_regions = Vec::new();

    // 1. Gutter left of the tabs
    empty_regions.push(Rect::from_min_max(
        bar.min,
        Pos2::new(tabs_left, bar.bottom()),
    ));

    // 2. Dead strip above the tab strip
    if tab_y > bar.top() {
        empty_regions.push(Rect::from_min_max(
            Pos2::new(tabs_left, bar.top()),
            Pos2::new(captions_left, tab_y.min(plus_rect.top())),
        ));
    }

    // 3. Huge unified draggable titlebar between [v] and caption buttons
    if chevron_rect.right() < captions_left {
        empty_regions.push(Rect::from_min_max(
            Pos2::new(chevron_rect.right() + 2.0, bar.top()),
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

    // New Tab Profile Dropdown menu
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

    // Reorder after painting so this frame's slot and the floating tab never
    // both show the same tab. The app applies the move before the next frame.
    // A close in the same frame would shift the tab list before MoveTab runs,
    // so cancel instead of moving a stale index.
    if closed_this_frame {
        state.drag = None;
        ctx.request_repaint();
    } else if let Some(mut drag) = state.drag {
        let pointer = Pos2::new(drag.pointer_x, drag.pointer_y);
        if !primary_down {
            // The release frame has already painted the floating tab. Commit
            // or restore now; the app moves the strip before the next frame.
            if !drag.moved {
                // A long press is a click, not a reorder.
                actions.push(Action::ActivateTab(drag.index));
            } else if drop_allowed(pointer, bar) {
                let target = drop_index(
                    drag.pointer_x,
                    tab_viewport,
                    slots,
                    state.tab_scroll,
                    drag.index,
                );
                if target != drag.index {
                    actions.push(Action::MoveTab(drag.index, target));
                }
                // Always activate, not just when `target != active`: MoveTab
                // remaps the old active index, and the dragged tab may land
                // exactly where the old active tab was.
                actions.push(Action::ActivateTab(target));
            } else if drag.index != drag.original {
                actions.push(Action::MoveTab(drag.index, drag.original));
            }
            state.drag = None;
            ctx.request_repaint();
        } else if !drag.moved {
            state.drag = Some(drag);
        } else if drop_allowed(pointer, bar) {
            let target = drop_index(
                drag.pointer_x,
                tab_viewport,
                slots,
                state.tab_scroll,
                drag.index,
            );
            if target != drag.index {
                actions.push(Action::MoveTab(drag.index, target));
                drag.index = target;
                state.last_click = None;
                ctx.request_repaint();
            }
            state.drag = Some(drag);
        } else {
            state.drag = Some(drag);
        }
    }

    actions
}

#[derive(Debug, Clone)]
struct ChromeLayout {
    captions: [Rect; 3],
    captions_left: f32,
    chevron: Rect,
    plus: Rect,
    tab_viewport: Rect,
    tab_y: f32,
    tab_height: f32,
    slots: Vec<(f32, f32)>,
}

fn chrome_layout(bar: Rect, tokens: &Tokens, tab_count: usize) -> ChromeLayout {
    let bar_height = bar.height();
    let caption_width = if tokens.caption_w > 0.0 {
        tokens.caption_w
    } else {
        CAPTION_WIDTH_FALLBACK
    };
    let captions = caption_rects(bar.right(), bar.top(), caption_width, bar_height);
    let captions_left = captions[0].left();

    let tab_height = if tokens.tabbar_h > 0.0 && tokens.tabbar_h <= bar_height {
        tokens.tabbar_h
    } else {
        bar_height
    };
    let tab_y = bar.bottom() - tab_height;

    let control_height = NEW_TAB_HEIGHT.min(bar_height.max(0.0));
    let control_top = tab_y + (tab_height - control_height) / 2.0;

    let tabs_left = bar.left() + TAB_START_X;

    // Both plus and chevron sit together immediately adjacent to each other
    let combined_controls_width = PLUS_WIDTH + CHEVRON_WIDTH;
    let controls_limit = (captions_left - CAPTION_GAP - combined_controls_width).max(tabs_left);
    let tabs_available = (controls_limit - CONTROL_GAP - tabs_left).max(0.0);

    let slots = layout_tabs(tabs_available, tab_count);
    let content_width = tabs_content_width(&slots);

    let plus_left = (tabs_left + content_width + CONTROL_GAP)
        .min(controls_limit)
        .max(tabs_left);
    let plus = Rect::from_min_size(
        Pos2::new(plus_left, control_top),
        Vec2::new(PLUS_WIDTH, control_height),
    );

    let chevron_left = plus.right();
    let chevron = Rect::from_min_size(
        Pos2::new(chevron_left, control_top),
        Vec2::new(CHEVRON_WIDTH, control_height),
    );

    let tabs_right = (plus.left() - CONTROL_GAP).max(tabs_left);
    let tab_viewport = Rect::from_min_max(
        Pos2::new(tabs_left, bar.top()),
        Pos2::new(tabs_right, bar.bottom()),
    );

    ChromeLayout {
        captions,
        captions_left,
        chevron,
        plus,
        tab_viewport,
        tab_y,
        tab_height,
        slots,
    }
}

fn physical_stroke(ppp: f32, logical: f32) -> f32 {
    let ppp = if ppp.is_finite() && ppp > 0.0 {
        ppp
    } else {
        1.0
    };
    ((logical * ppp).round().max(1.0)) / ppp
}

fn paint_caption_glyph(
    painter: &egui::Painter,
    center: Pos2,
    index: usize,
    maximized: bool,
    color: Color32,
    ppp: f32,
) {
    let stroke = Stroke::new(physical_stroke(ppp, CAPTION_ICON_STROKE), color);
    let extent = CAPTION_ICON_SIZE / 2.0;
    match index {
        0 => {
            painter.line_segment(
                [
                    Pos2::new(center.x - extent, center.y),
                    Pos2::new(center.x + extent, center.y),
                ],
                stroke,
            );
        }
        1 if maximized => {
            let front = Rect::from_center_size(center + Vec2::new(1.0, 1.0), Vec2::splat(8.0));
            let back = Rect::from_center_size(center - Vec2::new(1.0, 1.0), Vec2::splat(8.0));
            painter.rect_stroke(back, 0.0, stroke, StrokeKind::Inside);
            painter.rect_stroke(front, 0.0, stroke, StrokeKind::Inside);
        }
        1 => {
            let box_rect = Rect::from_center_size(center, Vec2::splat(CAPTION_ICON_SIZE - 3.0));
            painter.rect_stroke(box_rect, 0.0, stroke, StrokeKind::Inside);
        }
        _ => {
            painter.line_segment(
                [
                    center - Vec2::splat(extent - 0.5),
                    center + Vec2::splat(extent - 0.5),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x - extent + 0.5, center.y + extent - 0.5),
                    Pos2::new(center.x + extent - 0.5, center.y - extent + 0.5),
                ],
                stroke,
            );
        }
    }
}

fn paint_tab_close(painter: &egui::Painter, center: Pos2, color: Color32) {
    let half = TAB_CLOSE_GLYPH / 2.0;
    let stroke = Stroke::new(TAB_CLOSE_STROKE, color);
    painter.line_segment(
        [center - Vec2::splat(half), center + Vec2::splat(half)],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(center.x - half, center.y + half),
            Pos2::new(center.x + half, center.y - half),
        ],
        stroke,
    );
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

fn bar_fill(tokens: &Tokens, material: Material) -> Color32 {
    let color = tokens.bg_elev;
    let alpha = design::material_alpha(material);
    if alpha == 255 {
        color
    } else {
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
    }
}

fn active_tab_fill(tokens: &Tokens, material: Material) -> Color32 {
    let alpha = design::material_alpha(material);
    if alpha == 255 {
        tokens.bg
    } else {
        Color32::from_rgba_unmultiplied(tokens.bg.r(), tokens.bg.g(), tokens.bg.b(), alpha)
    }
}

fn menu_left(chevron: Rect, bar: Rect) -> f32 {
    let min = bar.left() + 4.0;
    let max = (bar.right() - MENU_WIDTH - 4.0).max(min);
    chevron.left().clamp(min, max)
}

fn layout_tabs(width: f32, count: usize) -> Vec<(f32, f32)> {
    if count == 0 {
        return Vec::new();
    }
    let width = if width.is_finite() {
        width.max(0.0)
    } else {
        0.0
    };
    let gaps = TAB_GAP * count.saturating_sub(1) as f32;
    let tab_width = ((width - gaps).max(0.0) / count as f32).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH);
    (0..count)
        .map(|index| (index as f32 * (tab_width + TAB_GAP), tab_width))
        .collect()
}

fn tabs_content_width(tabs: &[(f32, f32)]) -> f32 {
    tabs.last().map_or(0.0, |(x, width)| x + width)
}

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

/// Final index for the dragged tab, counted among the tabs that remain.
///
/// A tab inserts before the first remaining tab whose center is right of the
/// pointer. Offsets are content coordinates, so scrolling is added back once
/// and the pointer may sit outside the visible strip.
fn drop_index(
    pointer_x: f32,
    viewport: Rect,
    slots: &[(f32, f32)],
    scroll: f32,
    dragged: usize,
) -> usize {
    if slots.is_empty() {
        return 0;
    }
    let content_x = pointer_x - viewport.left() + scroll;
    let mut index = 0;
    for (slot, &(offset, width)) in slots.iter().enumerate() {
        if slot == dragged {
            continue;
        }
        let center = offset + width / 2.0;
        if content_x > center {
            index += 1;
        } else {
            break;
        }
    }
    index.min(slots.len().saturating_sub(1))
}

/// Scroll the strip while the pointer rests in an edge zone.
///
/// The returned scroll is clamped to the strip; `dt` is capped by the caller so
/// a stalled frame cannot jump the list.
fn edge_scroll(pointer_x: f32, viewport: Rect, dt: f32, scroll: f32, max_scroll: f32) -> f32 {
    if max_scroll <= 0.0 || viewport.width() <= 0.0 || dt <= 0.0 {
        return scroll.clamp(0.0, max_scroll.max(0.0));
    }
    let zone = EDGE_SCROLL_ZONE.min(viewport.width() / 4.0);
    if zone <= 0.0 {
        return scroll.clamp(0.0, max_scroll);
    }
    let left = viewport.left() + zone;
    let right = viewport.right() - zone;
    let direction = if pointer_x < left {
        -((left - pointer_x) / zone).clamp(0.0, 1.0)
    } else if pointer_x > right {
        ((pointer_x - right) / zone).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (scroll + direction * EDGE_SCROLL_SPEED * dt).clamp(0.0, max_scroll)
}

/// Where the floating tab is painted while the pointer drags it.
fn ghost_rect(
    width: f32,
    pointer_x: f32,
    grab_offset: f32,
    viewport: Rect,
    tab_y: f32,
    tab_height: f32,
) -> Rect {
    let width = width.min(viewport.width()).max(0.0);
    let max_left = (viewport.right() - width).max(viewport.left());
    let left = (pointer_x - grab_offset).clamp(viewport.left(), max_left);
    Rect::from_min_size(
        Pos2::new(left, tab_y - GHOST_LIFT),
        Vec2::new(width, tab_height),
    )
}

/// A release belongs to the strip only near the titlebar. Dragging into the
/// terminal is a cancel because tear-out is not supported.
fn drop_allowed(pointer: Pos2, bar: Rect) -> bool {
    pointer.y >= bar.top() - DROP_SLOP && pointer.y <= bar.bottom() + DROP_SLOP
}

/// Consume any Escape press, with or without modifiers, while cancelling a
/// drag. Leaving the event in the queue would type ESC into the shell.
fn take_escape(ctx: &egui::Context) -> bool {
    ctx.input_mut(|input| {
        let mut hit = false;
        input.events.retain(|event| {
            let escape = matches!(
                event,
                egui::Event::Key {
                    key: Key::Escape,
                    pressed: true,
                    ..
                }
            );
            hit |= escape;
            !escape
        });
        hit
    })
}

/// Position of the primary-button release in this frame, if any.
fn primary_release_pos(ctx: &egui::Context) -> Option<Pos2> {
    ctx.input(|input| {
        input.events.iter().rev().find_map(|event| match event {
            egui::Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                ..
            } => Some(*pos),
            _ => None,
        })
    })
}

/// Bound the text handed to the layout engine. No tab can display this many
/// characters, and OSC titles can be arbitrarily long.
fn layout_text(text: &str) -> &str {
    text.char_indices()
        .nth(MAX_LAYOUT_CHARS)
        .map_or(text, |(end, _)| &text[..end])
}

fn double_click_action(now: f64, last: Option<(f64, u64)>, id: u64) -> Option<Action> {
    match last {
        Some((then, last_id))
            if last_id == id && now >= then && now - then <= DOUBLE_CLICK_SECS =>
        {
            Some(Action::Maximize)
        }
        _ => None,
    }
}

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
    let mut job = LayoutJob::single_section(
        layout_text(text).to_owned(),
        TextFormat::simple(font_id, color),
    );
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
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 32.0), Sense::click());
    let painter = ui.painter().with_clip_rect(rect.shrink(1.0));
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), HOVER_SECS);

    if hover > 0.0 {
        painter.rect_filled(
            rect.shrink2(Vec2::new(2.0, 1.0)),
            tokens.radius_sm,
            design::fade(tokens.hover, hover),
        );
    }

    // Left Icon
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 18.0, rect.center().y),
        Vec2::splat(15.0),
    );
    icons::paint_icon(
        &painter,
        icon_rect,
        icon,
        design::mix(tokens.text_dim, tokens.text, hover),
    );

    // Styled Keyboard Shortcut Badge. Lay it out before the label so the label
    // can be ellipsized into the space that remains.
    let mut label_right = rect.right() - 8.0;
    if let Some(shortcut) = shortcut {
        let font = FontId::proportional(10.5);
        let galley = painter.layout_no_wrap(shortcut.to_owned(), font, tokens.text_dim);
        let badge_size = galley.size() + Vec2::new(10.0, 4.0);
        let badge_rect = Rect::from_min_size(
            Pos2::new(
                rect.right() - badge_size.x - 8.0,
                rect.center().y - badge_size.y * 0.5,
            ),
            badge_size,
        );

        painter.rect_filled(badge_rect, tokens.radius_sm - 1.0, tokens.bg_sunken);
        painter.rect_stroke(
            badge_rect,
            tokens.radius_sm - 1.0,
            Stroke::new(1.0, tokens.border),
            StrokeKind::Inside,
        );
        painter.galley(
            badge_rect.center() - galley.size() * 0.5,
            galley,
            tokens.text_dim,
        );
        label_right = badge_rect.left() - 6.0;
    }

    // Label, clipped to the row and ellipsized before the badge.
    let label_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 38.0, rect.top()),
        Pos2::new(label_right.max(rect.left() + 39.0), rect.bottom()),
    );
    paint_ellipsized(
        &painter,
        label,
        label_rect,
        FontId::proportional(12.5),
        tokens.text,
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome_frame(
        ctx: &egui::Context,
        state: &mut ChromeState,
        tabs: &[TabInfo],
        active: usize,
        events: Vec<egui::Event>,
    ) -> Vec<Action> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static FRAME: AtomicU64 = AtomicU64::new(0);
        let tokens = Tokens::default();
        let mut actions = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 760.0))),
            predicted_dt: 1.0 / 60.0,
            time: Some(FRAME.fetch_add(1, Ordering::Relaxed) as f64 / 60.0),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |_| {
            actions = show(ctx, state, &tokens, tabs, &[], active, Material::Solid);
        });
        output.textures_delta.clear();
        actions
    }

    fn pointer_button(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        }
    }

    fn apply_tab_moves(tabs: &mut Vec<TabInfo>, actions: &[Action]) {
        for action in actions {
            if let Action::MoveTab(from, to) = action
                && *from < tabs.len()
                && *to < tabs.len()
                && from != to
            {
                let tab = tabs.remove(*from);
                tabs.insert(*to, tab);
            }
        }
    }

    fn drag_tabs_to(
        ctx: &egui::Context,
        tabs: &mut Vec<TabInfo>,
        end: Pos2,
    ) -> (ChromeState, Vec<Action>) {
        let mut state = ChromeState::default();
        let start = Pos2::new(100.0, 20.0);
        let mut actions = chrome_frame(ctx, &mut state, tabs, 0, vec![]);
        for events in [
            vec![egui::Event::PointerMoved(start)],
            vec![pointer_button(start, true)],
            vec![egui::Event::PointerMoved(Pos2::new(200.0, 20.0))],
            vec![egui::Event::PointerMoved(end)],
        ] {
            let frame = chrome_frame(ctx, &mut state, tabs, 0, events);
            apply_tab_moves(tabs, &frame);
            actions.extend(frame);
        }
        (state, actions)
    }

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
    fn drop_index_counts_only_remaining_tabs_left_of_the_pointer() {
        let viewport = Rect::from_min_size(Pos2::new(8.0, 0.0), Vec2::new(600.0, 40.0));
        let slots = layout_tabs(600.0, 4);
        let center = |index: usize| viewport.left() + slots[index].0 + slots[index].1 / 2.0;

        // Dragging the third tab: positions count the three tabs that remain.
        assert_eq!(drop_index(center(0) - 1.0, viewport, &slots, 0.0, 2), 0);
        assert_eq!(drop_index(center(0) + 1.0, viewport, &slots, 0.0, 2), 1);
        assert_eq!(drop_index(center(1) + 1.0, viewport, &slots, 0.0, 2), 2);
        assert_eq!(drop_index(center(3) + 1.0, viewport, &slots, 0.0, 2), 3);

        // Dragging the first tab gives the same insertion positions.
        assert_eq!(drop_index(center(1) + 1.0, viewport, &slots, 0.0, 0), 1);
        assert_eq!(drop_index(center(2) + 1.0, viewport, &slots, 0.0, 0), 2);
    }

    #[test]
    fn drop_index_accounts_for_scroll_and_clamps_outside_the_strip() {
        let viewport = Rect::from_min_size(Pos2::new(8.0, 0.0), Vec2::new(300.0, 40.0));
        let slots = layout_tabs(300.0, 4);
        let step = slots[0].1 + TAB_GAP;

        assert_eq!(
            drop_index(viewport.left() + 1.0, viewport, &slots, 0.0, 0),
            0
        );
        assert_eq!(
            drop_index(viewport.left() + 1.0, viewport, &slots, 186.0, 0),
            1,
            "scroll moves the second tab's center under the pointer"
        );
        assert_eq!(
            drop_index(viewport.left() + 1.0, viewport, &slots, step, 0),
            0
        );
        assert_eq!(
            drop_index(viewport.left() - 500.0, viewport, &slots, 0.0, 1),
            0
        );
        assert_eq!(
            drop_index(viewport.right() + 500.0, viewport, &slots, 0.0, 1),
            3
        );
    }

    #[test]
    fn edge_scroll_ramps_only_inside_the_edge_zones() {
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 40.0));

        assert_eq!(edge_scroll(200.0, viewport, 0.016, 50.0, 1000.0), 50.0);
        assert!(edge_scroll(1.0, viewport, 0.016, 50.0, 1000.0) < 50.0);
        assert!(edge_scroll(399.0, viewport, 0.016, 50.0, 1000.0) > 50.0);
        assert_eq!(
            edge_scroll(0.0, viewport, 0.1, 0.0, 1000.0),
            0.0,
            "the left edge cannot scroll before the first tab"
        );
        assert!(edge_scroll(400.0, viewport, 0.1, 0.0, 1000.0) > 0.0);
        assert_eq!(edge_scroll(400.0, viewport, 0.1, 1000.0, 1000.0), 1000.0);
        assert_eq!(
            edge_scroll(1.0, viewport, 0.0, 50.0, 1000.0),
            50.0,
            "a zero-delta frame does not move"
        );
        assert_eq!(
            edge_scroll(1.0, viewport, 0.1, 50.0, 0.0),
            0.0,
            "a strip without overflow stays at its only scroll position"
        );
    }

    #[test]
    fn ghost_rect_clamps_to_the_strip_and_lifts_above_the_tabs() {
        let viewport = Rect::from_min_size(Pos2::new(10.0, 5.0), Vec2::new(300.0, 35.0));
        let right = ghost_rect(120.0, 900.0, 30.0, viewport, 5.0, 35.0);
        assert_eq!(right.width(), 120.0);
        assert_eq!(right.right(), viewport.right());
        assert_eq!(right.top(), viewport.top() - GHOST_LIFT);

        let left = ghost_rect(120.0, -100.0, 30.0, viewport, 5.0, 35.0);
        assert_eq!(left.left(), viewport.left());

        let narrow = ghost_rect(400.0, 100.0, 10.0, viewport, 5.0, 35.0);
        assert_eq!(narrow.width(), viewport.width());
        assert_eq!(narrow.left(), viewport.left());
    }

    #[test]
    fn layout_text_is_bounded_and_cuts_on_a_character_boundary() {
        let ascii = "a".repeat(MAX_LAYOUT_CHARS + 100);
        assert_eq!(layout_text(&ascii).chars().count(), MAX_LAYOUT_CHARS);

        let wide = "é".repeat(MAX_LAYOUT_CHARS + 100);
        let cut = layout_text(&wide);
        assert_eq!(cut.chars().count(), MAX_LAYOUT_CHARS);
        assert_eq!(cut.len(), MAX_LAYOUT_CHARS * "é".len());

        assert_eq!(layout_text("short"), "short");
    }

    #[test]
    fn dropping_far_below_the_titlebar_is_outside_the_drop_zone() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 40.0));
        assert!(drop_allowed(Pos2::new(100.0, 20.0), bar));
        assert!(drop_allowed(
            Pos2::new(100.0, bar.bottom() + DROP_SLOP),
            bar
        ));
        assert!(!drop_allowed(
            Pos2::new(100.0, bar.bottom() + DROP_SLOP + 1.0),
            bar
        ));
        assert!(drop_allowed(Pos2::new(100.0, bar.top() - DROP_SLOP), bar));
        assert!(!drop_allowed(
            Pos2::new(100.0, bar.top() - DROP_SLOP - 1.0),
            bar
        ));
    }

    #[test]
    fn captions_end_at_window_edge() {
        let rects = caption_rects(1180.0, 0.0, 48.0, 40.0);
        assert_eq!(rects[2].right(), 1180.0);
        assert_eq!(rects[2].width(), 48.0);
        assert_eq!(rects[2].height(), 40.0);
        assert_eq!(rects[0].left(), 1180.0 - 3.0 * 48.0);
        assert_eq!(rects[0].right(), rects[1].left());
        assert_eq!(rects[1].right(), rects[2].left());
    }

    #[test]
    fn tab_close_is_bolder_than_the_shared_icons() {
        let ctx = egui::Context::default();
        let center = Pos2::new(24.0, 24.0);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint_tab_close(ui.painter(), center, Color32::WHITE);
        });
        output.textures_delta.clear();

        let mut strokes = Vec::new();
        let mut bounds: Option<Rect> = None;
        for clipped in &output.shapes {
            if let egui::Shape::LineSegment { stroke, .. } = &clipped.shape {
                strokes.push(stroke.width);
            }
            let shape_bounds = clipped.shape.visual_bounding_rect();
            bounds = Some(bounds.map_or(shape_bounds, |rect| rect.union(shape_bounds)));
        }
        assert_eq!(strokes.len(), 2, "the X is two line segments");
        assert!(
            strokes
                .iter()
                .all(|width| (*width - TAB_CLOSE_STROKE).abs() < 0.001),
            "tab close alone uses the bolder stroke: {strokes:?}"
        );
        let bounds = bounds.expect("tab close painted");
        assert!(bounds.width() >= TAB_CLOSE_GLYPH);
        assert!(
            (bounds.center() - center).length() < 0.01,
            "X stays centered"
        );
    }

    #[test]
    fn caption_hit_targets_and_glyphs_are_windows_standard() {
        let tokens = Tokens::default();
        let rects = caption_rects(1180.0, 0.0, tokens.caption_w, tokens.titlebar_h);
        assert!((46.0..=50.0).contains(&tokens.caption_w));
        for rect in rects {
            assert_eq!(rect.width(), tokens.caption_w);
            assert_eq!(
                rect.height(),
                tokens.titlebar_h,
                "captions span the titlebar"
            );
        }
        assert!((12.0..=14.0).contains(&CAPTION_ICON_SIZE));
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let physical = physical_stroke(ppp, CAPTION_ICON_STROKE) * ppp;
            assert!(
                (physical - physical.round()).abs() < 0.001 && physical >= 1.0,
                "caption stroke must be whole physical pixels at {ppp}x, got {physical}"
            );
        }
    }

    #[test]
    fn controls_have_larger_hit_rects() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(720.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), 2);
        assert_eq!(layout.plus.width(), PLUS_WIDTH);
        assert_eq!(layout.chevron.width(), CHEVRON_WIDTH);
        assert!(layout.plus.height() >= NEW_TAB_HEIGHT);
        assert!(layout.chevron.height() >= NEW_TAB_HEIGHT);
    }

    #[test]
    fn plus_and_chevron_share_the_tab_close_center() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), 3);
        let tab_center = layout.tab_y + layout.tab_height / 2.0;
        assert!((layout.plus.center().y - tab_center).abs() < 0.001);
        assert!((layout.chevron.center().y - tab_center).abs() < 0.001);
        assert!((layout.plus.width() - PLUS_WIDTH).abs() < f32::EPSILON);
        assert!((layout.plus.height() - NEW_TAB_HEIGHT).abs() < f32::EPSILON);
    }

    #[test]
    fn chrome_metrics_scale_to_100_125_and_150_percent() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), 2);
        for (ppp, caption_px, close_stroke_px) in [
            (1.0_f32, 48.0_f32, 1.5_f32),
            (1.25, 60.0, 1.875),
            (1.5, 72.0, 2.25),
        ] {
            assert!((layout.captions[2].width() * ppp - caption_px).abs() < 0.001);
            assert!((TAB_CLOSE_STROKE * ppp - close_stroke_px).abs() < 0.001);
            assert!((layout.plus.center().y - layout.chevron.center().y).abs() < 0.001);
        }
    }

    #[test]
    fn all_themes_share_the_caption_and_control_geometry() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        for theme in illness_core::Theme::all() {
            let tokens = Tokens::from_theme(theme);
            let layout = chrome_layout(bar, &tokens, 4);
            assert_eq!(layout.captions[2].width(), tokens.caption_w);
            assert_eq!(layout.captions[2].height(), tokens.titlebar_h);
            assert_eq!(layout.chevron.width(), CHEVRON_WIDTH);
            assert_eq!(layout.plus.width(), PLUS_WIDTH);
            assert!(layout.captions_left >= layout.chevron.right());
            assert!(layout.chevron.left() >= layout.plus.right());
        }
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
                id: 1,
                title: "A very long PowerShell tab title that must ellipsize".to_owned(),
                profile: "PowerShell".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "Command Prompt".to_owned(),
                profile: "Command Prompt".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
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
        assert_eq!(alpha, 210);
        assert_eq!(
            (red, green, blue),
            (tokens.bg_elev.r(), tokens.bg_elev.g(), tokens.bg_elev.b())
        );
        assert_eq!(
            bar_fill(&tokens, Material::Acrylic).to_srgba_unmultiplied()[3],
            185
        );
        assert_eq!(
            active_tab_fill(&tokens, Material::Mica).to_srgba_unmultiplied()[3],
            210
        );
        assert_eq!(active_tab_fill(&tokens, Material::Solid), tokens.bg);
    }

    #[test]
    fn plus_sits_after_tab_content() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), 2);
        let content = tabs_content_width(&layout.slots);
        let expected = 0.0 + TAB_START_X + content + CONTROL_GAP;
        assert!((layout.plus.left() - expected).abs() < f32::EPSILON);
        assert!((layout.chevron.left() - layout.plus.right()).abs() < f32::EPSILON);
    }

    #[test]
    fn chevron_sits_after_plus_before_the_captions() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), 3);
        assert!((layout.chevron.left() - layout.plus.right()).abs() < f32::EPSILON);
        assert!(layout.chevron.right() + CAPTION_GAP <= layout.captions[0].left());
        assert!(layout.tab_viewport.right() < layout.plus.left());
    }

    #[test]
    fn minimum_width_keeps_tabs_plus_chevron_and_captions_apart() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(720.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), 12);
        assert!(tabs_content_width(&layout.slots) > layout.tab_viewport.width());
        assert!(layout.plus.left() >= layout.tab_viewport.right() + CONTROL_GAP);
        assert!((layout.chevron.left() - layout.plus.right()).abs() < f32::EPSILON);
        assert!(layout.chevron.right() + CAPTION_GAP <= layout.captions[0].left());
        assert!(layout.captions[2].right() <= bar.right());
        assert!(layout.tab_viewport.left() >= bar.left());
    }

    #[test]
    fn plus_pins_before_chevron_when_tabs_overflow() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(720.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), 12);
        let content = tabs_content_width(&layout.slots);
        assert!(content > layout.tab_viewport.width());
        let controls_limit = layout.captions_left - CAPTION_GAP - (PLUS_WIDTH + CHEVRON_WIDTH);
        assert!(
            (layout.plus.left() - controls_limit).abs() < f32::EPSILON,
            "plus pins at the limit"
        );
        assert!(
            layout.plus.left() < bar.left() + TAB_START_X + content + CONTROL_GAP,
            "content overflows past it"
        );
    }

    #[test]
    fn menu_left_edge_anchors_to_chevron() {
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let chevron = Rect::from_min_size(
            Pos2::new(850.0, 5.0),
            Vec2::new(CHEVRON_WIDTH, NEW_TAB_HEIGHT),
        );
        let left = menu_left(chevron, bar);
        assert!((left - chevron.left()).abs() < f32::EPSILON);
        assert!(left >= bar.left());
        assert!(left + MENU_WIDTH <= bar.right());

        let left_chevron = Rect::from_min_size(
            Pos2::new(18.0, 5.0),
            Vec2::new(CHEVRON_WIDTH, NEW_TAB_HEIGHT),
        );
        assert!((menu_left(left_chevron, bar) - left_chevron.left()).abs() < f32::EPSILON);

        let edge_chevron = Rect::from_min_size(
            Pos2::new(0.0, 5.0),
            Vec2::new(CHEVRON_WIDTH, NEW_TAB_HEIGHT),
        );
        assert!((menu_left(edge_chevron, bar) - (bar.left() + 4.0)).abs() < f32::EPSILON);

        let narrow = Rect::from_min_size(Pos2::ZERO, Vec2::new(240.0, 40.0));
        let right_chevron = Rect::from_min_size(
            Pos2::new(204.0, 5.0),
            Vec2::new(CHEVRON_WIDTH, NEW_TAB_HEIGHT),
        );
        let left = menu_left(right_chevron, narrow);
        assert!(
            (left - (narrow.left() + 4.0)).abs() < f32::EPSILON,
            "bar narrower than the menu pins the menu to the left edge"
        );
        assert!((menu_left(chevron, narrow) - (narrow.left() + 4.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn dragging_a_tab_reorders_and_activates_it_on_drop() {
        let ctx = egui::Context::default();
        let tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let mut state = ChromeState::default();
        let start = Pos2::new(100.0, 20.0);
        let end = Pos2::new(600.0, 20.0);

        let mut actions = chrome_frame(&ctx, &mut state, &tabs, 2, vec![]);
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            2,
            vec![egui::Event::PointerMoved(start)],
        ));
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            2,
            vec![pointer_button(start, true)],
        ));
        assert!(actions.is_empty());
        assert!(state.drag.is_none(), "a press alone is not yet a drag");

        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            2,
            vec![egui::Event::PointerMoved(Pos2::new(200.0, 20.0))],
        ));
        assert!(
            state.drag.is_some(),
            "moving past the threshold starts the drag"
        );

        // Final crossing and release share a frame; the previously active tab
        // C sits at index 2, exactly where the dragged tab lands.
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            2,
            vec![egui::Event::PointerMoved(end), pointer_button(end, false)],
        ));

        assert!(state.drag.is_none(), "release ends the drag");
        assert!(
            ctx.has_requested_repaint(),
            "the committed order must be painted without waiting for input"
        );
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::MoveTab(_, _))),
            "the drag emits at least one reorder: {actions:?}"
        );
        assert!(
            actions.contains(&Action::ActivateTab(2)),
            "the dropped tab becomes active even when it lands in the old active slot: {actions:?}"
        );
    }

    #[test]
    fn escape_restores_the_original_order() {
        let ctx = egui::Context::default();
        let mut tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let (mut state, _) = drag_tabs_to(&ctx, &mut tabs, Pos2::new(600.0, 20.0));
        assert_eq!(
            tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );

        let escape = egui::Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        };
        let actions = chrome_frame(&ctx, &mut state, &tabs, 0, vec![escape]);
        apply_tab_moves(&mut tabs, &actions);
        assert!(state.drag.is_none());
        assert!(
            !ctx.input(|i| {
                i.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key: Key::Escape,
                            pressed: true,
                            ..
                        }
                    )
                })
            }),
            "the cancel must not type ESC into the shell"
        );
        assert_eq!(
            tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "Escape cancels every live reorder"
        );
    }

    #[test]
    fn release_outside_the_titlebar_restores_the_original_order() {
        let ctx = egui::Context::default();
        let mut tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let (mut state, _) = drag_tabs_to(&ctx, &mut tabs, Pos2::new(600.0, 20.0));
        let end = Pos2::new(600.0, 400.0);
        let actions = chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            0,
            vec![egui::Event::PointerMoved(end), pointer_button(end, false)],
        );
        assert_eq!(
            actions,
            vec![Action::MoveTab(2, 0)],
            "a cancelled drop restores order without activating"
        );
        apply_tab_moves(&mut tabs, &actions);
        assert!(state.drag.is_none());
        assert_eq!(
            tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "dropping in the terminal is a cancel"
        );
    }

    #[test]
    fn losing_the_pointer_cancels_the_drag() {
        let ctx = egui::Context::default();
        let mut tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let (mut state, _) = drag_tabs_to(&ctx, &mut tabs, Pos2::new(600.0, 20.0));
        let actions = chrome_frame(&ctx, &mut state, &tabs, 0, vec![egui::Event::PointerGone]);
        apply_tab_moves(&mut tabs, &actions);
        assert!(state.drag.is_none());
        assert_eq!(
            tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "a lost mouse-up must not drop a stale tab"
        );
    }

    #[test]
    fn clicking_an_inactive_tab_activates_without_moving() {
        let ctx = egui::Context::default();
        let tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let mut state = ChromeState::default();
        let pos = Pos2::new(100.0, 20.0);
        let mut actions = chrome_frame(&ctx, &mut state, &tabs, 1, vec![]);
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            1,
            vec![egui::Event::PointerMoved(pos)],
        ));
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            1,
            vec![pointer_button(pos, true)],
        ));
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            1,
            vec![pointer_button(pos, false)],
        ));
        assert_eq!(actions, vec![Action::ActivateTab(0)]);
        assert!(state.drag.is_none());
    }

    #[test]
    fn pressing_the_close_glyph_does_not_start_a_drag() {
        let ctx = egui::Context::default();
        let tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), tabs.len());
        let tab = Rect::from_min_size(
            Pos2::new(layout.tab_viewport.left() + layout.slots[0].0, layout.tab_y),
            Vec2::new(layout.slots[0].1, layout.tab_height),
        );
        let close = Pos2::new(tab.right() - 20.0, tab.center().y);
        let mut state = ChromeState::default();
        let mut actions = chrome_frame(&ctx, &mut state, &tabs, 0, vec![]);
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            0,
            vec![egui::Event::PointerMoved(close)],
        ));
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            0,
            vec![pointer_button(close, true)],
        ));
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            0,
            vec![egui::Event::PointerMoved(close + Vec2::new(80.0, 0.0))],
        ));
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            0,
            vec![pointer_button(close + Vec2::new(80.0, 0.0), false)],
        ));
        assert!(state.drag.is_none());
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::MoveTab(_, _) | Action::CloseTab(_))),
            "sliding off the X neither drags nor closes: {actions:?}"
        );
    }

    #[test]
    fn a_long_press_without_movement_does_not_reorder() {
        let ctx = egui::Context::default();
        let tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let pos = Pos2::new(100.0, 20.0);
        let mut state = ChromeState::default();
        let mut actions = chrome_frame(&ctx, &mut state, &tabs, 1, vec![]);
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            1,
            vec![egui::Event::PointerMoved(pos), pointer_button(pos, true)],
        ));
        for _ in 0..60 {
            actions.extend(chrome_frame(&ctx, &mut state, &tabs, 1, vec![]));
        }
        assert!(
            state.drag.is_some(),
            "egui resolves the long press as a drag"
        );
        assert_eq!(state.tab_scroll, 0.0);
        actions.extend(chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            1,
            vec![pointer_button(pos, false)],
        ));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::MoveTab(_, _))),
            "a press without movement never reorders: {actions:?}"
        );
        assert_eq!(actions.last(), Some(&Action::ActivateTab(0)));
    }

    #[test]
    fn a_tab_list_change_cancels_the_drag_without_moving_another_tab() {
        let ctx = egui::Context::default();
        let mut tabs = vec![
            TabInfo {
                id: 1,
                title: "A".to_owned(),
                profile: "A".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            },
            TabInfo {
                id: 2,
                title: "B".to_owned(),
                profile: "B".to_owned(),
                accent: Rgb::new(0xe3, 0xb3, 0x41),
            },
            TabInfo {
                id: 3,
                title: "C".to_owned(),
                profile: "C".to_owned(),
                accent: Rgb::new(0x4c, 0x9a, 0xff),
            },
        ];
        let (mut state, _) = drag_tabs_to(&ctx, &mut tabs, Pos2::new(600.0, 20.0));
        tabs.swap(0, 2);
        let actions = chrome_frame(&ctx, &mut state, &tabs, 0, vec![]);
        assert!(state.drag.is_none());
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::MoveTab(_, _) | Action::ActivateTab(_))),
            "a reindexed strip must not move whatever tab now occupies the slot"
        );
    }

    #[test]
    fn autoscroll_runs_only_while_the_pointer_is_in_the_strip_edge() {
        let ctx = egui::Context::default();
        let mut tabs = (0..12)
            .map(|id| TabInfo {
                id,
                title: format!("tab {id}"),
                profile: "PowerShell".to_owned(),
                accent: Rgb::new(0x7e, 0xe7, 0x87),
            })
            .collect::<Vec<_>>();
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 40.0));
        let layout = chrome_layout(bar, &Tokens::default(), tabs.len());
        let start = Pos2::new(30.0, 20.0);
        let edge = Pos2::new(layout.tab_viewport.right() - 1.0, 20.0);
        let mut state = ChromeState::default();
        let mut actions = chrome_frame(&ctx, &mut state, &tabs, 0, vec![]);
        for events in [
            vec![egui::Event::PointerMoved(start)],
            vec![pointer_button(start, true)],
            vec![egui::Event::PointerMoved(Pos2::new(200.0, 20.0))],
            vec![egui::Event::PointerMoved(edge)],
        ] {
            let frame = chrome_frame(&ctx, &mut state, &tabs, 0, events);
            apply_tab_moves(&mut tabs, &frame);
            actions.extend(frame);
        }
        assert!(state.tab_scroll > 0.0, "the right edge scrolls");
        let scrolled = state.tab_scroll;

        let center = Pos2::new(layout.tab_viewport.center().x, 20.0);
        let frame = chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            0,
            vec![egui::Event::PointerMoved(center)],
        );
        apply_tab_moves(&mut tabs, &frame);
        assert_eq!(state.tab_scroll, scrolled, "the middle does not scroll");

        let below = Pos2::new(layout.tab_viewport.right() - 1.0, 400.0);
        let frame = chrome_frame(
            &ctx,
            &mut state,
            &tabs,
            0,
            vec![egui::Event::PointerMoved(below)],
        );
        apply_tab_moves(&mut tabs, &frame);
        assert_eq!(
            state.tab_scroll, scrolled,
            "the terminal area is outside the drop zone"
        );
        assert!(!actions.is_empty());
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
