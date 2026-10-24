use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Plus,
    ChevronDown,
    ChevronUp,
    Close,
    Minimize,
    Maximize,
    Restore,
    Search,
    Settings,
    Palette,
    Copy,
    Paste,
    Check,
    Terminal,
    ArrowUp,
    ArrowDown,
    Folder,
    Power,
    Info,
    /// Horizontal ellipsis, used by overflow menus.
    More,
    /// Two panes, used by the new-tab split button.
    Split,
}

impl Icon {
    pub const ALL: [Self; 21] = [
        Self::Plus,
        Self::ChevronDown,
        Self::ChevronUp,
        Self::Close,
        Self::Minimize,
        Self::Maximize,
        Self::Restore,
        Self::Search,
        Self::Settings,
        Self::Palette,
        Self::Copy,
        Self::Paste,
        Self::Check,
        Self::Terminal,
        Self::ArrowUp,
        Self::ArrowDown,
        Self::Folder,
        Self::Power,
        Self::Info,
        Self::More,
        Self::Split,
    ];
}

/// Draw a vector icon centred in `rect`, designed on a 16x16 grid.
pub fn paint_icon(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let scale = rect.width().min(rect.height()) / 16.0;
    if scale <= 0.0 {
        return;
    }
    let origin = rect.center() - Vec2::splat(8.0 * scale);
    let pen = Pen {
        painter,
        origin,
        scale,
        width: 1.5 * scale,
        color,
    };
    pen.icon(icon);
}

struct Pen<'a> {
    painter: &'a Painter,
    origin: Pos2,
    scale: f32,
    width: f32,
    color: Color32,
}

impl Pen<'_> {
    fn at(&self, x: f32, y: f32) -> Pos2 {
        self.origin + Vec2::new(x * self.scale, y * self.scale)
    }

    fn circle(&self, x: f32, y: f32, radius: f32) {
        self.painter.circle_stroke(
            self.at(x, y),
            radius * self.scale,
            Stroke::new(self.width, self.color),
        );
    }

    fn dot(&self, x: f32, y: f32, radius: f32) {
        self.painter
            .circle_filled(self.at(x, y), radius * self.scale, self.color);
    }

    /// Open polyline with round caps (circles at the end points).
    fn line(&self, points: &[(f32, f32)]) {
        if points.len() < 2 {
            return;
        }
        let screen: Vec<Pos2> = points.iter().map(|&(x, y)| self.at(x, y)).collect();
        if let (Some(first), Some(last)) = (screen.first(), screen.last()) {
            let cap = self.width * 0.5;
            self.painter.circle_filled(*first, cap, self.color);
            self.painter.circle_filled(*last, cap, self.color);
        }
        self.painter
            .add(Shape::line(screen, Stroke::new(self.width, self.color)));
    }

    /// Closed outline with mitred joins.
    fn outline(&self, points: &[(f32, f32)]) {
        if points.len() < 2 {
            return;
        }
        let screen: Vec<Pos2> = points.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.painter.add(Shape::closed_line(
            screen,
            Stroke::new(self.width, self.color),
        ));
    }

    fn icon(&self, icon: Icon) {
        match icon {
            Icon::Plus => {
                self.line(&[(4.0, 8.0), (12.0, 8.0)]);
                self.line(&[(8.0, 4.0), (8.0, 12.0)]);
            }
            Icon::ChevronDown => self.line(&[(4.5, 6.0), (8.0, 9.5), (11.5, 6.0)]),
            Icon::ChevronUp => self.line(&[(4.5, 10.0), (8.0, 6.5), (11.5, 10.0)]),
            Icon::Close => {
                self.line(&[(4.5, 4.5), (11.5, 11.5)]);
                self.line(&[(11.5, 4.5), (4.5, 11.5)]);
            }
            Icon::Minimize => self.line(&[(4.5, 8.0), (11.5, 8.0)]),
            Icon::Maximize => {
                self.outline(&[(4.5, 4.5), (11.5, 4.5), (11.5, 11.5), (4.5, 11.5)]);
            }
            Icon::Restore => {
                self.line(&[(6.5, 6.5), (6.5, 4.5), (11.5, 4.5), (11.5, 9.5), (9.5, 9.5)]);
                self.outline(&[(4.5, 6.5), (9.5, 6.5), (9.5, 11.5), (4.5, 11.5)]);
            }
            Icon::Search => {
                self.circle(7.0, 7.0, 3.5);
                self.line(&[(9.5, 9.5), (12.3, 12.3)]);
            }
            Icon::Settings => {
                for step in 0..8 {
                    let angle = step as f32 * std::f32::consts::TAU / 8.0;
                    let (sin, cos) = angle.sin_cos();
                    self.line(&[
                        (8.0 + 3.6 * cos, 8.0 + 3.6 * sin),
                        (8.0 + 5.4 * cos, 8.0 + 5.4 * sin),
                    ]);
                }
                self.circle(8.0, 8.0, 3.0);
            }
            Icon::Palette => {
                self.circle(8.0, 8.0, 5.1);
                self.dot(6.2, 5.8, 1.0);
                self.dot(9.8, 5.8, 1.0);
                self.dot(10.4, 9.2, 1.0);
                self.dot(5.8, 9.2, 1.0);
            }
            Icon::Copy => {
                self.line(&[(6.0, 6.0), (6.0, 4.0), (12.0, 4.0), (12.0, 10.0)]);
                self.outline(&[(4.0, 6.0), (10.0, 6.0), (10.0, 12.0), (4.0, 12.0)]);
            }
            Icon::Paste => {
                self.outline(&[(4.5, 3.8), (11.5, 3.8), (11.5, 12.2), (4.5, 12.2)]);
                self.painter.rect_filled(
                    Rect::from_two_pos(self.at(6.3, 2.5), self.at(9.7, 4.8)),
                    0.0,
                    self.color,
                );
                self.line(&[(6.2, 7.2), (9.8, 7.2)]);
                self.line(&[(6.2, 9.6), (9.0, 9.6)]);
            }
            Icon::Check => self.line(&[(3.5, 8.5), (6.5, 11.5), (12.5, 4.5)]),
            Icon::Terminal => {
                self.outline(&[(3.0, 3.5), (13.0, 3.5), (13.0, 12.5), (3.0, 12.5)]);
                self.line(&[(5.5, 6.5), (7.5, 8.0), (5.5, 9.5)]);
                self.line(&[(9.0, 10.5), (11.0, 10.5)]);
            }
            Icon::ArrowUp => {
                self.line(&[(8.0, 12.5), (8.0, 4.5)]);
                self.line(&[(5.0, 7.5), (8.0, 4.5), (11.0, 7.5)]);
            }
            Icon::ArrowDown => {
                self.line(&[(8.0, 3.5), (8.0, 11.5)]);
                self.line(&[(5.0, 8.5), (8.0, 11.5), (11.0, 8.5)]);
            }
            Icon::Folder => self.outline(&[
                (3.0, 5.5),
                (6.0, 5.5),
                (7.5, 7.0),
                (13.0, 7.0),
                (13.0, 12.0),
                (3.0, 12.0),
            ]),
            Icon::Power => {
                let mut arc = Vec::with_capacity(25);
                for step in 0..=24 {
                    let angle = (40.0 + 280.0 * (step as f32 / 24.0)).to_radians();
                    arc.push((8.0 + 4.3 * angle.sin(), 8.3 - 4.3 * angle.cos()));
                }
                self.line(&arc);
                self.line(&[(8.0, 3.5), (8.0, 8.3)]);
            }
            Icon::Info => {
                self.circle(8.0, 8.0, 5.3);
                self.dot(8.0, 5.2, 0.9);
                self.line(&[(8.0, 7.4), (8.0, 11.2)]);
            }
            Icon::More => {
                self.dot(4.5, 8.0, 1.1);
                self.dot(8.0, 8.0, 1.1);
                self.dot(11.5, 8.0, 1.1);
            }
            Icon::Split => {
                self.outline(&[(3.0, 4.0), (13.0, 4.0), (13.0, 12.0), (3.0, 12.0)]);
                self.line(&[(8.0, 4.0), (8.0, 12.0)]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icon_bounds() {
        for icon in Icon::ALL {
            let ctx = egui::Context::default();
            let rect = Rect::from_min_size(Pos2::new(12.0, 12.0), Vec2::splat(16.0));
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                paint_icon(&ui.painter_at(rect), rect, icon, Color32::WHITE);
            });
            output.textures_delta.clear();

            let mut painted = 0;
            for clipped in &output.shapes {
                let bounds = clipped.shape.visual_bounding_rect();
                if bounds.is_negative() {
                    continue;
                }
                painted += 1;
                assert!(
                    rect.expand(0.01).contains_rect(bounds),
                    "{icon:?} painted outside {rect:?}: {bounds:?}"
                );
            }
            assert!(painted > 0, "{icon:?} painted nothing");
        }
    }
}
