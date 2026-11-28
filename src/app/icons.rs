//! Line icons drawn with the painter: no icon font, no images, crisp at any
//! scale. Coordinates are on a 24x24 grid, as in common SVG icon sets.

use eframe::egui::{
    self, epaint::PathShape, pos2, Color32, Painter, Pos2, Rect, Response, Sense, Shape, Stroke,
    StrokeKind, Ui,
};

use super::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    ChevronRight,
    ChevronDown,
    ChevronUp,
    ChevronLeft,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    Refresh,
    Upload,
    Download,
    Folder,
    FolderPlus,
    File,
    FilePlus,
    Text,
    Code,
    Image,
    Video,
    Audio,
    Archive,
    Bucket,
    Link,
    Globe,
    Key,
    Copy,
    Cut,
    Paste,
    Trash,
    Pencil,
    Info,
    Help,
    Close,
    Search,
    Plus,
    Check,
    Alert,
    Panel,
    Eye,
    Settings,
    Transfers,
    Retry,
    Cloud,
    Play,
    Update,
}

struct Pen<'a> {
    painter: &'a Painter,
    rect: Rect,
    stroke: Stroke,
}

impl Pen<'_> {
    fn p(&self, x: f32, y: f32) -> Pos2 {
        let scale = self.rect.width() / 24.0;
        self.rect.min + egui::vec2(x, y) * scale
    }

    fn scale(&self) -> f32 {
        self.rect.width() / 24.0
    }

    fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.painter
            .line_segment([self.p(x1, y1), self.p(x2, y2)], self.stroke);
    }

    fn poly(&self, points: &[(f32, f32)]) {
        let points = points.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter.add(Shape::line(points, self.stroke));
    }

    fn closed(&self, points: &[(f32, f32)]) {
        let points = points.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter.add(Shape::closed_line(points, self.stroke));
    }

    fn filled(&self, points: &[(f32, f32)], fill: Color32) {
        let points = points.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter
            .add(Shape::convex_polygon(points, fill, Stroke::NONE));
    }

    fn rrect(&self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let rect = Rect::from_min_max(self.p(x, y), self.p(x + w, y + h));
        self.painter
            .rect_stroke(rect, r * self.scale(), self.stroke, StrokeKind::Middle);
    }

    fn circle(&self, cx: f32, cy: f32, r: f32) {
        self.painter
            .circle_stroke(self.p(cx, cy), r * self.scale(), self.stroke);
    }

    fn dot(&self, cx: f32, cy: f32) {
        self.painter
            .circle_filled(self.p(cx, cy), self.stroke.width * 0.7, self.stroke.color);
    }

    /// Elliptical arc; angles in degrees, 0 = +x, 90 = down.
    fn arc(&self, cx: f32, cy: f32, rx: f32, ry: f32, from: f32, to: f32) {
        let steps = (((to - from).abs() / 8.0).ceil() as usize).max(4);
        let points = (0..=steps)
            .map(|i| {
                let angle = (from + (to - from) * i as f32 / steps as f32).to_radians();
                self.p(cx + rx * angle.cos(), cy + ry * angle.sin())
            })
            .collect();
        self.painter
            .add(Shape::Path(PathShape::line(points, self.stroke)));
    }

    fn page(&self) {
        self.closed(&[
            (14.0, 3.0),
            (6.5, 3.0),
            (5.0, 4.5),
            (5.0, 19.5),
            (6.5, 21.0),
            (17.5, 21.0),
            (19.0, 19.5),
            (19.0, 8.0),
        ]);
        self.poly(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)]);
    }

    fn tray(&self) {
        self.poly(&[(4.0, 16.5), (4.0, 19.5), (20.0, 19.5), (20.0, 16.5)]);
    }

    fn folder(&self, tint: Option<Color32>) {
        if let Some(tint) = tint {
            self.filled(
                &[
                    (3.0, 8.0),
                    (3.0, 17.0),
                    (5.0, 19.0),
                    (19.0, 19.0),
                    (21.0, 17.0),
                    (21.0, 9.0),
                    (19.0, 7.0),
                    (3.5, 7.0),
                ],
                tint,
            );
            self.filled(
                &[(3.0, 7.5), (5.0, 5.0), (9.0, 5.0), (11.0, 7.0), (3.0, 7.0)],
                tint,
            );
        }
        self.closed(&[
            (3.0, 7.0),
            (5.0, 5.0),
            (9.0, 5.0),
            (11.0, 7.0),
            (19.0, 7.0),
            (21.0, 9.0),
            (21.0, 17.0),
            (19.0, 19.0),
            (5.0, 19.0),
            (3.0, 17.0),
        ]);
    }

    fn capsule(&self, a: (f32, f32), b: (f32, f32), r: f32) {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let angle = dy.atan2(dx).to_degrees();
        self.arc(b.0, b.1, r, r, angle - 90.0, angle + 90.0);
        self.arc(a.0, a.1, r, r, angle + 90.0, angle + 270.0);
        let length = dx.hypot(dy);
        let (nx, ny) = (-dy / length * r, dx / length * r);
        self.line(a.0 + nx, a.1 + ny, b.0 + nx, b.1 + ny);
        self.line(a.0 - nx, a.1 - ny, b.0 - nx, b.1 - ny);
    }
}

/// Paints an icon into `rect` (square) in `color`.
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let size = rect.width().min(rect.height());
    let rect = Rect::from_center_size(rect.center(), egui::vec2(size, size));
    let pen = Pen {
        painter,
        rect,
        stroke: Stroke::new((size / 24.0 * 1.6).max(1.1), color),
    };
    let tint = color.gamma_multiply(0.28);
    match icon {
        Icon::ChevronRight => pen.poly(&[(9.0, 6.0), (15.0, 12.0), (9.0, 18.0)]),
        Icon::ChevronDown => pen.poly(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)]),
        Icon::ChevronUp => pen.poly(&[(6.0, 15.0), (12.0, 9.0), (18.0, 15.0)]),
        Icon::ChevronLeft => pen.poly(&[(15.0, 6.0), (9.0, 12.0), (15.0, 18.0)]),
        Icon::ArrowLeft => {
            pen.line(19.0, 12.0, 5.0, 12.0);
            pen.poly(&[(11.0, 18.0), (5.0, 12.0), (11.0, 6.0)]);
        }
        Icon::ArrowRight => {
            pen.line(5.0, 12.0, 19.0, 12.0);
            pen.poly(&[(13.0, 6.0), (19.0, 12.0), (13.0, 18.0)]);
        }
        Icon::ArrowUp => {
            pen.line(12.0, 19.0, 12.0, 5.0);
            pen.poly(&[(6.0, 11.0), (12.0, 5.0), (18.0, 11.0)]);
        }
        Icon::Refresh => {
            pen.arc(12.0, 12.0, 8.0, 8.0, 0.0, 315.0);
            pen.poly(&[(20.0, 4.0), (20.0, 9.0), (15.0, 9.0)]);
        }
        Icon::Retry => {
            pen.arc(12.0, 12.0, 8.0, 8.0, 180.0, 495.0);
            pen.poly(&[(4.0, 20.0), (4.0, 15.0), (9.0, 15.0)]);
        }
        Icon::Upload => {
            pen.line(12.0, 15.5, 12.0, 4.0);
            pen.poly(&[(7.0, 9.0), (12.0, 4.0), (17.0, 9.0)]);
            pen.tray();
        }
        Icon::Download => {
            pen.line(12.0, 4.0, 12.0, 15.5);
            pen.poly(&[(7.0, 10.5), (12.0, 15.5), (17.0, 10.5)]);
            pen.tray();
        }
        Icon::Folder => pen.folder(Some(tint)),
        Icon::FolderPlus => {
            pen.folder(None);
            pen.line(12.0, 10.5, 12.0, 15.5);
            pen.line(9.5, 13.0, 14.5, 13.0);
        }
        Icon::File => pen.page(),
        Icon::FilePlus => {
            pen.page();
            pen.line(12.0, 11.5, 12.0, 16.5);
            pen.line(9.5, 14.0, 14.5, 14.0);
        }
        Icon::Text => {
            pen.page();
            pen.line(9.0, 13.0, 15.0, 13.0);
            pen.line(9.0, 17.0, 13.0, 17.0);
        }
        Icon::Code => {
            pen.page();
            pen.poly(&[(10.5, 12.5), (8.5, 15.0), (10.5, 17.5)]);
            pen.poly(&[(13.5, 12.5), (15.5, 15.0), (13.5, 17.5)]);
        }
        Icon::Archive => {
            pen.page();
            pen.line(10.0, 11.0, 12.0, 11.0);
            pen.line(10.0, 14.0, 12.0, 14.0);
            pen.line(10.0, 17.0, 12.0, 17.0);
        }
        Icon::Image => {
            pen.rrect(3.0, 4.0, 18.0, 16.0, 2.0);
            pen.circle(8.5, 9.5, 1.5);
            pen.poly(&[(21.0, 15.5), (16.5, 11.0), (7.0, 20.0)]);
        }
        Icon::Video => {
            pen.rrect(3.0, 5.0, 18.0, 14.0, 2.0);
            pen.closed(&[(10.0, 9.2), (10.0, 14.8), (14.8, 12.0)]);
        }
        Icon::Audio => {
            pen.poly(&[(9.0, 18.0), (9.0, 6.0), (19.0, 4.0), (19.0, 16.0)]);
            pen.circle(6.5, 18.0, 2.5);
            pen.circle(16.5, 16.0, 2.5);
        }
        Icon::Bucket => {
            pen.arc(12.0, 6.0, 8.0, 3.0, 0.0, 360.0);
            pen.line(4.0, 6.0, 4.0, 18.0);
            pen.line(20.0, 6.0, 20.0, 18.0);
            pen.arc(12.0, 18.0, 8.0, 3.0, 0.0, 180.0);
            pen.arc(12.0, 12.0, 8.0, 3.0, 0.0, 180.0);
        }
        Icon::Link => {
            pen.capsule((6.5, 17.5), (10.5, 13.5), 2.9);
            pen.capsule((13.5, 10.5), (17.5, 6.5), 2.9);
            pen.line(9.5, 14.5, 14.5, 9.5);
        }
        Icon::Globe => {
            pen.circle(12.0, 12.0, 9.0);
            pen.line(3.0, 12.0, 21.0, 12.0);
            pen.arc(12.0, 12.0, 3.8, 9.0, 0.0, 360.0);
        }
        Icon::Key => {
            pen.circle(8.0, 15.0, 4.0);
            pen.line(11.0, 12.0, 20.0, 3.0);
            pen.line(16.0, 7.0, 19.0, 10.0);
            pen.line(14.0, 9.0, 16.0, 11.0);
        }
        Icon::Copy => {
            pen.rrect(9.0, 9.0, 11.0, 11.0, 2.0);
            pen.poly(&[(5.0, 15.0), (5.0, 6.0), (6.0, 4.5), (7.0, 4.0), (16.0, 4.0)]);
        }
        Icon::Cut => {
            pen.circle(6.0, 7.0, 2.5);
            pen.circle(6.0, 17.0, 2.5);
            pen.line(8.2, 8.4, 20.0, 18.0);
            pen.line(8.2, 15.6, 20.0, 6.0);
        }
        Icon::Paste => {
            pen.rrect(5.0, 5.0, 14.0, 16.0, 2.0);
            pen.poly(&[(9.0, 5.0), (9.0, 3.5), (15.0, 3.5), (15.0, 5.0)]);
            pen.line(9.0, 12.0, 15.0, 12.0);
            pen.line(9.0, 16.0, 13.0, 16.0);
        }
        Icon::Trash => {
            pen.line(4.0, 7.0, 20.0, 7.0);
            pen.poly(&[(9.0, 7.0), (9.0, 4.0), (15.0, 4.0), (15.0, 7.0)]);
            pen.poly(&[(6.0, 7.0), (7.0, 20.0), (17.0, 20.0), (18.0, 7.0)]);
            pen.line(10.0, 11.0, 10.0, 16.0);
            pen.line(14.0, 11.0, 14.0, 16.0);
        }
        Icon::Pencil => {
            pen.closed(&[
                (4.0, 20.0),
                (5.0, 16.0),
                (16.0, 5.0),
                (19.0, 8.0),
                (8.0, 19.0),
            ]);
            pen.line(14.0, 7.0, 17.0, 10.0);
        }
        Icon::Info => {
            pen.circle(12.0, 12.0, 9.0);
            pen.line(12.0, 11.0, 12.0, 16.0);
            pen.dot(12.0, 8.0);
        }
        Icon::Help => {
            pen.circle(12.0, 12.0, 9.0);
            pen.arc(12.0, 9.8, 2.6, 2.6, 180.0, 400.0);
            pen.poly(&[(14.0, 11.5), (12.0, 13.0), (12.0, 14.2)]);
            pen.dot(12.0, 17.0);
        }
        Icon::Close => {
            pen.line(6.0, 6.0, 18.0, 18.0);
            pen.line(18.0, 6.0, 6.0, 18.0);
        }
        Icon::Search => {
            pen.circle(11.0, 11.0, 6.5);
            pen.line(16.0, 16.0, 20.5, 20.5);
        }
        Icon::Plus => {
            pen.line(12.0, 5.0, 12.0, 19.0);
            pen.line(5.0, 12.0, 19.0, 12.0);
        }
        Icon::Check => pen.poly(&[(5.0, 12.5), (9.5, 17.0), (19.0, 7.5)]),
        Icon::Alert => {
            pen.closed(&[(12.0, 4.0), (21.0, 20.0), (3.0, 20.0)]);
            pen.line(12.0, 10.0, 12.0, 14.0);
            pen.dot(12.0, 17.0);
        }
        Icon::Panel => {
            pen.rrect(3.0, 4.0, 18.0, 16.0, 2.0);
            pen.line(15.0, 4.0, 15.0, 20.0);
        }
        Icon::Eye => {
            pen.arc(12.0, 12.0, 10.0, 6.5, 0.0, 360.0);
            pen.circle(12.0, 12.0, 3.0);
        }
        Icon::Settings => {
            pen.line(4.0, 7.0, 13.5, 7.0);
            pen.line(18.5, 7.0, 20.0, 7.0);
            pen.line(4.0, 17.0, 7.5, 17.0);
            pen.line(12.5, 17.0, 20.0, 17.0);
            pen.circle(16.0, 7.0, 2.5);
            pen.circle(10.0, 17.0, 2.5);
        }
        Icon::Transfers => {
            pen.line(7.0, 20.0, 7.0, 5.0);
            pen.poly(&[(3.0, 9.0), (7.0, 5.0), (11.0, 9.0)]);
            pen.line(17.0, 4.0, 17.0, 19.0);
            pen.poly(&[(13.0, 15.0), (17.0, 19.0), (21.0, 15.0)]);
        }
        Icon::Cloud => {
            pen.line(7.0, 18.0, 17.5, 18.0);
            pen.arc(7.0, 14.0, 4.0, 4.0, 90.0, 265.0);
            pen.arc(12.5, 10.5, 5.6, 5.6, 195.0, 345.0);
            pen.arc(17.5, 13.75, 4.25, 4.25, 280.0, 450.0);
        }
        Icon::Play => pen.filled(&[(8.0, 5.0), (19.0, 12.0), (8.0, 19.0)], color),
        Icon::Update => {
            pen.circle(12.0, 12.0, 9.0);
            pen.line(12.0, 16.5, 12.0, 7.5);
            pen.poly(&[(8.5, 11.0), (12.0, 7.5), (15.5, 11.0)]);
        }
    }
}

/// Square, borderless toolbar button with an icon.
pub fn button(ui: &mut Ui, icon: Icon, tooltip: &str, enabled: bool) -> Response {
    sized_button(ui, icon, tooltip, enabled, 26.0, 16.0)
}

/// Smaller [`button`], to sit inside a 26px box.
pub fn inline_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    sized_button(ui, icon, tooltip, true, 20.0, 14.0)
}

fn sized_button(
    ui: &mut Ui,
    icon: Icon,
    tooltip: &str,
    enabled: bool,
    size: f32,
    icon_size: f32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(size, size),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let p = theme::palette(ui);
    if enabled && response.is_pointer_button_down_on() {
        ui.painter().rect_filled(rect, 3, p.pressed);
    } else if enabled && response.hovered() {
        ui.painter().rect_filled(rect, 3, p.hover);
    }
    let color = if enabled {
        p.text
    } else {
        p.faint.gamma_multiply(0.7)
    };
    paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), egui::vec2(icon_size, icon_size)),
        icon,
        color,
    );
    response.on_hover_text(tooltip)
}

/// Toggle variant of [`button`]: drawn pressed while `active`.
pub fn toggle(ui: &mut Ui, icon: Icon, tooltip: &str, active: bool) -> Response {
    let size = egui::vec2(26.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let p = theme::palette(ui);
    if active {
        ui.painter().rect(
            rect,
            3,
            p.pressed,
            Stroke::new(1.0, p.border_strong),
            StrokeKind::Inside,
        );
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 3, p.hover);
    }
    paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0)),
        icon,
        p.text,
    );
    response.on_hover_text(tooltip)
}

/// Borderless toolbar button with an icon and a label.
pub fn tool(ui: &mut Ui, icon: Icon, label: &str, enabled: bool) -> Response {
    let font = theme::body();
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font, Color32::PLACEHOLDER);
    let size = egui::vec2(8.0 + 16.0 + 6.0 + galley.size().x + 9.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let p = theme::palette(ui);
    if enabled && response.is_pointer_button_down_on() {
        ui.painter().rect_filled(rect, 3, p.pressed);
    } else if enabled && response.hovered() {
        ui.painter().rect_filled(rect, 3, p.hover);
    }
    let color = if enabled {
        p.text
    } else {
        p.faint.gamma_multiply(0.7)
    };
    let icon_rect = Rect::from_min_size(
        pos2(rect.left() + 8.0, rect.center().y - 8.0),
        egui::vec2(16.0, 16.0),
    );
    paint(ui.painter(), icon_rect, icon, color);
    ui.painter().galley(
        pos2(
            icon_rect.right() + 6.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    response
}
