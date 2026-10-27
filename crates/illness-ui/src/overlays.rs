use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{
    Align, Align2, Area, Color32, ComboBox, Context, CornerRadius, FontId, Frame, Id, Key, Label,
    Layout, Margin, Modifiers, Order, Pos2, Rect, Response, RichText, ScrollArea, Sense, Slider,
    Stroke, StrokeKind, TextEdit, Ui, Vec2,
};
use illness_core::config::BackgroundMode;
use illness_core::{Config, CursorKind, Material, Profile, Rgb, Theme};

use crate::chrome::{Action, TabInfo};
use crate::design::{self, Tokens};
use crate::fonts;
use crate::icons::{self, Icon};

pub const PALETTE_WIDTH: f32 = 680.0;
pub const SEARCH_WIDTH: f32 = 380.0;
pub const SEARCH_HEIGHT: f32 = 40.0;

/// How long a toast stays on screen.
const TOAST_SECS: f64 = 3.0;
/// 0.47 of opaque black, used only for modal backdrops.
const BACKDROP_ALPHA: u8 = 160;
/// Height of one command row. Roomier than a menu row so the palette breathes.
const PALETTE_ROW_HEIGHT: f32 = 40.0;
const PALETTE_SEARCH_HEIGHT: f32 = 46.0;
const PALETTE_VISIBLE_ROWS: usize = 8;
/// Everything except the scrollable list: margins, search field, gaps,
/// divider and footer. Kept as an explicit sum so the row math stays obvious:
/// top margin 12 + search 46 + gap 10 + gap 10 + divider 1 + gap 8 +
/// footer 20 + gap 4 + bottom margin 12 = 123. The painted outer rect is
/// exactly this tall plus the list, so the 12px insets are included.
const PALETTE_CHROME_HEIGHT: f32 =
    12.0 + PALETTE_SEARCH_HEIGHT + 10.0 + 10.0 + 1.0 + 8.0 + 20.0 + 4.0 + 12.0;
/// Entry animation for overlay surfaces. Short enough to feel instant.
const ENTRY_SECS: f32 = 0.16;
const HOVER_SECS: f32 = 0.12;
const TOGGLE_SECS: f32 = 0.14;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum OverlayKind {
    #[default]
    None,
    Palette,
    Search,
    ContextMenu(Pos2),
    /// Ask before closing the window while more than one tab is open.
    ConfirmClose,
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
    /// Overlay kind observed on the previous frame; used to restart entry animations.
    pub anim_kind: OverlayKind,
    /// Absolute egui time at which the current overlay appeared.
    pub opened_at: f64,
    /// `(profile index, name at the start of the active name edit)`.
    pub profile_name_edit: Option<(usize, String)>,
    /// Bumped every time an overlay closes. Used as a scroll-id salt so the
    /// palette reopens at the top instead of restoring the old scroll offset.
    pub palette_generation: u64,
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
            anim_kind: OverlayKind::None,
            opened_at: 0.0,
            profile_name_edit: None,
            palette_generation: 0,
        }
    }
}

impl OverlayState {
    pub fn close(&mut self) {
        self.kind = OverlayKind::None;
        self.query.clear();
        self.selected = 0;
        self.palette_generation = self.palette_generation.wrapping_add(1);
    }

    /// Show `message` and restart its lifetime. Replacing a toast must clear the
    /// old deadline; otherwise the new message can expire on its first frame.
    pub fn set_toast(&mut self, message: impl Into<String>) {
        self.toast = Some(message.into());
        self.toast_deadline = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    pub forward: bool,
    pub case_sensitive: bool,
}

/// What one Settings pass changed. `retry_material` asks the app to re-apply
/// the requested native material after the user re-selected it while the
/// session was showing its Solid fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettingsOutcome {
    pub changed: bool,
    pub retry_material: bool,
}

/// Show whichever overlay is active. At most one overlay is drawn, so only it captures the keyboard.
///
/// `terminal_tabs` is the number of tabs that own a shell; the Settings tab is
/// not counted. It is passed on every frame so the confirmation card's message
/// and its guard still agree when a background shell exits.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &Context,
    state: &mut OverlayState,
    tokens: &Tokens,
    config: &mut Config,
    profiles: &[Profile],
    tabs: &[TabInfo],
    terminal_tabs: usize,
) -> Vec<Action> {
    show_toast(ctx, state, tokens);

    let now = ctx.input(|input| input.time);
    if state.anim_kind != state.kind {
        state.anim_kind = state.kind;
        state.opened_at = now;
    }

    let mut actions = Vec::new();
    if matches!(state.kind, OverlayKind::None) {
        return actions;
    }

    // Escape cancels every overlay. The confirmation card also accepts Enter
    // as an explicit confirm; Escape is consumed first, so it wins when both
    // keys arrive in the same frame.
    let escape = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
    if matches!(state.kind, OverlayKind::ConfirmClose) {
        let enter = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
        if let Some(confirm) = close_confirmation_choice(escape, enter) {
            state.close();
            return vec![close_confirmation_action(confirm)];
        }
    } else if handle_escape(state, escape) {
        return actions;
    }

    actions = match state.kind {
        OverlayKind::None => Vec::new(),
        OverlayKind::Palette => show_palette(ctx, state, tokens, config, profiles, tabs),
        OverlayKind::Search => show_search(ctx, state, tokens),
        OverlayKind::ContextMenu(pos) => show_context_menu(ctx, state, tokens, config, pos),
        OverlayKind::ConfirmClose => show_confirm_close(ctx, state, tokens, terminal_tabs),
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

/// Draws only the palette rows inside the scroll viewport.
///
/// The row pitch matches [`palette_metrics`] (row height plus the 3px gap), so
/// `show_rows` can virtualize the full list while keeping the reserved height
/// and row clip identical. Returns the clicked command, if any, plus the
/// resulting vertical scroll offset (used by tests).
fn palette_list(
    ui: &mut Ui,
    items: &[PaletteItem],
    tokens: &Tokens,
    state: &mut OverlayState,
    list_height: f32,
    moved: bool,
) -> (Option<Command>, f32) {
    let row_gap = 3.0;
    let row_pitch = PALETTE_ROW_HEIGHT + row_gap;
    // `show_rows` reads the real spacing to compute the row pitch. The palette
    // chrome keeps implicit spacing at zero, so restore it for the footer.
    let previous_gap = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = row_gap;
    let output = ScrollArea::vertical()
        .id_salt(("illness_palette_scroll", state.palette_generation))
        .max_height(list_height)
        .auto_shrink([false, false])
        .show_rows(ui, PALETTE_ROW_HEIGHT, items.len(), |ui, range| {
            if moved {
                // `show_rows` scopes the viewport to the rendered range, so the
                // absolute row rect can be derived even when the selected row
                // lies outside that range. Centering matches the old
                // `Response::scroll_to_me` behavior.
                let top =
                    ui.max_rect().top() + (state.selected as f32 - range.start as f32) * row_pitch;
                let target = Rect::from_min_size(
                    Pos2::new(ui.max_rect().left(), top),
                    Vec2::new(ui.max_rect().width(), PALETTE_ROW_HEIGHT),
                );
                ui.scroll_to_rect(target, Some(Align::Center));
            }

            let mut chosen = None;
            for index in range {
                let item = &items[index];
                let response = menu_row(
                    ui,
                    tokens,
                    &item.label,
                    &item.hint,
                    item.icon,
                    Id::unique(("illness_palette_row", index)),
                    index == state.selected,
                );
                if response.clicked() {
                    chosen = Some(item.command.clone());
                }
                if response.hovered() && ui.ctx().input(|input| input.pointer.delta() != Vec2::ZERO)
                {
                    state.selected = index;
                }
            }
            chosen
        });
    ui.spacing_mut().item_spacing.y = previous_gap;
    (output.inner, output.state.clamped_offset().y)
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
    let (entry, slide) = entry_progress(ctx, state);
    let width = PALETTE_WIDTH.min((screen.width() - 32.0).max(320.0));
    // Resolve the list before sizing so the surface height and the visible
    // rows agree. `items` is only rebuilt below if the TextEdit actually
    // changed the query, avoiding a second full palette build every frame.
    let mut items = palette_items(&state.query, profiles, tabs);
    let sizing_count = items.len();
    let settled_y = (screen.top() + screen.height() * 0.14).max(screen.top() + 12.0);
    let palette_pos = Pos2::new(screen.center().x - width * 0.5, settled_y - slide);
    let available_height = (screen.bottom() - settled_y - 8.0).max(160.0);
    let (list_height, palette_height) = palette_metrics(sizing_count, available_height);
    let palette_rect = Rect::from_min_size(palette_pos, Vec2::new(width, palette_height));

    let mut chosen = None;
    let mut bg_clicked = false;

    // Single unified modal area: fixes the backdrop z-order bug permanently
    Area::new(Id::unique("illness_palette_modal"))
        .order(Order::Foreground)
        .fixed_pos(Pos2::ZERO)
        .interactable(true)
        .fade_in(false)
        .show(ctx, |ui| {
            // 1. Draw modal dark backdrop
            let (bg_rect, bg_resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let backdrop_alpha = (f32::from(BACKDROP_ALPHA) * entry).round() as u8;
            ui.painter()
                .rect_filled(bg_rect, 0.0, Color32::from_black_alpha(backdrop_alpha));

            // 2. Record backdrop clicks; containment is checked against the
            // exact painted rect after the content has been laid out.
            bg_clicked = bg_resp.clicked();

            // 3. Draw the palette surface on exactly `palette_rect`, then lay
            // content out inside its 12px inset. Painting manually keeps the
            // interactive/hit-test rect identical to the visible border.
            ui.scope_builder(egui::UiBuilder::new().max_rect(palette_rect), |ui| {
                ui.set_width(width);
                ui.set_opacity(entry);
                ui.painter().rect_filled(palette_rect, 14.0, tokens.bg_elev);
                ui.painter().rect_stroke(
                    palette_rect,
                    14.0,
                    Stroke::new(1.0, tokens.border),
                    StrokeKind::Inside,
                );
                let mut content = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(palette_rect.shrink(12.0))
                        .layout(Layout::top_down(Align::Min)),
                );
                {
                    let ui = &mut content;
                    ui.set_width(width - 24.0);
                    // Only the explicit add_space calls below contribute
                    // vertical gaps; implicit item spacing would overflow the
                    // fixed PALETTE_CHROME_HEIGHT math.
                    ui.spacing_mut().item_spacing.y = 0.0;

                    // --- Search Bar Header ---
                    let (search_bar_rect, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), PALETTE_SEARCH_HEIGHT),
                        Sense::hover(),
                    );
                    ui.painter()
                        .rect_filled(search_bar_rect, tokens.radius_md, tokens.bg_sunken);
                    ui.painter().rect_stroke(
                        search_bar_rect,
                        tokens.radius_md,
                        Stroke::new(1.0, tokens.border),
                        StrokeKind::Inside,
                    );

                    // Vertically centered search icon
                    let icon_center =
                        Pos2::new(search_bar_rect.left() + 23.0, search_bar_rect.center().y);
                    icons::paint_icon(
                        ui.painter(),
                        Rect::from_center_size(icon_center, Vec2::splat(16.0)),
                        Icon::Search,
                        tokens.text_dim,
                    );

                    // Vertically centered text input
                    let input_rect = Rect::from_min_max(
                        Pos2::new(search_bar_rect.left() + 46.0, search_bar_rect.top() + 8.0),
                        Pos2::new(
                            search_bar_rect.right() - 12.0,
                            search_bar_rect.bottom() - 8.0,
                        ),
                    );
                    let mut edit_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(input_rect)
                            .layout(Layout::left_to_right(Align::Center)),
                    );
                    let output = TextEdit::singleline(&mut state.query)
                        .hint_text("Search commands, profiles, tabs…")
                        .desired_width(input_rect.width())
                        .font(FontId::proportional(14.5))
                        .text_color(tokens.text)
                        .margin(Margin::ZERO)
                        .align(Align2::LEFT_CENTER)
                        .frame(Frame::NONE)
                        .return_key(None)
                        .show(&mut edit_ui);
                    output.response.request_focus();

                    // The TextEdit may have changed the query this frame.
                    // Rebuild rows only when it did, so Enter/arrows match
                    // what the user just typed rather than the pre-edit
                    // list used for the outer height above.
                    if output.response.changed() {
                        items = palette_items(&state.query, profiles, tabs);
                        if items.len() != sizing_count {
                            ctx.request_repaint();
                        }
                    }

                    ui.add_space(10.0);

                    // --- Command List ---
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
                        ui.add_space(16.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("No matching commands")
                                    .size(13.0)
                                    .color(tokens.text_dim),
                            );
                        });
                        ui.add_space(16.0);
                    } else {
                        let (command, _) =
                            palette_list(ui, &items, tokens, state, list_height, moved);
                        if command.is_some() {
                            chosen = command;
                        }
                    }

                    // --- Footer Divider & Key Hints ---
                    ui.add_space(10.0);
                    let (sep, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                    ui.painter().rect_filled(sep, 0.0, tokens.border);
                    ui.add_space(8.0);

                    ui.horizontal_centered(|ui| {
                        ui.add_space(6.0);
                        palette_footer_hint(ui, tokens, "↑↓", "Navigate");
                        ui.add_space(18.0);
                        palette_footer_hint(ui, tokens, "↵", "Select");
                        ui.add_space(18.0);
                        palette_footer_hint(ui, tokens, "Esc", "Dismiss");
                    });
                    ui.add_space(4.0);
                }
            });
        });

    if bg_clicked
        && ctx
            .input(|input| input.pointer.interact_pos())
            .is_none_or(|position| !palette_rect.contains(position))
    {
        state.close();
        return Vec::new();
    }

    if let Some(command) = chosen {
        let mut actions = Vec::new();
        execute_command(command, config, &mut actions);
        state.close();
        actions
    } else {
        Vec::new()
    }
}

/// Height of the scrollable list and of the whole palette surface.
/// Rows are capped at [`PALETTE_VISIBLE_ROWS`] and by the viewport so the
/// footer always stays on screen in short windows.
fn palette_metrics(rows: usize, available_height: f32) -> (f32, f32) {
    let rows = rows.min(PALETTE_VISIBLE_ROWS);
    let desired_list = if rows == 0 {
        52.0
    } else {
        rows as f32 * PALETTE_ROW_HEIGHT + rows.saturating_sub(1) as f32 * 3.0
    };
    let max_list = (available_height - PALETTE_CHROME_HEIGHT).max(PALETTE_ROW_HEIGHT);
    let list = desired_list.min(max_list);
    (list, (PALETTE_CHROME_HEIGHT + list).min(available_height))
}

/// Entry animation for overlay surfaces. Returns `(opacity, slide_offset)` where
/// the offset is 8 px at first paint and settles at zero.
fn entry_progress(ctx: &Context, state: &OverlayState) -> (f32, f32) {
    let now = ctx.input(|input| input.time);
    let elapsed = (now - state.opened_at).max(0.0) as f32;
    let linear = (elapsed / ENTRY_SECS).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - linear).powi(3);
    if linear < 1.0 {
        ctx.request_repaint();
    }
    (eased, (1.0 - eased) * 8.0)
}

fn palette_footer_hint(ui: &mut Ui, tokens: &Tokens, key: &str, label: &str) {
    let font = FontId::proportional(10.5);
    let label_font = FontId::proportional(11.0);
    let key_galley = ui
        .painter()
        .layout_no_wrap(key.to_owned(), font.clone(), tokens.text_dim);
    let label_galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), label_font, tokens.text_dim);
    let badge_size = key_galley.size() + Vec2::new(10.0, 4.0);
    let total = Vec2::new(
        badge_size.x + 6.0 + label_galley.size().x,
        badge_size.y.max(label_galley.size().y).max(20.0),
    );
    let (rect, _) = ui.allocate_exact_size(total, Sense::hover());
    let badge_rect = Rect::from_center_size(
        Pos2::new(rect.left() + badge_size.x * 0.5, rect.center().y),
        badge_size,
    );

    ui.painter().rect_filled(badge_rect, 4.0, tokens.bg_sunken);
    ui.painter().rect_stroke(
        badge_rect,
        4.0,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );
    ui.painter().galley(
        badge_rect.center() - key_galley.size() * 0.5,
        key_galley,
        tokens.text_dim,
    );
    ui.painter().galley(
        Pos2::new(
            badge_rect.right() + 6.0,
            rect.center().y - label_galley.size().y * 0.5,
        ),
        label_galley,
        tokens.text_dim,
    );
}

fn menu_row(
    ui: &mut Ui,
    tokens: &Tokens,
    label: &str,
    hint: &str,
    icon: Icon,
    row_id: Id,
    selected: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), PALETTE_ROW_HEIGHT),
        Sense::click(),
    );

    let painter = ui.painter().with_clip_rect(rect);
    let selected_t = ui
        .ctx()
        .animate_bool_with_time(row_id.with("selected"), selected, 0.10);
    let hovered_t = ui.ctx().animate_bool_with_time(
        row_id.with("hovered"),
        response.hovered() && !selected,
        HOVER_SECS,
    );

    if selected_t > 0.0 {
        painter.rect_filled(
            rect,
            tokens.radius_sm,
            design::fade(tokens.hover, selected_t),
        );
        painter.rect_stroke(
            rect,
            tokens.radius_sm,
            Stroke::new(1.0, design::fade(tokens.border, selected_t)),
            StrokeKind::Inside,
        );

        // Accent indicator
        let indicator = Rect::from_min_size(
            Pos2::new(rect.left() + 2.0, rect.top() + 7.0),
            Vec2::new(3.0, rect.height() - 14.0),
        );
        painter.rect_filled(indicator, 1.5, design::fade(tokens.accent, selected_t));
    } else if hovered_t > 0.0 {
        painter.rect_filled(
            rect,
            tokens.radius_sm,
            design::fade(tokens.bg_sunken, hovered_t),
        );
    }

    // Content is inset by a pixel so the selected row's inside stroke is not
    // clipped; the background stroke above uses the full rect instead.
    let content = ui.painter().with_clip_rect(rect.shrink(1.0));

    // Centered icon
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 22.0, rect.center().y),
        Vec2::splat(16.0),
    );
    icons::paint_icon(
        &content,
        icon_rect,
        icon,
        design::mix(tokens.text_dim, tokens.accent, selected_t),
    );

    // Centered label
    content.text(
        Pos2::new(rect.left() + 46.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.5),
        tokens.text,
    );

    // Centered shortcut badge. Long profile paths are elided so the badge
    // cannot grow wider than the row.
    if !hint.is_empty() {
        let hint = elide_path(hint, 44);
        let font = FontId::proportional(10.5);
        let galley = content.layout_no_wrap(hint, font.clone(), tokens.text_dim);
        let badge_size = galley.size() + Vec2::new(10.0, 4.0);
        let badge_rect = Rect::from_center_size(
            Pos2::new(rect.right() - badge_size.x * 0.5 - 10.0, rect.center().y),
            badge_size,
        );

        content.rect_filled(badge_rect, tokens.radius_sm - 1.0, tokens.bg_sunken);
        content.rect_stroke(
            badge_rect,
            tokens.radius_sm - 1.0,
            Stroke::new(1.0, tokens.border),
            StrokeKind::Inside,
        );
        content.galley(
            badge_rect.center() - galley.size() * 0.5,
            galley,
            tokens.text_dim,
        );
    }

    response
}

fn show_search(ctx: &Context, state: &mut OverlayState, tokens: &Tokens) -> Vec<Action> {
    let screen = ctx.content_rect();
    let (entry, slide) = entry_progress(ctx, state);
    let position = Pos2::new(
        screen.right() - SEARCH_WIDTH - 12.0,
        screen.top() + tokens.titlebar_h + 12.0 - slide,
    );
    let mut actions = Vec::new();

    Area::new(Id::unique("illness_search"))
        .order(Order::Foreground)
        .fixed_pos(position)
        .sense(Sense::click())
        .fade_in(false)
        .show(ctx, |ui| {
            ui.set_opacity(entry);
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
                            .margin(Margin::ZERO)
                            .align(Align2::LEFT_CENTER)
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
    backdrop_present: bool,
    material_fallback: bool,
) -> SettingsOutcome {
    let mut changed = false;
    let mut retry_material = false;

    Frame::new()
        .fill(tokens.bg)
        .inner_margin(Margin::symmetric(28, 20))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());

            let content_width = ui.available_width().min(780.0);
            let pad = ((ui.available_width() - content_width) * 0.5).max(0.0);

            ui.horizontal_top(|ui| {
                if pad > 0.0 {
                    ui.add_space(pad);
                }
                ui.allocate_ui_with_layout(
                    Vec2::new(content_width, ui.available_height()),
                    Layout::top_down(Align::Min),
                    |ui| {
                        // Header
                        ui.label(
                            RichText::new("Settings")
                                .size(24.0)
                                .strong()
                                .color(tokens.text),
                        );
                        ui.add_space(2.0);
                        muted_label(
                            ui,
                            tokens,
                            "Changes apply instantly and save shortly after you stop editing.",
                        );
                        ui.add_space(14.0);

                        // Scroll Area with breathing room for the scrollbar
                        ScrollArea::vertical()
                            .id_salt("illness_settings_scroll")
                            .max_height(ui.available_height())
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 6.0;
                                ui.add_space(2.0);

                                changed |= appearance_section(
                                    ui,
                                    tokens,
                                    config,
                                    state,
                                    backdrop_present,
                                    material_fallback,
                                    &mut retry_material,
                                );
                                changed |= terminal_section(ui, tokens, config);
                                changed |= profiles_section(ui, tokens, config, state);
                                about_section(ui, tokens, theme, state);
                                ui.add_space(48.0);
                            });
                    },
                );
            });
        });

    SettingsOutcome {
        changed,
        retry_material,
    }
}

fn appearance_section(
    ui: &mut Ui,
    tokens: &Tokens,
    config: &mut Config,
    state: &mut OverlayState,
    backdrop_present: bool,
    material_fallback: bool,
    retry_material: &mut bool,
) -> bool {
    let mut changed = false;
    section_title(ui, tokens, "Appearance");

    changed |= setting_card(
        ui,
        tokens,
        "Theme",
        Some("Application color palette across windows and tabs"),
        |ui| {
            let mut chosen = config.theme.clone();
            let current = Theme::by_name(&config.theme);

            ComboBox::from_id_salt("illness_settings_theme")
                .width(190.0)
                .height(300.0)
                .selected_text(current.name)
                .popup_style(popup_style(tokens))
                .show_ui(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for theme in Theme::all() {
                        let is_active = chosen.eq_ignore_ascii_case(theme.name);
                        let (row_rect, row_resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 28.0),
                            Sense::click(),
                        );

                        if is_active {
                            ui.painter()
                                .rect_filled(row_rect, tokens.radius_sm, tokens.hover);
                            ui.painter().rect_stroke(
                                row_rect,
                                tokens.radius_sm,
                                Stroke::new(1.0, tokens.border),
                                StrokeKind::Inside,
                            );
                        } else if row_resp.hovered() {
                            ui.painter()
                                .rect_filled(row_rect, tokens.radius_sm, tokens.bg_sunken);
                        }

                        // Theme Accent Dot Swatch
                        let dot_center = Pos2::new(row_rect.left() + 14.0, row_rect.center().y);
                        ui.painter()
                            .circle_filled(dot_center, 4.5, design::color(theme.accent));

                        // Label
                        ui.painter().text(
                            Pos2::new(row_rect.left() + 28.0, row_rect.center().y),
                            Align2::LEFT_CENTER,
                            theme.name,
                            FontId::proportional(12.5),
                            if is_active {
                                tokens.text
                            } else {
                                tokens.text_dim
                            },
                        );

                        if row_resp.clicked() {
                            chosen = theme.name.to_string();
                        }
                    }
                });
            select_theme(config, &chosen)
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Material",
        Some("Compositor backdrop transparency effect"),
        |ui| {
            let mut chosen = config.material;
            let mut picked = false;
            ComboBox::from_id_salt("illness_settings_material")
                .width(170.0)
                .selected_text(material_label(config.material, material_fallback))
                .popup_style(popup_style(tokens))
                .show_ui(ui, |ui| {
                    for material in [Material::Mica, Material::Acrylic, Material::Solid] {
                        if ui
                            .selectable_label(config.material == material, material_name(material))
                            .clicked()
                        {
                            chosen = material;
                            picked = true;
                        }
                    }
                });
            if !picked {
                return false;
            }
            match material_pick(config.material, material_fallback, chosen) {
                MaterialPick::Changed => {
                    config.material = chosen;
                    true
                }
                MaterialPick::Retry => {
                    *retry_material = true;
                    false
                }
                MaterialPick::None => false,
            }
        },
    );

    let background_enabled = background_controls_enabled(config.material);
    changed |= setting_card(
        ui,
        tokens,
        "Background",
        Some("Source image or wallpaper for translucent materials"),
        |ui| {
            ui.add_enabled_ui(background_enabled, |ui| {
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
                    return true;
                }
                false
            })
            .inner
        },
    );

    if config.background == BackgroundMode::Image {
        changed |= ui
            .add_enabled_ui(background_enabled, |ui| {
                background_image_row(ui, tokens, config, state)
            })
            .inner;
    }

    let opacity_enabled =
        background_opacity_enabled(config.material, config.background, backdrop_present);
    changed |= setting_card(
        ui,
        tokens,
        "Background Opacity",
        Some("0% paints solid theme background; 100% keeps material strength"),
        |ui| {
            ui.add_enabled_ui(opacity_enabled, |ui| {
                ui.spacing_mut().slider_width = 150.0;
                let mut opacity = config.background_opacity;
                let response = ui.add(
                    Slider::new(&mut opacity, 0.0..=100.0)
                        .fixed_decimals(0)
                        .suffix("%"),
                );
                if response.changed() {
                    config.background_opacity = if opacity.is_finite() {
                        opacity.clamp(0.0, 100.0)
                    } else {
                        100.0
                    };
                    true
                } else {
                    false
                }
            })
            .inner
        },
    );

    if material_fallback && config.material != Material::Solid {
        info_note(
            ui,
            tokens,
            "Native materials are unavailable, so the window is Solid. Opacity and images apply when available.",
        );
    } else if !background_enabled {
        info_note(
            ui,
            tokens,
            "Solid paints an opaque background. Choose Mica or Acrylic to use desktop wallpapers.",
        );
    } else if !opacity_enabled {
        let message =
            if config.material == Material::Mica && config.background == BackgroundMode::Image {
                "The image could not be loaded, so Mica paints only its native backdrop."
            } else {
                "Mica with no background ignores opacity. Choose Desktop or Image, or use Acrylic."
            };
        info_note(ui, tokens, message);
    }

    changed |= setting_card(
        ui,
        tokens,
        "Font",
        Some("Monospace font family used for terminal rendering"),
        |ui| {
            let families = fonts::installed_families();
            let mut chosen = config.font_family.clone();
            ComboBox::from_id_salt("illness_settings_font")
                .width(220.0)
                .height(300.0)
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
                    for family in families {
                        ui.selectable_value(&mut chosen, family.clone(), family);
                    }
                });
            if chosen != config.font_family {
                config.font_family = chosen;
                true
            } else {
                false
            }
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Font Size",
        Some("Terminal character size in points"),
        |ui| {
            stepper_widget(
                ui,
                tokens,
                "font_size",
                &format!("{:.0} pt", config.font_size),
                |dir| {
                    if dir < 0 {
                        if config.font_size > 8.0 {
                            config.font_size = clamp_font_size(config.font_size - 1.0);
                            return true;
                        }
                    } else if config.font_size < 40.0 {
                        config.font_size = clamp_font_size(config.font_size + 1.0);
                        return true;
                    }
                    false
                },
            )
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Line Height",
        Some("Line height multiplier (1.0 - 2.0)"),
        |ui| {
            stepper_widget(
                ui,
                tokens,
                "line_height",
                &format!("{:.1}", config.line_height),
                |dir| {
                    if dir < 0 {
                        if config.line_height > 1.0 {
                            config.line_height = clamp_line_height(config.line_height - 0.1);
                            return true;
                        }
                    } else if config.line_height < 2.0 {
                        config.line_height = clamp_line_height(config.line_height + 0.1);
                        return true;
                    }
                    false
                },
            )
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Cursor Style",
        Some("Cursor shape applied to new sessions"),
        |ui| {
            let cursors = [
                (CursorKind::Block, "Block"),
                (CursorKind::Beam, "Beam"),
                (CursorKind::Underline, "Underline"),
            ];
            segmented_cursor_widget(ui, tokens, &mut config.cursor, &cursors)
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Blink Cursor",
        Some("Terminal cursor smoothly blinks when focused"),
        |ui| toggle(ui, tokens, &mut config.cursor_blink),
    );

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
        elide_path(&current, 40)
    };

    setting_card(ui, tokens, "Custom Image", Some(&summary), |ui| {
        if text_button(ui, tokens, "Browse", false, Vec2::new(68.0, 26.0)).clicked()
            && let Some(source) = rfd::FileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "bmp"])
                .pick_file()
        {
            match import_background(&source) {
                Ok(path) => {
                    let previous = config.background_path.clone();
                    config.background_path = path.to_string_lossy().into_owned();
                    cleanup_imported_background(&previous, &path);
                    changed = true;
                }
                Err(error) => state.set_toast(error),
            }
        }
        if !current.is_empty()
            && text_button(ui, tokens, "Clear", false, Vec2::new(56.0, 26.0)).clicked()
        {
            let previous = config.background_path.clone();
            config.background_path.clear();
            cleanup_imported_background(&previous, Path::new(""));
            changed = true;
        }
        changed
    })
}

pub fn background_controls_enabled(material: Material) -> bool {
    material != Material::Solid
}

fn background_opacity_enabled(
    material: Material,
    mode: BackgroundMode,
    backdrop_present: bool,
) -> bool {
    match material {
        Material::Solid => false,
        Material::Acrylic => true,
        Material::Mica => match mode {
            BackgroundMode::None => false,
            BackgroundMode::Desktop => true,
            BackgroundMode::Image => backdrop_present,
        },
    }
}

fn import_background(source: &Path) -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA")
        .ok_or_else(|| "Cannot save background: APPDATA is not set".to_string())?;
    let directory = PathBuf::from(appdata).join("illness").join("backgrounds");
    import_background_into(source, &directory)
}

fn import_background_into(source: &Path, directory: &Path) -> Result<PathBuf, String> {
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|extension| matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "bmp"))
        .ok_or_else(|| "Background must be a PNG, JPEG, or BMP image".to_string())?;

    std::fs::create_dir_all(directory)
        .map_err(|error| format!("Could not create {}: {error}", directory.display()))?;

    let stem = safe_stem(source.file_stem().unwrap_or_default());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut source_file = std::fs::File::open(source)
        .map_err(|error| format!("Could not read background: {error}"))?;

    for attempt in 0..1_000_u32 {
        let target = directory.join(format!("{stem}-{stamp}-{attempt}.{extension}"));
        let copied = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target);
        match copied {
            Ok(mut target_file) => {
                if let Err(error) = io::copy(&mut source_file, &mut target_file)
                    .and_then(|_| target_file.sync_all())
                {
                    drop(target_file);
                    let _ = std::fs::remove_file(&target);
                    return Err(format!("Could not copy background: {error}"));
                }
                return Ok(target);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Could not copy background: {error}")),
        }
    }

    Err("Could not find a free background file name".to_string())
}

fn cleanup_imported_background(previous: &str, replacement: &Path) {
    let Ok(appdata) = std::env::var("APPDATA") else {
        return;
    };
    let directory = PathBuf::from(appdata).join("illness").join("backgrounds");
    remove_imported_background_in(Path::new(previous.trim()), replacement, &directory);
}

fn remove_imported_background_in(previous: &Path, replacement: &Path, directory: &Path) -> bool {
    if previous.as_os_str().is_empty() || same_path(previous, replacement) {
        return false;
    }
    let Some(parent) = previous.parent() else {
        return false;
    };
    if !same_path(parent, directory) {
        return false;
    }
    std::fs::remove_file(previous).is_ok()
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

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

    changed |= setting_card(
        ui,
        tokens,
        "Default Shell",
        Some("Profile launched when clicking the new tab button"),
        |ui| {
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
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Scrollback Lines",
        Some("History lines preserved in terminal memory"),
        |ui| {
            stepper_widget(
                ui,
                tokens,
                "scrollback",
                &format!("{}", config.scrollback),
                |dir| {
                    if dir < 0 {
                        if config.scrollback > 1_000 {
                            config.scrollback =
                                clamp_scrollback(config.scrollback.saturating_sub(1_000));
                            return true;
                        }
                    } else if config.scrollback < 200_000 {
                        config.scrollback =
                            clamp_scrollback(config.scrollback.saturating_add(1_000));
                        return true;
                    }
                    false
                },
            )
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Window Padding",
        Some("Internal padding between window border and text grid"),
        |ui| {
            stepper_widget(
                ui,
                tokens,
                "padding",
                &format!("{:.0} px", config.padding),
                |dir| {
                    if dir < 0 {
                        if config.padding > 0.0 {
                            config.padding = clamp_padding(config.padding - 1.0);
                            return true;
                        }
                    } else if config.padding < 32.0 {
                        config.padding = clamp_padding(config.padding + 1.0);
                        return true;
                    }
                    false
                },
            )
        },
    );

    changed |= setting_card(
        ui,
        tokens,
        "Inherit Working Folder",
        Some("New tabs start in the active tab's current directory"),
        |ui| toggle(ui, tokens, &mut config.inherit_cwd),
    );

    changed |= setting_card(
        ui,
        tokens,
        "Copy on Select",
        Some("Automatically copy highlighted text to system clipboard"),
        |ui| toggle(ui, tokens, &mut config.copy_on_select),
    );

    changed |= setting_card(
        ui,
        tokens,
        "Shift+Enter Newline",
        Some(
            "Sends ESC CR so TUIs insert a line instead of submitting; off makes Shift+Enter behave like Enter",
        ),
        |ui| toggle(ui, tokens, &mut config.shift_enter_newline),
    );

    changed |= setting_card(
        ui,
        tokens,
        "Confirm on Close",
        Some("Ask before closing the window while multiple tabs are open"),
        |ui| toggle(ui, tokens, &mut config.confirm_close_with_tabs),
    );

    changed
}

fn profiles_section(
    ui: &mut Ui,
    tokens: &Tokens,
    config: &mut Config,
    state: &mut OverlayState,
) -> bool {
    section_title(ui, tokens, "Profiles");
    info_note(
        ui,
        tokens,
        "Changes apply to new tabs. Renaming a profile automatically updates the default shell.",
    );
    ui.add_space(4.0);

    let mut changed = false;
    let mut remove = None;
    let mut renames: Vec<(String, String, String)> = Vec::new();
    let count = config.profiles.len();
    let existing_names: Vec<String> = config
        .profiles
        .iter()
        .map(|profile| profile.name.clone())
        .collect();

    for (index, profile) in config.profiles.iter_mut().enumerate() {
        let committed_name = profile.name.clone();
        let mut name = committed_name.clone();

        Frame::new()
            .fill(tokens.bg_elev)
            .stroke(Stroke::new(1.0, tokens.border))
            .corner_radius(tokens.radius_md)
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                // Top Header Row: Vertically Centered
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;

                    // Accent Dot. Painted after the name so it can follow the
                    // text's line box: a singleline TextEdit's glyphs sit below
                    // the row centre, which makes a row-centred dot look high.
                    let (swatch, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());

                    // In-place Profile Name
                    let name_width = (ui.available_width() - 36.0).max(100.0);
                    let name_output = TextEdit::singleline(&mut name)
                        .hint_text("Profile name")
                        .desired_width(name_width)
                        .font(FontId::proportional(14.0))
                        .text_color(tokens.text)
                        .margin(Margin::ZERO)
                        .align(Align2::LEFT_CENTER)
                        .frame(Frame::NONE)
                        .show(ui);
                    let dot_y = name_output.galley_pos.y + name_output.galley.size().y * 0.5;
                    ui.painter().circle_filled(
                        Pos2::new(swatch.center().x, dot_y),
                        5.0,
                        design::color(profile.accent),
                    );
                    let name_response = name_output.response;

                    if name_response.gained_focus() {
                        state.profile_name_edit = Some((index, committed_name.clone()));
                    }

                    if name_response.changed() {
                        let start_name = match &state.profile_name_edit {
                            Some((edit_index, start)) if *edit_index == index => start.clone(),
                            _ => committed_name.clone(),
                        };
                        let trimmed = name.trim();
                        let duplicate = !trimmed.is_empty()
                            && existing_names.iter().enumerate().any(|(other, existing)| {
                                other != index && existing.trim().eq_ignore_ascii_case(trimmed)
                            });
                        if duplicate {
                            state.set_toast(format!("Profile name \"{trimmed}\" is already used"));
                        } else {
                            let new_name = name.clone();
                            profile.name = new_name.clone();
                            renames.push((committed_name.clone(), start_name, new_name));
                            changed = true;
                        }
                    }

                    if name_response.lost_focus()
                        && let Some((edit_index, start_name)) = state.profile_name_edit.clone()
                        && edit_index == index
                    {
                        if profile.name.trim().is_empty() {
                            profile.name = start_name;
                            state.set_toast("Profile name cannot be empty".to_string());
                            changed = true;
                        }
                        state.profile_name_edit = None;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if count > 1 {
                            let del_btn = icon_button(ui, tokens, Icon::Close, 20.0);
                            if del_btn.clicked() {
                                remove = Some(index);
                            }
                            del_btn.on_hover_text("Remove profile");
                        }
                    });
                });

                ui.add_space(8.0);

                // Executable Row: Unified 30px height and centered layout
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;

                    let browse_width = 74.0;
                    let input_width = (ui.available_width() - browse_width - 8.0).max(120.0);

                    let program = ui.add_sized(
                        [input_width, 30.0],
                        TextEdit::singleline(&mut profile.program)
                            .hint_text("Command or executable path")
                            .font(FontId::monospace(11.5))
                            .text_color(tokens.text)
                            .margin(Margin::symmetric(16, 0))
                            .align(Align2::LEFT_CENTER)
                            .frame(edit_frame(tokens)),
                    );

                    if program.changed() {
                        changed = true;
                    }
                    let program = program.on_hover_text(&profile.program);
                    if program.lost_focus() && profile.program.trim().is_empty() {
                        state.set_toast(format!(
                            "Profile \"{}\" needs a program path",
                            profile.name
                        ));
                    }

                    let browse =
                        text_button(ui, tokens, "Browse", false, Vec2::new(browse_width, 30.0));
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

                // Args preview tag
                let args = profile.args.join(" ");
                if !args.is_empty() {
                    ui.add_space(6.0);
                    ui.horizontal_centered(|ui| {
                        ui.label(
                            RichText::new("Arguments:")
                                .size(11.0)
                                .color(tokens.text_dim),
                        );
                        let summary = elide_path(&args, 46);
                        let badge = ui.label(
                            RichText::new(summary)
                                .size(11.0)
                                .monospace()
                                .color(tokens.text),
                        );
                        badge.on_hover_text(args);
                    });
                }
            });
        ui.add_space(6.0);
    }

    for (previous, start, new) in renames {
        config.note_profile_rename(&previous, &new);
        config.note_profile_rename(&start, &new);
        if !start.eq_ignore_ascii_case(&new) {
            config.hide_profile_name(&start);
        }
    }

    if let Some(index) = remove
        && config.profiles.len() > 1
    {
        let removed = config.profiles.remove(index);
        if config.default_profile.eq_ignore_ascii_case(&removed.name) {
            config.default_profile = config
                .profiles
                .first()
                .map(|profile| profile.name.clone())
                .unwrap_or_default();
        }
        config.hide_profile_name(&removed.name);
        state.profile_name_edit = None;
        changed = true;
    }

    ui.add_space(4.0);
    if text_button(ui, tokens, "+ Add Profile", false, Vec2::new(124.0, 30.0)).clicked() {
        let name = unique_profile_name(&config.profiles);
        config.profiles.push(Profile {
            name: name.clone(),
            program: String::new(),
            args: Vec::new(),
            accent: Rgb::new(tokens.accent.r(), tokens.accent.g(), tokens.accent.b()),
        });
        state.set_toast(format!("Added \"{name}\"; set its executable path"));
        changed = true;
    }

    changed
}

/// Decode the bundled logo once and cache it in egui's temporary data store.
/// Keeping the handle there avoids re-uploading a 256px texture each frame.
fn app_logo(ctx: &Context) -> Option<egui::TextureHandle> {
    const LOGO_PNG: &[u8] = include_bytes!("../../../assets/illness.png");
    let id = Id::unique("illness_app_logo_texture");
    if let Some(cached) = ctx.data(|data| data.get_temp::<Option<egui::TextureHandle>>(id)) {
        return cached;
    }

    let decoded = image::load_from_memory(LOGO_PNG).ok().map(|image| {
        let image = image.to_rgba8();
        let (width, height) = image.dimensions();
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], image.as_raw())
    });
    let handle = decoded
        .map(|pixels| ctx.load_texture("illness_app_logo", pixels, egui::TextureOptions::LINEAR));
    // Cache the negative result too: a corrupt asset must not re-decode every frame.
    ctx.data_mut(|data| data.insert_temp(id, handle.clone()));
    handle
}

/// Edge length of the About card logo. It shrinks with the card width so the
/// text column always keeps room, but never grows past the compact cap.
const ABOUT_LOGO_MAX: f32 = 44.0;
const ABOUT_LOGO_MIN: f32 = 28.0;
/// Shared action-strip metrics: every About button is the same height, and the
/// link rows share the same copy glyph so the strip reads as one row.
const ABOUT_BUTTON_HEIGHT: f32 = 26.0;
const ABOUT_BUTTON_MIN_WIDTH: f32 = 96.0;
const ABOUT_BUTTON_PAD: f32 = 12.0;
const ABOUT_ICON_SIZE: f32 = 12.0;
const ABOUT_ICON_GAP: f32 = 6.0;

fn about_logo_size(available_width: f32) -> f32 {
    (available_width * 0.22).clamp(ABOUT_LOGO_MIN, ABOUT_LOGO_MAX)
}

/// Widest label plus symmetric padding, floored at the historic button width
/// so the strip keeps its weight even when every label is short.
fn about_button_width(content_widths: &[f32]) -> f32 {
    let widest = content_widths.iter().copied().fold(0.0_f32, f32::max);
    (widest + 2.0 * ABOUT_BUTTON_PAD).max(ABOUT_BUTTON_MIN_WIDTH)
}

/// Width of `text` in the card's proportional face, without laying out a widget.
fn about_text_width(ui: &Ui, text: &str, size: f32) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(size), Color32::WHITE)
        .size()
        .x
}

fn about_section(ui: &mut Ui, tokens: &Tokens, theme: &Theme, state: &mut OverlayState) {
    section_title(ui, tokens, "About");

    Frame::new()
        .fill(tokens.bg_elev)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(tokens.radius_md)
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Header: App Icon + Name + Version + Tagline
            let logo_size = about_logo_size(ui.available_width());
            let version = concat!("v", env!("CARGO_PKG_VERSION"));
            let version_font = FontId::proportional(10.5);
            let v_galley =
                ui.painter()
                    .layout_no_wrap(version.to_owned(), version_font.clone(), tokens.text);
            let badge_size = v_galley.size() + Vec2::new(10.0, 4.0);

            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;

                // Real app logo, decoded once and cached for the panel's lifetime.
                let (icon_box, _) = ui.allocate_exact_size(Vec2::splat(logo_size), Sense::hover());
                if let Some(logo) = app_logo(ui.ctx()) {
                    ui.painter().image(
                        logo.id(),
                        icon_box,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    ui.painter()
                        .rect_filled(icon_box, tokens.radius_md, tokens.bg_sunken);
                    ui.painter().rect_stroke(
                        icon_box,
                        tokens.radius_md,
                        Stroke::new(1.0, tokens.accent),
                        StrokeKind::Inside,
                    );
                    icons::paint_icon(
                        ui.painter(),
                        Rect::from_center_size(icon_box.center(), Vec2::splat(logo_size * 0.5)),
                        Icon::Terminal,
                        tokens.accent,
                    );
                }

                ui.vertical(|ui| {
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = ABOUT_ICON_GAP;

                        // Truncate the name instead of wrapping when a long
                        // version string or a narrow card squeezes the row.
                        ui.scope(|ui| {
                            let reserved = badge_size.x + ABOUT_ICON_GAP;
                            ui.set_max_width((ui.available_width() - reserved).max(24.0));
                            ui.add(
                                Label::new(
                                    RichText::new("Illness Terminal")
                                        .size(15.0)
                                        .strong()
                                        .color(tokens.text),
                                )
                                .truncate(),
                            );
                        });

                        // Version Pill Badge: sized from the galley so a longer
                        // version grows the pill without reflowing the row.
                        let (badge_rect, _) = ui.allocate_exact_size(badge_size, Sense::hover());
                        ui.painter().rect_filled(badge_rect, 4.0, tokens.bg_sunken);
                        ui.painter().rect_stroke(
                            badge_rect,
                            4.0,
                            Stroke::new(1.0, tokens.border),
                            StrokeKind::Inside,
                        );
                        ui.painter().text(
                            badge_rect.center(),
                            Align2::CENTER_CENTER,
                            version,
                            version_font,
                            tokens.text,
                        );
                    });
                    ui.label(
                        RichText::new("The Terminal Emulator for the Terminally Ill.")
                            .size(11.5)
                            .color(tokens.text_dim),
                    );
                });
            });

            ui.add_space(10.0);
            let (divider, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
            ui.painter().rect_filled(divider, 0.0, tokens.border);
            ui.add_space(10.0);

            // Information Grid
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(
                    RichText::new("Active Theme:")
                        .size(11.5)
                        .color(tokens.text_dim),
                );
                ui.label(RichText::new(theme.name).size(11.5).color(tokens.text));
                ui.add_space(10.0);
                ui.label(RichText::new("License:").size(11.5).color(tokens.text_dim));
                ui.label(
                    RichText::new("MIT OR Apache-2.0")
                        .size(11.5)
                        .color(tokens.text),
                );
            });

            ui.add_space(10.0);

            // Action strip: equal widths, all copy the value to the clipboard.
            let label_size = 11.5;
            let alacritty_width =
                ABOUT_ICON_SIZE + ABOUT_ICON_GAP + about_text_width(ui, "Alacritty", label_size);
            let egui_width =
                ABOUT_ICON_SIZE + ABOUT_ICON_GAP + about_text_width(ui, "egui", label_size);
            let license_width = about_text_width(ui, "Copy License", label_size);
            let button_size = Vec2::new(
                about_button_width(&[alacritty_width, egui_width, license_width]),
                ABOUT_BUTTON_HEIGHT,
            );

            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = tokens.gap_sm;

                if link_row(
                    ui,
                    tokens,
                    "Alacritty",
                    "https://github.com/alacritty/alacritty",
                    button_size,
                ) {
                    state.set_toast("Copied Alacritty link");
                }
                if link_row(
                    ui,
                    tokens,
                    "egui",
                    "https://github.com/emilk/egui",
                    button_size,
                ) {
                    state.set_toast("Copied egui link");
                }
                if text_button(ui, tokens, "Copy License", false, button_size).clicked() {
                    ui.ctx().copy_text("MIT OR Apache-2.0".to_string());
                    state.set_toast("Copied license to clipboard");
                }
            });
        });
}

/// Map the confirmation card's choice to the app's close handling.
fn close_confirmation_action(confirm: bool) -> Action {
    if confirm {
        Action::ConfirmCloseWindow
    } else {
        Action::CancelCloseWindow
    }
}

/// Keyboard choice for the confirmation card.
///
/// Escape cancels and Enter confirms. Escape wins when both keys arrive in the
/// same frame. `None` leaves the card open; no button is focused, so a keypress
/// is an explicit choice rather than a default.
fn close_confirmation_choice(escape: bool, enter: bool) -> Option<bool> {
    if escape {
        Some(false)
    } else if enter {
        Some(true)
    } else {
        None
    }
}

/// Message for the confirmation card, pluralized from the terminal-tab count
/// measured for this frame. A shell can exit while the card is open, so the
/// text is rebuilt every frame instead of being captured when it opened.
fn close_confirmation_message(tab_count: usize) -> String {
    if tab_count == 1 {
        "1 tab is open. Closing the window ends the session.".to_string()
    } else {
        format!("{tab_count} tabs are open. Closing the window ends every session.")
    }
}

/// Layout constants for the close-confirmation card. They are shared by
/// [`confirm_close_card_height`] and [`show_confirm_close`], so the height
/// computed for the card always matches the content laid out inside it.
const CONFIRM_CARD_WIDTH: f32 = 480.0;
const CONFIRM_MARGIN: f32 = 26.0;
const CONFIRM_BUTTON_HEIGHT: f32 = 34.0;

fn confirm_close_card_height(title_height: f32, message_height: f32) -> f32 {
    CONFIRM_MARGIN * 2.0 + title_height + 10.0 + message_height + 24.0 + CONFIRM_BUTTON_HEIGHT
}

/// Confirmation card shown when the window close is held back for more than
/// one open terminal tab. Escape cancels; only the primary button (or Enter)
/// reports a confirmed close.
fn show_confirm_close(
    ctx: &Context,
    state: &mut OverlayState,
    tokens: &Tokens,
    tab_count: usize,
) -> Vec<Action> {
    let screen = ctx.content_rect();
    let (entry, slide) = entry_progress(ctx, state);
    let width = CONFIRM_CARD_WIDTH.min((screen.width() - 32.0).max(300.0));
    let content_width = width - CONFIRM_MARGIN * 2.0;
    let message = close_confirmation_message(tab_count);

    let title_height = ctx.fonts_mut(|fonts| {
        fonts
            .layout(
                "Close window?".to_owned(),
                FontId::proportional(tokens.font_title + 2.0),
                tokens.text,
                content_width,
            )
            .size()
            .y
    });
    let message_height = ctx.fonts_mut(|fonts| {
        fonts
            .layout(
                message.clone(),
                FontId::proportional(tokens.font_body),
                tokens.text_dim,
                content_width,
            )
            .size()
            .y
    });

    let height = confirm_close_card_height(title_height, message_height).ceil();
    let card = Rect::from_center_size(
        Pos2::new(screen.center().x, screen.center().y - slide),
        Vec2::new(width, height),
    );

    let mut choice = None;
    let mut backdrop_clicked = false;

    Area::new(Id::unique("illness_confirm_close_modal"))
        .order(Order::Foreground)
        .fixed_pos(Pos2::ZERO)
        .interactable(true)
        .fade_in(false)
        .show(ctx, |ui| {
            let (bg_rect, bg_response) = ui.allocate_exact_size(screen.size(), Sense::click());
            let backdrop_alpha = (f32::from(BACKDROP_ALPHA) * entry).round() as u8;
            ui.painter()
                .rect_filled(bg_rect, 0.0, Color32::from_black_alpha(backdrop_alpha));
            backdrop_clicked = bg_response.clicked();

            ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
                ui.set_width(width);
                ui.set_opacity(entry);

                ui.painter().rect_filled(card, 14.0, tokens.bg_elev);
                ui.painter().rect_stroke(
                    card,
                    14.0,
                    Stroke::new(1.0, tokens.border),
                    StrokeKind::Inside,
                );

                let mut content = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(card.shrink(CONFIRM_MARGIN))
                        .layout(Layout::top_down(Align::Min)),
                );
                {
                    let ui = &mut content;
                    ui.set_width(content_width);
                    ui.spacing_mut().item_spacing.y = 0.0;

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;

                        let (dot_rect, _) =
                            ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        ui.painter()
                            .circle_filled(dot_rect.center(), 5.5, tokens.danger);

                        ui.label(
                            RichText::new("Close window?")
                                .size(tokens.font_title + 2.0)
                                .strong()
                                .color(tokens.text),
                        );
                    });

                    ui.add_space(8.0);

                    ui.label(
                        RichText::new(message)
                            .size(tokens.font_body)
                            .color(tokens.text_dim),
                    );

                    ui.add_space(24.0);

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if danger_button(
                            ui,
                            tokens,
                            "Close Window",
                            Vec2::new(124.0, CONFIRM_BUTTON_HEIGHT),
                        )
                        .clicked()
                        {
                            choice = Some(true);
                        }

                        ui.add_space(tokens.gap_sm);

                        if text_button(
                            ui,
                            tokens,
                            "Cancel",
                            false,
                            Vec2::new(84.0, CONFIRM_BUTTON_HEIGHT),
                        )
                        .clicked()
                        {
                            choice = Some(false);
                        }
                    });
                }
            });
        });

    if let Some(confirm) = choice {
        state.close();
        return vec![close_confirmation_action(confirm)];
    }

    let outside = ctx
        .input(|input| input.pointer.interact_pos())
        .is_none_or(|pos| !card.contains(pos));
    if backdrop_clicked && outside {
        state.close();
    }

    Vec::new()
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
    let estimated_height = 296.0;
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
                        if menu_row(
                            ui,
                            tokens,
                            label,
                            hint,
                            icon,
                            Id::unique(("illness_context_row", label)),
                            false,
                        )
                        .clicked()
                        {
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
                        if menu_row(
                            ui,
                            tokens,
                            label,
                            hint,
                            icon,
                            Id::unique(("illness_context_row", label)),
                            false,
                        )
                        .clicked()
                        {
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
    let elapsed = (now - (deadline - TOAST_SECS)).max(0.0);
    let remaining = (deadline - now).max(0.0);
    let fade_in = (elapsed / 0.12).clamp(0.0, 1.0);
    let fade_out = (remaining / 0.18).clamp(0.0, 1.0);
    let opacity = fade_in.min(fade_out) as f32;
    if opacity < 1.0 {
        ctx.request_repaint_after(Duration::from_millis(16));
    }
    Area::new(Id::unique("illness_toast"))
        .order(Order::Tooltip)
        .anchor(
            Align2::CENTER_BOTTOM,
            Vec2::new(0.0, -20.0 - (1.0 - opacity) * 4.0),
        )
        .interactable(false)
        .fade_in(false)
        .show(ctx, |ui| {
            ui.set_opacity(opacity);
            Frame::new()
                .fill(tokens.bg_elev)
                .stroke(Stroke::new(1.0, tokens.border))
                .corner_radius(tokens.radius_md)
                .inner_margin(Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.label(RichText::new(message).size(12.5).color(tokens.text));
                });
        });
}

fn edit_frame(tokens: &Tokens) -> Frame {
    Frame::new()
        .fill(tokens.bg_sunken)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(tokens.radius_sm)
}

fn setting_card<R>(
    ui: &mut Ui,
    tokens: &Tokens,
    title: &str,
    description: Option<&str>,
    controls: impl FnOnce(&mut Ui) -> R,
) -> R {
    let desired_width = ui.available_width();
    let min_height = if description.is_some() { 56.0 } else { 46.0 };

    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(desired_width, min_height), Sense::hover());

    let hover = ui.ctx().animate_bool_with_time(
        Id::unique(("illness_setting_card", title)),
        response.hovered(),
        HOVER_SECS,
    );
    let bg = design::mix(tokens.bg_elev, tokens.hover, hover);
    ui.painter().rect_filled(rect, tokens.radius_md, bg);
    ui.painter().rect_stroke(
        rect,
        tokens.radius_md,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );

    let controls_width = 340.0_f32.min(rect.width() * 0.55);
    let text_left = rect.left() + 16.0;
    let text_right = (rect.right() - controls_width - 8.0).max(text_left);

    if let Some(desc) = description {
        let title_pos = Pos2::new(text_left, rect.center().y - 10.0);
        let desc_pos = Pos2::new(text_left, rect.center().y + 10.0);

        ui.painter().text(
            title_pos,
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(tokens.font_body),
            tokens.text,
        );
        ui.painter().text(
            desc_pos,
            Align2::LEFT_CENTER,
            desc,
            FontId::proportional(tokens.font_small),
            tokens.text_dim,
        );
    } else {
        ui.painter().text(
            Pos2::new(text_left, rect.center().y),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(tokens.font_body),
            tokens.text,
        );
    }

    let mut right_child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                Pos2::new(text_right, rect.top()),
                Pos2::new(rect.right() - 14.0, rect.bottom()),
            ))
            .layout(Layout::right_to_left(Align::Center)),
    );

    controls(&mut right_child)
}

fn stepper_widget(
    ui: &mut Ui,
    tokens: &Tokens,
    salt: &str,
    label: &str,
    mut on_step: impl FnMut(i32) -> bool,
) -> bool {
    let mut changed = false;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(128.0, 30.0), Sense::hover());

    ui.painter()
        .rect_filled(rect, tokens.radius_sm, tokens.bg_sunken);
    ui.painter().rect_stroke(
        rect,
        tokens.radius_sm,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );

    let btn_w = 30.0;
    let minus_rect = Rect::from_min_size(rect.min, Vec2::new(btn_w, rect.height()));
    let plus_rect = Rect::from_min_size(
        Pos2::new(rect.right() - btn_w, rect.top()),
        Vec2::new(btn_w, rect.height()),
    );

    let minus_resp = ui.interact(
        minus_rect,
        Id::unique(("illness_stepper", salt, "minus")),
        Sense::click(),
    );
    if minus_resp.hovered() {
        ui.painter().rect_filled(
            minus_rect,
            CornerRadius {
                nw: tokens.radius_sm as u8,
                sw: tokens.radius_sm as u8,
                ne: 0,
                se: 0,
            },
            tokens.hover,
        );
    }
    ui.painter().text(
        minus_rect.center(),
        Align2::CENTER_CENTER,
        "−",
        FontId::proportional(15.0),
        if minus_resp.hovered() {
            tokens.text
        } else {
            tokens.text_dim
        },
    );
    if minus_resp.clicked() {
        changed |= on_step(-1);
    }

    let plus_resp = ui.interact(
        plus_rect,
        Id::unique(("illness_stepper", salt, "plus")),
        Sense::click(),
    );
    if plus_resp.hovered() {
        ui.painter().rect_filled(
            plus_rect,
            CornerRadius {
                ne: tokens.radius_sm as u8,
                se: tokens.radius_sm as u8,
                nw: 0,
                sw: 0,
            },
            tokens.hover,
        );
    }
    ui.painter().text(
        plus_rect.center(),
        Align2::CENTER_CENTER,
        "+",
        FontId::proportional(15.0),
        if plus_resp.hovered() {
            tokens.text
        } else {
            tokens.text_dim
        },
    );
    if plus_resp.clicked() {
        changed |= on_step(1);
    }

    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(tokens.font_small),
        tokens.text,
    );

    changed
}

fn segmented_cursor_widget(
    ui: &mut Ui,
    tokens: &Tokens,
    current: &mut CursorKind,
    options: &[(CursorKind, &str)],
) -> bool {
    let mut changed = false;
    let height = 30.0;
    let opt_width = 74.0;
    let total_width = opt_width * options.len() as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total_width, height), Sense::hover());

    ui.painter()
        .rect_filled(rect, tokens.radius_sm, tokens.bg_sunken);
    ui.painter().rect_stroke(
        rect,
        tokens.radius_sm,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );

    for (i, (kind, label)) in options.iter().enumerate() {
        let opt_rect = Rect::from_min_size(
            Pos2::new(rect.left() + i as f32 * opt_width, rect.top()),
            Vec2::new(opt_width, height),
        );
        let resp = ui.interact(
            opt_rect,
            Id::unique(("illness_cursor_option", i)),
            Sense::click(),
        );
        let selected = *current == *kind;

        if selected {
            let pill = opt_rect.shrink(2.0);
            ui.painter()
                .rect_filled(pill, tokens.radius_sm - 1.0, tokens.bg_elev);
            ui.painter().rect_stroke(
                pill,
                tokens.radius_sm - 1.0,
                Stroke::new(1.0, tokens.border),
                StrokeKind::Inside,
            );
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(opt_rect.shrink(2.0), tokens.radius_sm - 1.0, tokens.hover);
        }

        ui.painter().text(
            opt_rect.center(),
            Align2::CENTER_CENTER,
            *label,
            FontId::proportional(tokens.font_small),
            if selected {
                tokens.text
            } else {
                tokens.text_dim
            },
        );

        if resp.clicked() && *current != *kind {
            *current = *kind;
            changed = true;
        }
    }

    changed
}

fn popup_style(tokens: &Tokens) -> egui::style::StyleModifier {
    let tokens = *tokens;
    egui::style::StyleModifier::new(move |style| {
        style.visuals.window_fill = tokens.bg_elev;
        style.visuals.window_stroke = Stroke::new(1.0, tokens.border);
        style.visuals.window_corner_radius = egui::CornerRadius::same(tokens.radius_md as u8);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(tokens.radius_md as u8);
        style.spacing.menu_margin = Margin::same(4);
    })
}

fn section_title(ui: &mut Ui, tokens: &Tokens, title: &str) {
    ui.add_space(14.0);
    ui.label(
        RichText::new(title)
            .size(tokens.font_title)
            .strong()
            .color(tokens.text),
    );
    ui.add_space(2.0);
}

fn info_note(ui: &mut Ui, tokens: &Tokens, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(tokens.text_dim));
    ui.add_space(2.0);
}

fn muted_label(ui: &mut Ui, tokens: &Tokens, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(11.0).color(tokens.text_dim));
}

fn text_button(ui: &mut Ui, tokens: &Tokens, label: &str, selected: bool, size: Vec2) -> Response {
    let enabled = ui.is_enabled();
    let (rect, response) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let fill = if !enabled {
        tokens.bg
    } else if selected {
        tokens.accent
    } else {
        let hover = ui
            .ctx()
            .animate_bool_with_time(response.id, response.hovered(), HOVER_SECS);
        design::mix(tokens.bg_elev, tokens.hover, hover)
    };
    ui.painter().rect_filled(rect, tokens.radius_sm, fill);
    ui.painter().rect_stroke(
        rect,
        tokens.radius_sm,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );
    let text_color = if !enabled {
        tokens.text_dim
    } else if selected {
        tokens.bg
    } else {
        tokens.text
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.5),
        text_color,
    );
    response
}

fn danger_button(ui: &mut Ui, tokens: &Tokens, label: &str, size: Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), HOVER_SECS);

    let fill = design::fade(tokens.danger, 0.88 + 0.12 * hover);
    ui.painter().rect_filled(rect, tokens.radius_sm, fill);
    ui.painter().rect_stroke(
        rect,
        tokens.radius_sm,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );

    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.5),
        Color32::WHITE,
    );

    response
}

fn toggle(ui: &mut Ui, tokens: &Tokens, value: &mut bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(38.0, 20.0), Sense::click());
    let radius = rect.height() * 0.5;
    let on = ui
        .ctx()
        .animate_bool_with_time(response.id, *value, TOGGLE_SECS);
    let track = design::mix(
        if response.hovered() {
            design::mix(tokens.bg_sunken, tokens.hover, 0.45)
        } else {
            tokens.bg_sunken
        },
        tokens.accent,
        on,
    );
    ui.painter().rect_filled(rect, radius, track);
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );

    let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), on);
    ui.painter().circle_filled(
        Pos2::new(knob_x, rect.center().y),
        radius - 3.0,
        design::mix(
            if response.hovered() {
                tokens.text
            } else {
                tokens.text_dim
            },
            Color32::WHITE,
            on,
        ),
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
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), HOVER_SECS);
    if hover > 0.0 {
        ui.painter()
            .rect_filled(rect, tokens.radius_sm, design::fade(tokens.hover, hover));
    }
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(12.0));
    icons::paint_icon(
        ui.painter(),
        icon_rect,
        icon,
        design::mix(tokens.text_dim, tokens.text, hover),
    );
    response
}

fn chevron_button(ui: &mut Ui, tokens: &Tokens, up: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), HOVER_SECS);
    if hover > 0.0 {
        ui.painter()
            .rect_filled(rect, tokens.radius_sm, design::fade(tokens.hover, hover));
    }

    icons::paint_icon(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(12.0)),
        if up { Icon::ArrowUp } else { Icon::ArrowDown },
        design::mix(tokens.text_dim, tokens.text, hover),
    );
    response
}

/// Copy-to-clipboard row. The copy glyph (not a `↗`) tells the truth about
/// what the click does; the URL itself is handled by the caller's toast.
fn link_row(ui: &mut Ui, tokens: &Tokens, label: &str, url: &str, size: Vec2) -> bool {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let response = response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Copy link");
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), HOVER_SECS);
    if hover > 0.0 {
        ui.painter()
            .rect_filled(rect, tokens.radius_sm, design::fade(tokens.hover, hover));
    }
    ui.painter().rect_stroke(
        rect,
        tokens.radius_sm,
        Stroke::new(1.0, tokens.border),
        StrokeKind::Inside,
    );

    let font = FontId::proportional(11.5);
    let text_width = about_text_width(ui, label, 11.5);
    let content = ABOUT_ICON_SIZE + ABOUT_ICON_GAP + text_width;
    let left = rect.center().x - content * 0.5;
    let color = design::mix(tokens.text, tokens.accent, hover);
    icons::paint_icon(
        ui.painter(),
        Rect::from_center_size(
            Pos2::new(left + ABOUT_ICON_SIZE * 0.5, rect.center().y),
            Vec2::splat(ABOUT_ICON_SIZE),
        ),
        Icon::Copy,
        color,
    );
    ui.painter().text(
        Pos2::new(left + ABOUT_ICON_SIZE + ABOUT_ICON_GAP, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        color,
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

fn material_label(material: Material, fallback: bool) -> String {
    if fallback && material != Material::Solid {
        format!("{} (unavailable)", material_name(material))
    } else {
        material_name(material).to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaterialPick {
    None,
    Changed,
    Retry,
}

fn material_pick(current: Material, fallback: bool, chosen: Material) -> MaterialPick {
    if chosen != current {
        MaterialPick::Changed
    } else if fallback && current != Material::Solid {
        MaterialPick::Retry
    } else {
        MaterialPick::None
    }
}

fn background_name(mode: BackgroundMode) -> &'static str {
    match mode {
        BackgroundMode::Desktop => "Desktop",
        BackgroundMode::Image => "Image",
        BackgroundMode::None => "None",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "illness-overlays-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn palette_fits_short_and_tall_windows() {
        assert_eq!(
            PALETTE_CHROME_HEIGHT, 123.0,
            "fixed chrome height includes the 12px insets and no implicit spacing"
        );

        let (short_list, short_total) = palette_metrics(40, 405.0);
        assert!(
            short_total <= 405.0,
            "palette {short_total} must fit a 405px viewport"
        );
        assert!(short_list >= PALETTE_ROW_HEIGHT);

        let (tall_list, tall_total) = palette_metrics(40, 600.0);
        assert!((tall_list - 341.0).abs() < 0.01, "eight rows with 3px gaps");
        assert!((tall_total - (123.0 + 341.0)).abs() < 0.01);
        assert!(tall_list > short_list);

        let (empty_list, empty_total) = palette_metrics(0, 600.0);
        assert_eq!(empty_list, 52.0);
        assert_eq!(empty_total, 123.0 + 52.0);

        // 720x480 puts the palette top at 14% and leaves 8px below it.
        let available = 480.0 - 480.0 * 0.14 - 8.0;
        let (_, total) = palette_metrics(40, available);
        assert!(
            total <= available + 0.01,
            "height-capped palette must fit the viewport: {total} > {available}"
        );
    }

    #[test]
    fn palette_keyboard_scroll_reaches_offscreen_rows() {
        let profiles: Vec<Profile> = (0..25)
            .map(|index| Profile {
                name: format!("Profile {index}"),
                ..Default::default()
            })
            .collect();
        let items = palette_items("", &profiles, &[]);
        assert!(items.len() > 20, "need enough rows to scroll");

        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let ctx = egui::Context::default();
        // Observe the offset after a single pass instead of mid-animation.
        ctx.all_styles_mut(|style| style.scroll_animation = egui::style::ScrollAnimation::none());

        let mut state = OverlayState {
            kind: OverlayKind::Palette,
            selected: items.len() - 1,
            ..Default::default()
        };
        let mut scroll_y = 0.0;
        for frame in 0..3 {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(720.0, 480.0))),
                time: Some(frame as f64 * 0.1),
                ..Default::default()
            };
            ctx.run_ui(raw, |ui| {
                let (_, offset) = palette_list(ui, &items, &tokens, &mut state, 341.0, true);
                scroll_y = offset;
            })
            .drop_without_applying_deltas();
        }

        assert!(
            scroll_y > 0.0,
            "keyboard selection did not scroll the virtualized list: {scroll_y}"
        );
    }

    #[test]
    fn palette_arrow_down_reaches_offscreen_rows() {
        let profiles: Vec<Profile> = (0..25)
            .map(|index| Profile {
                name: format!("Profile {index}"),
                ..Default::default()
            })
            .collect();
        let items_len = palette_items("", &profiles, &[]).len();
        assert!(
            items_len > 20,
            "need enough rows to navigate past the viewport"
        );

        let theme = Theme::by_name("Illness Dark");
        let tokens = Tokens::from_theme(theme);
        let mut config = Config::default();
        let ctx = egui::Context::default();
        let mut state = OverlayState {
            kind: OverlayKind::Palette,
            ..Default::default()
        };

        let target = 22;
        assert!(target < items_len);
        for frame in 0..target {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(720.0, 480.0))),
                time: Some(frame as f64 * 0.1),
                events: vec![egui::Event::Key {
                    key: Key::ArrowDown,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            };
            ctx.run_ui(raw, |ui| {
                let actions =
                    show_palette(ui.ctx(), &mut state, &tokens, &mut config, &profiles, &[]);
                assert!(actions.is_empty(), "ArrowDown must not activate a command");
            })
            .drop_without_applying_deltas();
        }

        assert_eq!(state.selected, target);
    }

    #[test]
    fn close_resets_the_palette_and_advances_generation() {
        let mut state = OverlayState {
            kind: OverlayKind::Palette,
            query: "set".to_string(),
            selected: 3,
            ..Default::default()
        };
        let generation = state.palette_generation;

        state.close();

        assert_eq!(state.kind, OverlayKind::None);
        assert!(state.query.is_empty());
        assert_eq!(state.selected, 0);
        assert_eq!(state.palette_generation, generation + 1);
    }

    #[test]
    fn escape_dismisses_close_confirmation() {
        let mut state = OverlayState {
            kind: OverlayKind::ConfirmClose,
            ..Default::default()
        };
        let generation = state.palette_generation;

        assert!(
            handle_escape(&mut state, true),
            "Escape must consume the key"
        );
        assert_eq!(state.kind, OverlayKind::None);
        assert_eq!(state.palette_generation, generation + 1);
    }

    #[test]
    fn close_confirmation_maps_buttons_to_close_or_cancel() {
        assert_eq!(close_confirmation_action(true), Action::ConfirmCloseWindow);
        assert_eq!(close_confirmation_action(false), Action::CancelCloseWindow);
    }

    #[test]
    fn close_confirmation_accepts_enter_and_prefers_escape() {
        assert_eq!(close_confirmation_choice(false, false), None);
        assert_eq!(
            close_confirmation_choice(false, true),
            Some(true),
            "Enter confirms the destructive choice"
        );
        assert_eq!(close_confirmation_choice(true, false), Some(false));
        assert_eq!(
            close_confirmation_choice(true, true),
            Some(false),
            "Escape cancels when both keys arrive in one frame"
        );
    }

    #[test]
    fn close_confirmation_message_is_pluralized() {
        assert_eq!(
            close_confirmation_message(1),
            "1 tab is open. Closing the window ends the session."
        );
        assert_eq!(
            close_confirmation_message(2),
            "2 tabs are open. Closing the window ends every session."
        );
        assert!(
            close_confirmation_message(1).contains("1 tab "),
            "a lone tab must not read 1 tabs"
        );
    }

    #[test]
    fn close_card_height_follows_the_laid_out_text() {
        let small = confirm_close_card_height(18.0, 14.0);
        let zoomed = confirm_close_card_height(27.0, 21.0);
        assert_eq!(small, 52.0 + 18.0 + 10.0 + 14.0 + 24.0 + 34.0);
        assert!(
            zoomed > small,
            "the card must grow with the measured text, not stay 176 px"
        );
        let wrapped = confirm_close_card_height(18.0, 28.0);
        assert!(wrapped > small);
    }

    #[test]
    fn enter_uses_the_query_edited_in_the_same_frame() {
        // The pre-edit list used for sizing starts with New Tab.
        let mut items = palette_items("", &[], &[]);
        assert!(matches!(items[0].command, Command::NewTab(_)));

        // A same-frame keystroke changes the query and show_palette rebuilds
        // `items`; Enter then resolves against that fresh list, whose first
        // item is Settings.
        items = palette_items("settings", &[], &[]);
        assert!(
            matches!(
                items.first().map(|item| &item.command),
                Some(Command::Settings)
            ),
            "Enter must use the edited query, not the stale sizing list"
        );
    }

    #[test]
    fn replacing_a_toast_restarts_its_deadline() {
        let mut state = OverlayState::default();

        state.set_toast("first");
        state.toast_deadline = Some(3.0);
        state.set_toast("second");
        assert_eq!(state.toast.as_deref(), Some("second"));
        assert_eq!(
            state.toast_deadline, None,
            "a replacement must not inherit the old expiry"
        );
    }

    #[test]
    fn about_card_metrics_stay_compact_and_keep_the_text_column() {
        // Logo: capped at 44px on wide cards, still shrinking on narrow ones.
        assert_eq!(about_logo_size(420.0), ABOUT_LOGO_MAX);
        assert!((about_logo_size(200.0) - 44.0).abs() < 0.01);
        assert!((about_logo_size(100.0) - 28.0).abs() < 0.01);
        assert_eq!(about_logo_size(0.0), ABOUT_LOGO_MIN);
        for width in [0.0, 40.0, 120.0, 240.0, 720.0] {
            let logo = about_logo_size(width);
            assert!(
                (ABOUT_LOGO_MIN..=ABOUT_LOGO_MAX).contains(&logo),
                "logo {logo} out of range at {width}"
            );
        }

        // Action strip: one shared width, floored at the historic 96px.
        assert_eq!(about_button_width(&[]), ABOUT_BUTTON_MIN_WIDTH);
        assert_eq!(about_button_width(&[20.0, 30.0]), 96.0);
        assert!((about_button_width(&[90.0, 40.0]) - 114.0).abs() < 0.01);
        assert_eq!(
            about_button_width(&[40.0, 90.0]),
            about_button_width(&[90.0, 40.0])
        );
    }

    #[test]
    fn material_label_flags_a_fallback_but_not_solid() {
        assert_eq!(material_label(Material::Mica, false), "Mica");
        assert_eq!(material_label(Material::Acrylic, false), "Acrylic");
        assert_eq!(material_label(Material::Mica, true), "Mica (unavailable)");
        assert_eq!(
            material_label(Material::Acrylic, true),
            "Acrylic (unavailable)"
        );
        assert_eq!(material_label(Material::Solid, true), "Solid");
    }

    #[test]
    fn material_pick_retries_the_unavailable_request_and_accepts_solid() {
        assert_eq!(
            material_pick(Material::Mica, true, Material::Solid),
            MaterialPick::Changed
        );
        assert_eq!(
            material_pick(Material::Mica, true, Material::Mica),
            MaterialPick::Retry
        );
        assert_eq!(
            material_pick(Material::Acrylic, true, Material::Acrylic),
            MaterialPick::Retry
        );
        assert_eq!(
            material_pick(Material::Mica, false, Material::Mica),
            MaterialPick::None
        );
        assert_eq!(
            material_pick(Material::Mica, false, Material::Acrylic),
            MaterialPick::Changed
        );
    }

    #[test]
    fn background_controls_require_a_translucent_material() {
        assert!(background_controls_enabled(Material::Mica));
        assert!(background_controls_enabled(Material::Acrylic));
        assert!(!background_controls_enabled(Material::Solid));
    }

    #[test]
    fn opacity_requires_a_painted_backdrop_or_the_acrylic_accent() {
        assert!(!background_opacity_enabled(
            Material::Mica,
            BackgroundMode::None,
            false
        ));
        assert!(background_opacity_enabled(
            Material::Mica,
            BackgroundMode::Desktop,
            false
        ));
        assert!(!background_opacity_enabled(
            Material::Mica,
            BackgroundMode::Image,
            false
        ));
        assert!(background_opacity_enabled(
            Material::Mica,
            BackgroundMode::Image,
            true
        ));
        for mode in [
            BackgroundMode::Desktop,
            BackgroundMode::Image,
            BackgroundMode::None,
        ] {
            assert!(background_opacity_enabled(Material::Acrylic, mode, false));
            assert!(background_opacity_enabled(Material::Acrylic, mode, true));
        }
        assert!(!background_opacity_enabled(
            Material::Solid,
            BackgroundMode::Desktop,
            true
        ));
        assert!(!background_opacity_enabled(
            Material::Solid,
            BackgroundMode::None,
            false
        ));
    }

    #[test]
    fn imported_backgrounds_get_unique_names_and_replacements_are_cleaned_up() {
        let dir = temp_dir();
        let source = dir.join("wall.png");
        std::fs::write(&source, b"not a real png").expect("write source");
        let backgrounds = dir.join("backgrounds");

        let first = import_background_into(&source, &backgrounds).expect("first import");
        let second = import_background_into(&source, &backgrounds).expect("second import");
        assert_ne!(first, second);
        assert!(first.is_file());
        assert!(second.is_file());
        assert!(source.is_file(), "the source image must be untouched");

        assert!(remove_imported_background_in(&first, &second, &backgrounds));
        assert!(!first.exists(), "replaced import should be removed");
        assert!(second.exists(), "the replacement must survive");

        let outside = dir.join("outside.png");
        std::fs::write(&outside, b"x").expect("write outside");
        assert!(!remove_imported_background_in(
            &outside,
            &second,
            &backgrounds
        ));
        assert!(outside.exists(), "files outside the import dir are kept");

        let _ = std::fs::remove_dir_all(dir);
    }

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

        let unicode = "C:\\CLI\\Документы\\shell.exe";
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
