use std::time::Duration;

use egui::{
    Align, Align2, Area, Color32, ComboBox, Context, DragValue, FontId, Frame, Id, Key, Layout,
    Margin, Modifiers, Order, Pos2, Rect, Response, RichText, ScrollArea, Sense, Slider, Stroke,
    StrokeKind, TextEdit, Ui, Vec2,
};
use illness_core::config::BackgroundMode;
use illness_core::{Config, CursorKind, Material, Profile, Rgb, Theme};
use std::path::{Path, PathBuf};

use crate::chrome::{Action, TabInfo};
use crate::design::{self, Tokens};
use crate::fonts;
use crate::icons::{self, Icon};

pub const PALETTE_WIDTH: f32 = 640.0;
pub const SEARCH_WIDTH: f32 = 380.0;
pub const SEARCH_HEIGHT: f32 = 40.0;

/// How long a toast stays on screen.
const TOAST_SECS: f64 = 3.0;
/// 0.47 of opaque black, used only for modal backdrops.
const BACKDROP_ALPHA: u8 = 160;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum OverlayKind {
    #[default]
    None,
    Palette,
    Search,
    ContextMenu(Pos2),
}

#[derive(Debug, Clone)]
pub struct OverlayState {
    pub kind: OverlayKind,
    pub query: String,
    pub selected: usize,
    pub search: String,
    pub case_sensitive: bool,
    /// `(current, total)` placeholder for the search bar's match count.
    pub search_index: Option<(usize, usize)>,
    pub toast: Option<String>,
    /// Absolute egui time at which the current toast expires.
    pub toast_deadline: Option<f64>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            kind: OverlayKind::None,
            query: String::new(),
            selected: 0,
            search: String::new(),
            case_sensitive: false,
            search_index: None,
            toast: None,
            toast_deadline: None,
        }
    }
}

impl OverlayState {
    pub fn close(&mut self) {
        self.kind = OverlayKind::None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    pub forward: bool,
    pub case_sensitive: bool,
}

/// Show whichever overlay is active. At most one overlay is drawn, so only it captures the keyboard.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &Context,
    state: &mut OverlayState,
    tokens: &Tokens,
    config: &mut Config,
    profiles: &[Profile],
    tabs: &[TabInfo],
) -> Vec<Action> {
    show_toast(ctx, state, tokens);

    let mut actions = Vec::new();
    if matches!(state.kind, OverlayKind::None) {
        return actions;
    }

    let escape = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
    if handle_escape(state, escape) {
        return actions;
    }

    actions = match state.kind {
        OverlayKind::None => Vec::new(),
        OverlayKind::Palette => show_palette(ctx, state, tokens, config, profiles, tabs),
        OverlayKind::Search => show_search(ctx, state, tokens),
        OverlayKind::ContextMenu(pos) => show_context_menu(ctx, state, tokens, config, pos),
    };
    actions
}

/// Close the overlay when Escape was pressed. Returns `true` if the state changed.
pub fn handle_escape(state: &mut OverlayState, pressed: bool) -> bool {
    if pressed {
        state.close();
        true
    } else {
        false
    }
}

/// Case-insensitive subsequence score. Word starts and consecutive matches are rewarded.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let needle: Vec<char> = query
        .chars()
        .map(|character| character.to_ascii_lowercase())
        .collect();
    if needle.is_empty() {
        return Some(0);
    }

    let haystack: Vec<char> = candidate.chars().collect();
    let mut matched = 0;
    let mut score = 0;
    let mut last: Option<usize> = None;

    for (index, character) in haystack.iter().enumerate() {
        if matched == needle.len() {
            break;
        }
        if character.to_ascii_lowercase() != needle[matched] {
            continue;
        }

        score += 10;
        if index == 0 {
            score += 30;
        }
        if is_word_start(&haystack, index) {
            score += 20;
        }
        if let Some(previous) = last {
            if index == previous + 1 {
                score += 15;
            } else {
                score -= ((index - previous - 1).min(6) as i32) * 2;
            }
        }

        last = Some(index);
        matched += 1;
    }

    (matched == needle.len()).then_some(score)
}

fn is_word_start(characters: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = characters[index - 1];
    matches!(
        previous,
        ' ' | '-' | '_' | '/' | '\\' | '.' | ':' | '(' | '['
    ) || (previous.is_lowercase() && characters[index].is_uppercase())
}

/// Move the palette selection, wrapping at both ends.
pub fn move_selection(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let index = selected.min(len - 1) as isize + delta;
    index.rem_euclid(len as isize) as usize
}

pub fn clamp_font_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(8.0, 40.0)
    } else {
        18.0
    }
}

pub fn clamp_line_height(height: f32) -> f32 {
    if height.is_finite() {
        height.clamp(1.0, 2.0)
    } else {
        1.2
    }
}

pub fn clamp_padding(padding: f32) -> f32 {
    if padding.is_finite() {
        padding.clamp(0.0, 32.0)
    } else {
        8.0
    }
}

pub fn clamp_scrollback(lines: usize) -> usize {
    lines.clamp(1_000, 200_000)
}

/// Apply a theme name chosen in Settings. Returns whether the theme changed.
pub fn select_theme(config: &mut Config, chosen: &str) -> bool {
    let name = Theme::by_name(chosen).name;
    if config.theme == name {
        return false;
    }
    config.theme = name.to_string();
    true
}

/// Shorten a path for a one-line label while keeping both its start (drive or
/// root) and its tail (file name), so long shell paths do not overflow a row.
pub fn elide_path(path: &str, max_chars: usize) -> String {
    let count = path.chars().count();
    if count <= max_chars {
        return path.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_string();
    }

    let keep = max_chars - 1;
    let head_len = keep.div_ceil(2);
    let tail_len = keep - head_len;
    let head: String = path.chars().take(head_len).collect();
    let tail: String = path
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

fn unique_profile_name(profiles: &[Profile]) -> String {
    let base = "New Profile";
    if !profiles
        .iter()
        .any(|profile| profile.name.eq_ignore_ascii_case(base))
    {
        return base.to_string();
    }
    for index in 2..=999 {
        let candidate = format!("{base} {index}");
        if !profiles
            .iter()
            .any(|profile| profile.name.eq_ignore_ascii_case(&candidate))
        {
            return candidate;
        }
    }
    format!("{base} {}", profiles.len() + 1)
}

pub fn search_request(state: &OverlayState, forward: bool) -> SearchRequest {
    SearchRequest {
        query: state.search.clone(),
        forward,
        case_sensitive: state.case_sensitive,
    }
}

/// Translate the search bar's Enter/Shift+Enter keys into a request.
pub fn search_on_enter(state: &OverlayState, enter: bool, shift: bool) -> Option<SearchRequest> {
    enter.then(|| search_request(state, !shift))
}

#[derive(Debug, Clone)]
enum Command {
    NewTab(Option<String>),
    Copy,
    Paste,
    SelectAll,
    Clear,
    Find,
    Settings,
    ToggleMaterial,
    SetTheme(String),
    ActivateTab(usize),
}

#[derive(Debug, Clone)]
struct PaletteItem {
    label: String,
    hint: String,
    icon: Icon,
    command: Command,
}

fn palette_items(query: &str, profiles: &[Profile], tabs: &[TabInfo]) -> Vec<PaletteItem> {
    let mut items = vec![
        PaletteItem {
            label: "New Tab".to_string(),
            hint: "Ctrl+Shift+T".to_string(),
            icon: Icon::Plus,
            command: Command::NewTab(None),
        },
        PaletteItem {
            label: "New PowerShell tab".to_string(),
            hint: "pwsh".to_string(),
            icon: Icon::Terminal,
            command: Command::NewTab(Some("PowerShell".to_string())),
        },
        PaletteItem {
            label: "New Command Prompt tab".to_string(),
            hint: "cmd".to_string(),
            icon: Icon::Terminal,
            command: Command::NewTab(Some("Command Prompt".to_string())),
        },
        PaletteItem {
            label: "Copy".to_string(),
            hint: "Ctrl+Shift+C".to_string(),
            icon: Icon::Copy,
            command: Command::Copy,
        },
        PaletteItem {
            label: "Paste".to_string(),
            hint: "Ctrl+Shift+V".to_string(),
            icon: Icon::Paste,
            command: Command::Paste,
        },
        PaletteItem {
            label: "Find".to_string(),
            hint: "Ctrl+Shift+F".to_string(),
            icon: Icon::Search,
            command: Command::Find,
        },
        PaletteItem {
            label: "Settings".to_string(),
            hint: "Ctrl+,".to_string(),
            icon: Icon::Settings,
            command: Command::Settings,
        },
        PaletteItem {
            label: "Toggle Material".to_string(),
            hint: String::new(),
            icon: Icon::Palette,
            command: Command::ToggleMaterial,
        },
    ];

    for theme in Theme::all() {
        items.push(PaletteItem {
            label: format!("Theme: {}", theme.name),
            hint: theme.accent.hex(),
            icon: Icon::Palette,
            command: Command::SetTheme(theme.name.to_string()),
        });
    }

    for profile in profiles {
        items.push(PaletteItem {
            label: profile.name.clone(),
            hint: profile.program.clone(),
            icon: Icon::Terminal,
            command: Command::NewTab(Some(profile.name.clone())),
        });
    }

    for (index, tab) in tabs.iter().enumerate() {
        if !tab.title.is_empty() {
            items.push(PaletteItem {
                label: format!("Switch: {}", tab.title),
                hint: "Tab".to_string(),
                icon: Icon::Terminal,
                command: Command::ActivateTab(index),
            });
        }
    }

    if query.trim().is_empty() {
        return items;
    }

    let mut scored: Vec<(i32, PaletteItem)> = items
        .into_iter()
        .filter_map(|item| fuzzy_score(query, &item.label).map(|score| (score, item)))
        .collect();
    scored.sort_by_key(|left| std::cmp::Reverse(left.0));
    scored.into_iter().map(|(_, item)| item).collect()
}

fn execute_command(command: Command, config: &mut Config, actions: &mut Vec<Action>) {
    match command {
        Command::NewTab(profile) => actions.push(Action::NewTab(profile)),
        Command::Copy => actions.push(Action::Copy),
        Command::Paste => actions.push(Action::Paste),
        Command::SelectAll => actions.push(Action::SelectAll),
        Command::Clear => actions.push(Action::Clear),
        Command::Find => actions.push(Action::OpenSearch),
        Command::Settings => actions.push(Action::OpenSettings),
        Command::ToggleMaterial => actions.push(Action::ToggleMaterial),
        Command::SetTheme(name) => {
            config.theme = name;
            actions.push(Action::ConfigChanged);
        }
        Command::ActivateTab(index) => actions.push(Action::ActivateTab(index)),
    }
}

fn show_palette(
    ctx: &Context,
    state: &mut OverlayState,
    tokens: &Tokens,
    config: &mut Config,
    profiles: &[Profile],
    tabs: &[TabInfo],
) -> Vec<Action> {
    let screen = ctx.content_rect();
    if backdrop(ctx, "illness_palette_backdrop", BACKDROP_ALPHA) {
        state.close();
        return Vec::new();
    }

    let width = PALETTE_WIDTH.min((screen.width() - 24.0).max(320.0));
    let position = Pos2::new(
        screen.center().x - width * 0.5,
        screen.top() + screen.height() * 0.18,
    );
    let mut chosen = None;

    Area::new(Id::unique("illness_palette"))
        .order(Order::Foreground)
        .fixed_pos(position)
        .sense(Sense::click())
        .show(ctx, |ui| {
            ui.set_width(width);
            Frame::new()
                .fill(tokens.bg_elev)
                .stroke(Stroke::new(1.0, tokens.border))
                .corner_radius(12.0)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.set_width(width - 24.0);
                    Frame::new()
                        .fill(tokens.bg)
                        .stroke(Stroke::new(1.0, tokens.border))
                        .corner_radius(tokens.radius_md)
                        .inner_margin(Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            let output = TextEdit::singleline(&mut state.query)
                                .hint_text("Search commands…")
                                .desired_width(f32::INFINITY)
                                .font(FontId::proportional(16.0))
                                .text_color(tokens.text)
                                .frame(Frame::NONE)
                                .return_key(None)
                                .show(ui);
                            output.response.request_focus();
                        });

                    ui.add_space(8.0);
                    let items = palette_items(&state.query, profiles, tabs);
                    if state.selected >= items.len() {
                        state.selected = items.len().saturating_sub(1);
                    }

                    let mut moved = false;
                    if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                        state.selected = move_selection(state.selected, 1, items.len());
                        moved = true;
                    }
                    if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                        state.selected = move_selection(state.selected, -1, items.len());
                        moved = true;
                    }
                    if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter)) {
                        chosen = items.get(state.selected).map(|item| item.command.clone());
                    }

                    if items.is_empty() {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("No matching commands")
                                .size(12.0)
                                .color(tokens.text_dim),
                        );
                    } else {
                        ScrollArea::vertical()
                            .max_height(34.0 * 10.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for (index, item) in items.iter().enumerate() {
                                    let response = menu_row(
                                        ui,
                                        tokens,
                                        &item.label,
                                        &item.hint,
                                        item.icon,
                                        index == state.selected,
                                    );
                                    if response.clicked() {
                                        chosen = Some(item.command.clone());
                                    }
                                    if response.hovered()
                                        && ctx.input(|input| input.pointer.delta() != Vec2::ZERO)
                                    {
                                        state.selected = index;
                                    }
                                    if moved && index == state.selected {
                                        response.scroll_to_me(Some(Align::Center));
                                    }
                                }
                            });
                    }

                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("↑↓ navigate   Enter run   Esc close")
                            .size(10.5)
                            .color(tokens.text_dim),
                    );
                });
        });

    if let Some(command) = chosen {
        let mut actions = Vec::new();
        execute_command(command, config, &mut actions);
        state.close();
        actions
    } else {
        Vec::new()
    }
}

fn show_search(ctx: &Context, state: &mut OverlayState, tokens: &Tokens) -> Vec<Action> {
    let screen = ctx.content_rect();
    let position = Pos2::new(
        screen.right() - SEARCH_WIDTH - 12.0,
        screen.top() + tokens.titlebar_h + 12.0,
    );
    let mut actions = Vec::new();

    Area::new(Id::unique("illness_search"))
        .order(Order::Foreground)
        .fixed_pos(position)
        .sense(Sense::click())
        .show(ctx, |ui| {
            Frame::new()
                .fill(tokens.bg_elev)
                .stroke(Stroke::new(1.0, tokens.border))
                .corner_radius(tokens.radius_md)
                .inner_margin(Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.set_width(SEARCH_WIDTH - 16.0);
                    ui.set_height(SEARCH_HEIGHT - 8.0);
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let output = TextEdit::singleline(&mut state.search)
                            .hint_text("Search")
                            .desired_width(196.0)
                            .font(FontId::proportional(13.0))
                            .text_color(tokens.text)
                            .frame(Frame::NONE)
                            .return_key(None)
                            .show(ui);
                        output.response.request_focus();

                        if text_button(
                            ui,
                            tokens,
                            "Aa",
                            state.case_sensitive,
                            Vec2::new(26.0, 22.0),
                        )
                        .clicked()
                        {
                            state.case_sensitive = !state.case_sensitive;
                        }

                        let count = state.search_index.map_or_else(
                            || "0/0".to_string(),
                            |(current, total)| format!("{current}/{total}"),
                        );
                        ui.label(RichText::new(count).size(10.5).color(tokens.text_dim));

                        if chevron_button(ui, tokens, true).clicked() {
                            actions.push(Action::Search(search_request(state, false)));
                        }
                        if chevron_button(ui, tokens, false).clicked() {
                            actions.push(Action::Search(search_request(state, true)));
                        }
                        if icon_button(ui, tokens, Icon::Close, 20.0).clicked() {
                            state.close();
                        }
                    });
                });
        });

    if matches!(state.kind, OverlayKind::Search) {
        // Match the shifted shortcut first: unmodified `consume_key` also matches Shift+Enter.
        let shifted = ctx.input_mut(|input| input.consume_key(Modifiers::SHIFT, Key::Enter));
        let plain =
            !shifted && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
        if let Some(request) = search_on_enter(state, shifted || plain, shifted) {
            actions.push(Action::Search(request));
        }
    }

    actions
}

pub fn show_settings_panel(
    ui: &mut Ui,
    tokens: &Tokens,
    theme: &Theme,
    config: &mut Config,
    state: &mut OverlayState,
) -> bool {
    let mut changed = false;

    Frame::new()
        .fill(tokens.bg)
        .inner_margin(Margin::symmetric(24, 18))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());

            let content_width = ui.available_width().min(760.0);
            let pad = ((ui.available_width() - content_width) * 0.5).max(0.0);

            ui.horizontal_top(|ui| {
                if pad > 0.0 {
                    ui.add_space(pad);
                }
                ui.allocate_ui_with_layout(
                    Vec2::new(content_width, ui.available_height()),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.label(
                            RichText::new("Settings")
                                .size(22.0)
                                .strong()
                                .color(tokens.text),
                        );
                        ui.add_space(2.0);
                        muted_label(ui, tokens, "Changes are saved automatically.");
                        ui.add_space(6.0);
                        hairline(ui, tokens);
                        ui.add_space(6.0);

                        ScrollArea::vertical()
                            .id_salt("illness_settings_scroll")
                            .max_height(ui.available_height())
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                changed |= appearance_section(ui, tokens, config, state);
                                changed |= terminal_section(ui, tokens, config);
                                changed |= profiles_section(ui, tokens, config, state);
                                about_section(ui, tokens, theme, state);
                            });
                    },
                );
            });
        });

    changed
}

fn appearance_section(
    ui: &mut Ui,
    tokens: &Tokens,
    config: &mut Config,
    state: &mut OverlayState,
) -> bool {
    let mut changed = false;
    section_title(ui, tokens, "Appearance");

    changed |= setting_row(ui, tokens, "Theme", |ui| {
        let mut chosen = config.theme.clone();
        ComboBox::from_id_salt("illness_settings_theme")
            .width(170.0)
            .height(300.0)
            .selected_text(Theme::by_name(&config.theme).name)
            .popup_style(popup_style(tokens))
            .show_ui(ui, |ui| {
                for theme in Theme::all() {
                    ui.selectable_value(&mut chosen, theme.name.to_string(), theme.name);
                }
            });
        select_theme(config, &chosen)
    });

    changed |= setting_row(ui, tokens, "Material", |ui| {
        let mut chosen = config.material;
        ComboBox::from_id_salt("illness_settings_material")
            .width(170.0)
            .selected_text(material_name(config.material))
            .popup_style(popup_style(tokens))
            .show_ui(ui, |ui| {
                for material in [Material::Mica, Material::Acrylic, Material::Solid] {
                    ui.selectable_value(&mut chosen, material, material_name(material));
                }
            });
        if chosen != config.material {
            config.material = chosen;
            true
        } else {
            false
        }
    });

    changed |= setting_row(ui, tokens, "Background", |ui| {
        let mut chosen = config.background;
        ComboBox::from_id_salt("illness_settings_background")
            .width(170.0)
            .selected_text(background_name(config.background))
            .popup_style(popup_style(tokens))
            .show_ui(ui, |ui| {
                for mode in [
                    BackgroundMode::Desktop,
                    BackgroundMode::Image,
                    BackgroundMode::None,
                ] {
                    ui.selectable_value(&mut chosen, mode, background_name(mode));
                }
            });
        if chosen != config.background {
            config.background = chosen;
            true
        } else {
            false
        }
    });

    if config.background == BackgroundMode::Image {
        changed |= background_image_row(ui, tokens, config, state);
    }

    changed |= setting_row(ui, tokens, "Background opacity", |ui| {
        ui.spacing_mut().slider_width = 150.0;
        let mut opacity = config.background_opacity;
        let changed = ui
            .add(
                Slider::new(&mut opacity, 0.0..=100.0)
                    .fixed_decimals(0)
                    .suffix("%"),
            )
            .changed();
        if changed {
            config.background_opacity = if opacity.is_finite() {
                opacity.clamp(0.0, 100.0)
            } else {
                100.0
            };
        }
        changed
    });
    muted_label(
        ui,
        tokens,
        "0% paints a solid theme background; 100% keeps the material strength.",
    );

    changed |= setting_row(ui, tokens, "Font", |ui| {
        let families = fonts::installed_families();
        let mut chosen = config.font_family.clone();
        ComboBox::from_id_salt("illness_settings_font")
            .width(220.0)
            .height(320.0)
            .selected_text(chosen.clone())
            .popup_style(popup_style(tokens))
            .show_ui(ui, |ui| {
                if !families
                    .iter()
                    .any(|family| family.eq_ignore_ascii_case(&chosen))
                {
                    let current = chosen.clone();
                    ui.selectable_value(&mut chosen, current.clone(), current);
                }
                for family in &families {
                    ui.selectable_value(&mut chosen, family.clone(), family);
                }
            });
        if chosen != config.font_family {
            config.font_family = chosen;
            true
        } else {
            false
        }
    });

    ui.add_space(10.0);
    changed |= setting_row(ui, tokens, "Font size", |ui| {
        let plus = text_button(ui, tokens, "+", false, Vec2::new(26.0, 24.0)).clicked();
        let mut size = config.font_size;
        let dragged = ui
            .add(
                DragValue::new(&mut size)
                    .range(8.0..=40.0)
                    .speed(0.1)
                    .fixed_decimals(0)
                    .suffix(" pt"),
            )
            .changed();
        let minus = text_button(ui, tokens, "-", false, Vec2::new(26.0, 24.0)).clicked();

        let mut step_changed = false;
        if dragged {
            config.font_size = clamp_font_size(size);
            step_changed = true;
        } else if plus && config.font_size < 40.0 {
            config.font_size = clamp_font_size(config.font_size + 1.0);
            step_changed = true;
        }
        if minus && config.font_size > 8.0 {
            config.font_size = clamp_font_size(config.font_size - 1.0);
            step_changed = true;
        }
        step_changed
    });

    changed |= setting_row(ui, tokens, "Line height", |ui| {
        let plus = text_button(ui, tokens, "+", false, Vec2::new(26.0, 24.0)).clicked();
        ui.label(
            RichText::new(format!("{:.1}", config.line_height))
                .size(12.0)
                .color(tokens.text),
        );
        let minus = text_button(ui, tokens, "−", false, Vec2::new(26.0, 24.0)).clicked();

        let mut step_changed = false;
        if plus && config.line_height < 2.0 {
            config.line_height = clamp_line_height(config.line_height + 0.1);
            step_changed = true;
        }
        if minus && config.line_height > 1.0 {
            config.line_height = clamp_line_height(config.line_height - 0.1);
            step_changed = true;
        }
        step_changed
    });

    ui.add_space(10.0);
    muted_label(ui, tokens, "Cursor");
    let cursors = [CursorKind::Block, CursorKind::Beam, CursorKind::Underline];
    let selected_cursor = cursors
        .iter()
        .position(|cursor| *cursor == config.cursor)
        .unwrap_or(0);
    let mut chosen_cursor = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (index, cursor) in cursors.iter().enumerate() {
            if text_button(
                ui,
                tokens,
                cursor_name(*cursor),
                index == selected_cursor,
                Vec2::new(74.0, 26.0),
            )
            .clicked()
            {
                chosen_cursor = Some(*cursor);
            }
        }
    });
    if let Some(cursor) = chosen_cursor
        && cursor != config.cursor
    {
        config.cursor = cursor;
        changed = true;
    }
    muted_label(ui, tokens, "Cursor shape applies to new tabs.");

    changed |= setting_row(ui, tokens, "Blink cursor", |ui| {
        toggle(ui, tokens, &mut config.cursor_blink)
    });
    changed
}

fn background_image_row(
    ui: &mut Ui,
    tokens: &Tokens,
    config: &mut Config,
    state: &mut OverlayState,
) -> bool {
    let mut changed = false;
    let current = config.background_path.trim().to_string();
    let summary = if current.is_empty() {
        "No image selected".to_string()
    } else {
        elide_path(&current, 54)
    };

    ui.horizontal(|ui| {
        let path_label = ui.label(RichText::new(summary).size(11.0).color(tokens.text_dim));
        if !current.is_empty() {
            path_label.on_hover_text(&current);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if text_button(ui, tokens, "Clear", false, Vec2::new(56.0, 24.0)).clicked()
                && !current.is_empty()
            {
                config.background_path.clear();
                changed = true;
            }
            if text_button(ui, tokens, "Browse", false, Vec2::new(66.0, 24.0)).clicked()
                && let Some(source) = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "bmp"])
                    .pick_file()
            {
                match import_background(&source) {
                    Ok(path) => {
                        config.background_path = path.to_string_lossy().into_owned();
                        changed = true;
                    }
                    Err(error) => state.toast = Some(error),
                }
            }
        });
    });

    changed
}

/// Copy a chosen image into `%APPDATA%\illness\backgrounds` under a safe,
/// collision-free name and return the stored path. The source is untouched.
fn import_background(source: &Path) -> Result<PathBuf, String> {
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|extension| matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "bmp"))
        .ok_or_else(|| "Background must be a PNG, JPEG, or BMP image".to_string())?;

    let appdata = std::env::var_os("APPDATA")
        .ok_or_else(|| "Cannot save background: APPDATA is not set".to_string())?;
    let directory = PathBuf::from(appdata).join("illness").join("backgrounds");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create {}: {error}", directory.display()))?;

    let stem = safe_stem(source.file_stem().unwrap_or_default());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);

    for attempt in 0..1_000_u32 {
        let target = directory.join(format!("{stem}-{stamp}-{attempt}.{extension}"));
        if target.exists() {
            continue;
        }
        std::fs::copy(source, &target)
            .map_err(|error| format!("Could not copy background: {error}"))?;
        return Ok(target);
    }

    Err("Could not find a free background file name".to_string())
}

/// Lowercase ASCII stem with path separators and dots removed, so the copied
/// file cannot escape the backgrounds directory or collide on case.
fn safe_stem(stem: &std::ffi::OsStr) -> String {
    let mut cleaned = String::new();
    for character in stem.to_string_lossy().chars() {
        if cleaned.len() >= 40 {
            break;
        }
        if character.is_ascii_alphanumeric() {
            cleaned.push(character.to_ascii_lowercase());
        } else if !cleaned.ends_with('-') {
            cleaned.push('-');
        }
    }
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "background".to_string()
    } else {
        cleaned
    }
}

fn terminal_section(ui: &mut Ui, tokens: &Tokens, config: &mut Config) -> bool {
    let mut changed = false;
    section_title(ui, tokens, "Terminal");

    changed |= setting_row(ui, tokens, "Default shell", |ui| {
        let mut chosen = config.default_profile.clone();
        let selected = if chosen.trim().is_empty() {
            "None".to_string()
        } else {
            chosen.clone()
        };
        ComboBox::from_id_salt("illness_settings_default_shell")
            .width(170.0)
            .selected_text(selected)
            .popup_style(popup_style(tokens))
            .show_ui(ui, |ui| {
                for profile in &config.profiles {
                    if profile.program.trim().is_empty() {
                        continue;
                    }
                    ui.selectable_value(&mut chosen, profile.name.clone(), &profile.name);
                }
            });
        if chosen != config.default_profile {
            config.default_profile = chosen;
            true
        } else {
            false
        }
    });

    changed |= setting_row(ui, tokens, "Scrollback", |ui| {
        let plus = text_button(ui, tokens, "+", false, Vec2::new(26.0, 24.0)).clicked();
        ui.label(
            RichText::new(format!("{}", config.scrollback))
                .size(12.0)
                .color(tokens.text),
        );
        let minus = text_button(ui, tokens, "−", false, Vec2::new(26.0, 24.0)).clicked();

        let mut step_changed = false;
        if plus && config.scrollback < 200_000 {
            config.scrollback = clamp_scrollback(config.scrollback.saturating_add(1_000));
            step_changed = true;
        }
        if minus && config.scrollback > 1_000 {
            config.scrollback = clamp_scrollback(config.scrollback.saturating_sub(1_000));
            step_changed = true;
        }
        step_changed
    });
    muted_label(ui, tokens, "Scrollback changes apply to new tabs.");

    changed |= setting_row(ui, tokens, "Padding", |ui| {
        let plus = text_button(ui, tokens, "+", false, Vec2::new(26.0, 24.0)).clicked();
        ui.label(
            RichText::new(format!("{:.0}", config.padding))
                .size(12.0)
                .color(tokens.text),
        );
        let minus = text_button(ui, tokens, "−", false, Vec2::new(26.0, 24.0)).clicked();

        let mut step_changed = false;
        if plus && config.padding < 32.0 {
            config.padding = clamp_padding(config.padding + 1.0);
            step_changed = true;
        }
        if minus && config.padding > 0.0 {
            config.padding = clamp_padding(config.padding - 1.0);
            step_changed = true;
        }
        step_changed
    });

    changed |= setting_row(ui, tokens, "Copy on select", |ui| {
        toggle(ui, tokens, &mut config.copy_on_select)
    });
    changed
}

fn profiles_section(
    ui: &mut Ui,
    tokens: &Tokens,
    config: &mut Config,
    state: &mut OverlayState,
) -> bool {
    section_title(ui, tokens, "Profiles");
    muted_label(ui, tokens, "Changes apply to new tabs.");
    ui.add_space(4.0);

    let mut changed = false;
    let mut remove = None;
    let count = config.profiles.len();

    for (index, profile) in config.profiles.iter_mut().enumerate() {
        Frame::new()
            .fill(tokens.bg)
            .stroke(Stroke::new(1.0, tokens.border))
            .corner_radius(tokens.radius_sm)
            .inner_margin(Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                ui.horizontal(|ui| {
                    let (swatch, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().rect_filled(
                        swatch.shrink(2.0),
                        2.0,
                        design::color(profile.accent),
                    );

                    let name_width = (ui.available_width() - 74.0).max(80.0);
                    let name = ui.add(
                        TextEdit::singleline(&mut profile.name)
                            .hint_text("Profile name")
                            .desired_width(name_width)
                            .font(FontId::proportional(12.0))
                            .text_color(tokens.text)
                            .margin(Margin::symmetric(6, 3))
                            .frame(edit_frame(tokens)),
                    );
                    if name.changed() {
                        changed = true;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if count > 1 {
                            if text_button(ui, tokens, "Remove", false, Vec2::new(58.0, 22.0))
                                .clicked()
                            {
                                remove = Some(index);
                            }
                        } else {
                            ui.label(
                                RichText::new("Last profile")
                                    .size(10.0)
                                    .color(tokens.text_dim),
                            );
                        }
                    });
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let browse_width = 54.0;
                    let program_width = (ui.available_width() - browse_width - 12.0).max(80.0);
                    let program = ui.add(
                        TextEdit::singleline(&mut profile.program)
                            .hint_text("C:\\Windows\\System32\\cmd.exe")
                            .desired_width(program_width)
                            .font(FontId::proportional(11.5))
                            .text_color(tokens.text)
                            .margin(Margin::symmetric(6, 3))
                            .frame(edit_frame(tokens)),
                    );
                    if program.changed() {
                        changed = true;
                    }
                    let program = program.on_hover_text(&profile.program);
                    if program.lost_focus() && profile.program.trim().is_empty() {
                        state.toast =
                            Some(format!("Profile \"{}\" needs a program path", profile.name));
                    }

                    let browse =
                        text_button(ui, tokens, "Browse", false, Vec2::new(browse_width, 22.0));
                    if browse.clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("Programs", &["exe", "cmd", "bat", "com"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                    {
                        profile.program = path.to_string_lossy().into_owned();
                        changed = true;
                    }
                });

                let args = profile.args.join(" ");
                ui.add_space(2.0);
                let summary = if args.is_empty() {
                    "No arguments".to_string()
                } else {
                    elide_path(&args, 46)
                };
                let args_label = ui.label(
                    RichText::new(format!("Args: {summary}"))
                        .size(10.5)
                        .color(tokens.text_dim),
                );
                if !args.is_empty() {
                    args_label.on_hover_text(args);
                }
            });
        ui.add_space(6.0);
    }

    if let Some(index) = remove
        && config.profiles.len() > 1
    {
        config.profiles.remove(index);
        changed = true;
    }

    if text_button(ui, tokens, "Add profile", false, Vec2::new(92.0, 24.0)).clicked() {
        let name = unique_profile_name(&config.profiles);
        config.profiles.push(Profile {
            name: name.clone(),
            program: String::new(),
            args: Vec::new(),
            accent: Rgb::new(tokens.accent.r(), tokens.accent.g(), tokens.accent.b()),
        });
        state.toast = Some(format!("Added \"{name}\"; choose its program"));
        changed = true;
    }

    changed
}

fn about_section(ui: &mut Ui, tokens: &Tokens, theme: &Theme, state: &mut OverlayState) {
    section_title(ui, tokens, "About");
    ui.label(RichText::new("Illness 0.1.0").size(13.0).color(tokens.text));
    muted_label(ui, tokens, "The Terminal Emulator for the Terminally Ill.");
    muted_label(ui, tokens, format!("Active theme: {}", theme.name));
    muted_label(ui, tokens, "License: MIT OR Apache-2.0");
    ui.add_space(4.0);

    if link_row(
        ui,
        tokens,
        "Alacritty",
        "https://github.com/alacritty/alacritty",
    ) {
        state.toast = Some("Copied Alacritty link".to_string());
    }
    if link_row(ui, tokens, "egui", "https://github.com/emilk/egui") {
        state.toast = Some("Copied egui link".to_string());
    }
    if text_button(ui, tokens, "Copy license", false, Vec2::new(96.0, 24.0)).clicked() {
        ui.ctx().copy_text("MIT OR Apache-2.0".to_string());
        state.toast = Some("Copied license".to_string());
    }
}

fn show_context_menu(
    ctx: &Context,
    state: &mut OverlayState,
    tokens: &Tokens,
    config: &mut Config,
    position: Pos2,
) -> Vec<Action> {
    if backdrop(ctx, "illness_context_backdrop", 0) {
        state.close();
        return Vec::new();
    }

    let screen = ctx.content_rect();
    let width = 220.0;
    let estimated_height = 230.0;
    let x = position
        .x
        .min(screen.right() - width - 8.0)
        .max(screen.left() + 8.0);
    let y = position
        .y
        .min(screen.bottom() - estimated_height - 8.0)
        .max(screen.top() + 8.0);
    let mut chosen = None;

    Area::new(Id::unique("illness_context_menu"))
        .order(Order::Foreground)
        .fixed_pos(Pos2::new(x, y))
        .sense(Sense::click())
        .show(ctx, |ui| {
            Frame::new()
                .fill(tokens.bg_elev)
                .stroke(Stroke::new(1.0, tokens.border))
                .corner_radius(tokens.radius_md)
                .inner_margin(4)
                .show(ui, |ui| {
                    ui.set_width(width - 8.0);
                    let entries = [
                        ("Copy", "Ctrl+Shift+C", Icon::Copy, Command::Copy),
                        ("Paste", "Ctrl+Shift+V", Icon::Paste, Command::Paste),
                        ("Select All", "Ctrl+A", Icon::Copy, Command::SelectAll),
                        ("Clear", "", Icon::Terminal, Command::Clear),
                    ];
                    for (label, hint, icon, command) in entries {
                        if menu_row(ui, tokens, label, hint, icon, false).clicked() {
                            chosen = Some(command);
                        }
                    }

                    ui.add_space(4.0);
                    let (line, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                    ui.painter().rect_filled(line, 0.0, tokens.border);
                    ui.add_space(4.0);

                    let entries = [
                        ("New Tab", "Ctrl+Shift+T", Icon::Plus, Command::NewTab(None)),
                        ("Settings", "Ctrl+,", Icon::Settings, Command::Settings),
                    ];
                    for (label, hint, icon, command) in entries {
                        if menu_row(ui, tokens, label, hint, icon, false).clicked() {
                            chosen = Some(command);
                        }
                    }
                });
        });

    if let Some(command) = chosen {
        let mut actions = Vec::new();
        execute_command(command, config, &mut actions);
        state.close();
        actions
    } else {
        Vec::new()
    }
}

fn backdrop(ctx: &Context, id: &str, alpha: u8) -> bool {
    dim(ctx, id, ctx.content_rect(), alpha)
}

fn dim(ctx: &Context, id: &str, area: Rect, alpha: u8) -> bool {
    if area.width() <= 0.0 || area.height() <= 0.0 {
        return false;
    }
    let mut clicked = false;
    Area::new(Id::unique(id))
        .order(Order::Foreground)
        .fixed_pos(area.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, response) = ui.allocate_exact_size(area.size(), Sense::click());
            ui.painter()
                .rect_filled(rect, 0.0, Color32::from_black_alpha(alpha));
            clicked = response.clicked();
        });
    clicked
}

fn show_toast(ctx: &Context, state: &mut OverlayState, tokens: &Tokens) {
    let Some(message) = state.toast.clone() else {
        state.toast_deadline = None;
        return;
    };

    let now = ctx.input(|input| input.time);
    let deadline = match state.toast_deadline {
        Some(deadline) if now < deadline => deadline,
        Some(_) => {
            state.toast = None;
            state.toast_deadline = None;
            return;
        }
        None => {
            let deadline = now + TOAST_SECS;
            state.toast_deadline = Some(deadline);
            deadline
        }
    };

    ctx.request_repaint_after(Duration::from_secs_f64((deadline - now).max(0.001)));
    Area::new(Id::unique("illness_toast"))
        .order(Order::Tooltip)
        .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -20.0))
        .interactable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(tokens.bg_elev)
                .stroke(Stroke::new(1.0, tokens.border))
                .corner_radius(tokens.radius_md)
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.label(RichText::new(message).size(12.0).color(tokens.text));
                });
        });
}

fn menu_row(
    ui: &mut Ui,
    tokens: &Tokens,
    label: &str,
    hint: &str,
    icon: Icon,
    selected: bool,
) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());

    let fill = if selected {
        tokens.selection
    } else if response.hovered() {
        tokens.hover
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, tokens.radius_sm, fill);
    }

    let painter = ui.painter().with_clip_rect(rect.shrink(1.0));
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 18.0, rect.center().y),
        Vec2::splat(16.0),
    );
    icons::paint_icon(
        &painter,
        icon_rect,
        icon,
        if selected {
            tokens.accent
        } else {
            tokens.text_dim
        },
    );
    painter.text(
        Pos2::new(rect.left() + 36.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        tokens.text,
    );
    if !hint.is_empty() {
        painter.text(
            Pos2::new(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            hint,
            FontId::proportional(10.5),
            tokens.text_dim,
        );
    }
    response
}

fn edit_frame(tokens: &Tokens) -> Frame {
    Frame::new()
        .fill(tokens.bg_sunken)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(tokens.radius_sm)
}

/// ComboBox popups use `window_fill`, which the app sets to transparent for
/// its frameless material backdrop. Give these menus an opaque panel.
fn popup_style(tokens: &Tokens) -> egui::style::StyleModifier {
    let tokens = *tokens;
    egui::style::StyleModifier::new(move |style| {
        style.visuals.window_fill = tokens.bg_elev;
        style.visuals.window_stroke = Stroke::new(1.0, tokens.border);
        style.visuals.window_corner_radius = egui::CornerRadius::same(tokens.radius_md as u8);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(tokens.radius_md as u8);
    })
}

fn setting_row(
    ui: &mut Ui,
    tokens: &Tokens,
    label: &str,
    controls: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(13.0).color(tokens.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            changed = controls(ui);
        });
    });
    changed
}

fn section_title(ui: &mut Ui, tokens: &Tokens, title: &str) {
    ui.add_space(10.0);
    ui.label(
        RichText::new(title)
            .size(11.0)
            .strong()
            .color(tokens.accent),
    );
    ui.add_space(2.0);
}

fn muted_label(ui: &mut Ui, tokens: &Tokens, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(11.0).color(tokens.text_dim));
}

fn hairline(ui: &mut Ui, tokens: &Tokens) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, tokens.border);
}

fn text_button(ui: &mut Ui, tokens: &Tokens, label: &str, selected: bool, size: Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if selected {
        tokens.accent
    } else if response.hovered() {
        tokens.hover
    } else {
        tokens.bg
    };
    ui.painter().rect_filled(rect, tokens.radius_sm, fill);
    if !selected {
        ui.painter().rect_stroke(
            rect,
            tokens.radius_sm,
            Stroke::new(1.0, tokens.border),
            StrokeKind::Inside,
        );
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(12.0),
        if selected { tokens.bg } else { tokens.text },
    );
    response
}

fn toggle(ui: &mut Ui, tokens: &Tokens, value: &mut bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), Sense::click());
    let track = if *value {
        tokens.accent
    } else {
        tokens.selection
    };
    ui.painter().rect_filled(rect, 10.0, track);

    let knob_x = if *value {
        rect.right() - 10.0
    } else {
        rect.left() + 10.0
    };
    ui.painter().circle_filled(
        Pos2::new(knob_x, rect.center().y),
        7.0,
        if *value { tokens.bg } else { tokens.text_dim },
    );

    if response.clicked() {
        *value = !*value;
        true
    } else {
        false
    }
}

fn icon_button(ui: &mut Ui, tokens: &Tokens, icon: Icon, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, tokens.radius_sm, tokens.hover);
    }
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(12.0));
    icons::paint_icon(
        ui.painter(),
        icon_rect,
        icon,
        if response.hovered() {
            tokens.text
        } else {
            tokens.text_dim
        },
    );
    response
}

fn chevron_button(ui: &mut Ui, tokens: &Tokens, up: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, tokens.radius_sm, tokens.hover);
    }

    let color = if response.hovered() {
        tokens.text
    } else {
        tokens.text_dim
    };
    icons::paint_icon(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(12.0)),
        if up { Icon::ArrowUp } else { Icon::ArrowDown },
        color,
    );
    response
}

fn link_row(ui: &mut Ui, tokens: &Tokens, label: &str, url: &str) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, tokens.radius_sm, tokens.hover);
    }
    ui.painter().text(
        Pos2::new(rect.left() + 4.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        if response.hovered() {
            tokens.accent
        } else {
            tokens.text
        },
    );
    ui.painter().text(
        Pos2::new(rect.right() - 4.0, rect.center().y),
        Align2::RIGHT_CENTER,
        "copy",
        FontId::proportional(10.5),
        tokens.text_dim,
    );
    if response.clicked() {
        ui.ctx().copy_text(url.to_string());
        true
    } else {
        false
    }
}

fn material_name(material: Material) -> &'static str {
    match material {
        Material::Mica => "Mica",
        Material::Acrylic => "Acrylic",
        Material::Solid => "Solid",
    }
}

fn background_name(mode: BackgroundMode) -> &'static str {
    match mode {
        BackgroundMode::Desktop => "Desktop",
        BackgroundMode::Image => "Image",
        BackgroundMode::None => "None",
    }
}

fn cursor_name(cursor: CursorKind) -> &'static str {
    match cursor {
        CursorKind::Block => "Block",
        CursorKind::Beam => "Beam",
        CursorKind::Underline => "Underline",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_order() {
        let settings = fuzzy_score("set", "Settings").expect("Settings matches");
        let reset = fuzzy_score("set", "Reset terminal").expect("Reset terminal matches");
        assert!(
            settings > reset,
            "Settings ({settings}) should outrank Reset terminal ({reset})"
        );
    }

    #[test]
    fn fuzzy_case() {
        assert_eq!(
            fuzzy_score("SET", "Settings"),
            fuzzy_score("set", "Settings")
        );
        assert!(fuzzy_score("zz", "Settings").is_none());
        assert!(fuzzy_score("sz", "Settings").is_none());
    }

    #[test]
    fn palette_filters_settings_first() {
        let items = palette_items("set", &[], &[]);
        assert_eq!(
            items.first().map(|item| item.label.as_str()),
            Some("Settings")
        );
    }

    #[test]
    fn palette_nav() {
        let mut selected = 0;
        selected = move_selection(selected, 1, 5);
        selected = move_selection(selected, 1, 5);
        selected = move_selection(selected, -1, 5);
        assert_eq!(selected, 1);
        assert_eq!(move_selection(0, -1, 3), 2);
        assert_eq!(move_selection(7, 1, 3), 0);
        assert_eq!(move_selection(0, 1, 0), 0);
    }

    #[test]
    fn settings_clamp() {
        assert_eq!(clamp_font_size(2.0), 8.0);
        assert_eq!(clamp_font_size(200.0), 40.0);
        assert_eq!(clamp_line_height(0.5), 1.0);
        assert_eq!(clamp_line_height(3.0), 2.0);
        assert_eq!(clamp_scrollback(0), 1_000);
        assert_eq!(clamp_scrollback(999_999), 200_000);
        assert_eq!(clamp_padding(-4.0), 0.0);
        assert_eq!(clamp_padding(80.0), 32.0);
    }

    #[test]
    fn elide_path_keeps_the_start_and_file_name() {
        assert_eq!(elide_path("cmd.exe", 12), "cmd.exe");
        assert_eq!(elide_path("anything", 0), "");
        assert_eq!(elide_path("anything", 1), "…");

        let long = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let short = elide_path(long, 20);
        assert_eq!(short.chars().count(), 20);
        assert!(short.starts_with(r"C:\Program"));
        assert!(short.ends_with("pwsh.exe"));
        assert!(short.contains('…'));

        let unicode = "C:\\Users\\Simulp\\Документы\\shell.exe";
        assert_eq!(elide_path(unicode, 16).chars().count(), 16);
    }

    #[test]
    fn theme_selection_updates_config() {
        let mut config = Config::default();
        assert!(select_theme(&mut config, "Light"));
        assert_eq!(config.theme, "Light");
        assert!(Theme::by_name(&config.theme).is_light());

        assert!(!select_theme(&mut config, "Light"));
        assert!(select_theme(&mut config, "nord"));
        assert_eq!(config.theme, "Nord");
    }

    #[test]
    fn new_profile_names_do_not_collide() {
        assert_eq!(unique_profile_name(&[]), "New Profile");
        let existing = vec![Profile {
            name: "new profile".to_string(),
            program: "pwsh.exe".to_string(),
            args: Vec::new(),
            accent: Rgb::new(0x7e, 0xe7, 0x87),
        }];
        assert_eq!(unique_profile_name(&existing), "New Profile 2");
    }

    #[test]
    fn search_enter() {
        let state = OverlayState {
            search: "needle".to_string(),
            case_sensitive: true,
            ..OverlayState::default()
        };

        assert_eq!(
            search_on_enter(&state, true, false),
            Some(SearchRequest {
                query: "needle".to_string(),
                forward: true,
                case_sensitive: true,
            })
        );
        assert_eq!(
            search_on_enter(&state, true, true).map(|request| request.forward),
            Some(false)
        );
        assert_eq!(search_on_enter(&state, false, false), None);
    }

    #[test]
    fn escape_closes() {
        let mut state = OverlayState {
            kind: OverlayKind::Palette,
            ..OverlayState::default()
        };

        assert!(!handle_escape(&mut state, false));
        assert_eq!(state.kind, OverlayKind::Palette);
        assert!(handle_escape(&mut state, true));
        assert_eq!(state.kind, OverlayKind::None);
    }
}
