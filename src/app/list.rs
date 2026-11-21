//! The file list: a virtualized, sortable table with multi-selection. Only the
//! rows in view are laid out, so very large folders stay fast.

use eframe::egui::{self, pos2, vec2, Color32, Rect, Sense, Stroke, StrokeKind, Ui};

use super::icons::{self, Icon};
use super::{theme, ClipMode, Column, FathomApp, Item, ItemKind};
use crate::format::{self, Kind};

/// Scroll position of the list body, kept so keyboard navigation can reveal rows.
#[derive(Default)]
pub struct Scroll {
    pub offset: f32,
    pub height: f32,
    pub set: Option<f32>,
}

const COLUMNS: [(Column, &str, f32); 4] = [
    (Column::Name, "Name", 0.0),
    (Column::Size, "Size", 90.0),
    (Column::Type, "Type", 140.0),
    (Column::Modified, "Modified", 150.0),
];

pub fn icon_for(item: &Item) -> Icon {
    match item.kind {
        ItemKind::Bucket => Icon::Bucket,
        ItemKind::Folder => Icon::Folder,
        ItemKind::File => match format::kind_of(&item.name) {
            Kind::Image => Icon::Image,
            Kind::Video => Icon::Video,
            Kind::Audio => Icon::Audio,
            Kind::Code => Icon::Code,
            Kind::Text => Icon::Text,
            Kind::Archive => Icon::Archive,
            Kind::File => Icon::File,
        },
    }
}

pub fn color_for(item: &Item, p: &theme::Palette) -> Color32 {
    match item.kind {
        ItemKind::Bucket => p.bucket,
        ItemKind::Folder => p.folder,
        ItemKind::File => match format::kind_of(&item.name) {
            Kind::Image => p.image,
            Kind::Video => p.video,
            Kind::Audio => p.audio,
            Kind::Code => p.code,
            Kind::Text => p.text_file,
            Kind::Archive => p.archive,
            Kind::File => p.file,
        },
    }
}

/// Widths of the four columns for a list `width` wide.
fn column_widths(width: f32) -> [f32; 4] {
    let fixed: f32 = COLUMNS.iter().map(|c| c.2).sum();
    [
        (width - fixed).max(160.0),
        COLUMNS[1].2,
        COLUMNS[2].2,
        COLUMNS[3].2,
    ]
}

/// Draws `text` left- or right-aligned in `rect`, cut with an ellipsis if it does not fit.
fn cell_text(ui: &Ui, rect: Rect, text: &str, color: Color32, right: bool) {
    let font = theme::body();
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let mut galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
    if galley.size().x > rect.width() {
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width());
        galley = painter.fonts_mut(|f| f.layout_job(job));
    }
    let y = rect.center().y - galley.size().y / 2.0;
    let x = if right {
        rect.right() - galley.size().x
    } else {
        rect.left()
    };
    painter.galley(pos2(x, y), galley, color);
}

impl FathomApp {
    pub(super) fn file_list(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let full = ui.available_rect_before_wrap();
        self.list_rect = Some(full);
        let widths = column_widths(full.width() - 14.0);

        // Header.
        let header = Rect::from_min_size(full.min, vec2(full.width(), theme::ROW));
        ui.painter().rect_filled(header, 0, p.bg);
        ui.painter().hline(
            header.x_range(),
            header.bottom() - 0.5,
            Stroke::new(1.0, p.border),
        );
        let mut x = header.left();
        for (index, (column, label, _)) in COLUMNS.iter().enumerate() {
            let rect = Rect::from_min_size(pos2(x, header.top()), vec2(widths[index], theme::ROW));
            let response = ui.interact(rect, ui.scope_id().with(("header", index)), Sense::click());
            let sorted = self.sort.0 == *column;
            if response.hovered() {
                ui.painter().rect_filled(rect, 0, p.hover);
            }
            if index < 3 {
                ui.painter().vline(
                    rect.right() - 0.5,
                    rect.y_range().shrink(4.0),
                    Stroke::new(1.0, p.border),
                );
            }
            let color = if sorted || response.hovered() {
                p.text
            } else {
                p.muted
            };
            let inner = rect.shrink2(vec2(8.0, 0.0));
            cell_text(ui, inner, label, color, *column == Column::Size);
            if sorted {
                let arrow_x = if *column == Column::Size {
                    inner.left()
                } else {
                    inner.right() - 10.0
                };
                let arrow =
                    Rect::from_center_size(pos2(arrow_x + 5.0, rect.center().y), vec2(10.0, 10.0));
                let icon = if self.sort.1 {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronUp
                };
                icons::paint(ui.painter(), arrow, icon, p.muted);
            }
            if response.clicked() {
                self.sort = (*column, if sorted { !self.sort.1 } else { false });
                self.view_dirty = true;
            }
            x += widths[index];
        }

        let body = Rect::from_min_max(pos2(full.left(), header.bottom()), full.max);
        ui.allocate_rect(full, Sense::hover());

        // Thin activity bar while a listing is loading.
        if self.listing.loading {
            let t = ui.input(|i| i.time) as f32;
            let width = body.width() * 0.3;
            let start = body.left() + ((t * 0.8) % 1.0) * (body.width() + width) - width;
            let bar = Rect::from_min_max(
                pos2(start.max(body.left()), body.top()),
                pos2((start + width).min(body.right()), body.top() + 2.0),
            );
            ui.painter().rect_filled(bar, 0, p.accent);
            ui.ctx().request_repaint();
        }

        // Empty space behind the rows: clicking it clears the selection.
        let background = ui.interact(body, ui.scope_id().with("list-background"), Sense::click());
        if background.clicked() && !ui.input(|i| i.modifiers.command || i.modifiers.shift) {
            self.selection.clear();
        }
        background.context_menu(|ui| self.background_menu(ui));

        if self.view.is_empty() {
            self.list_placeholder(ui, body);
            return;
        }

        // Rows.
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body.shrink2(vec2(0.0, 1.0))));
        // show_rows sizes the content from this ui's spacing, so it has to be zero
        // here, not just inside the rows, or the list gains empty space at the end.
        child.spacing_mut().item_spacing.y = 0.0;
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("file-list")
            .auto_shrink(false)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible);
        if let Some(key) = self.reveal.take() {
            if let Some(index) = self.view_position(&key) {
                let top = index as f32 * theme::ROW;
                let bottom = top + theme::ROW;
                if top < self.list_scroll.offset {
                    self.list_scroll.set = Some(top);
                } else if bottom > self.list_scroll.offset + self.list_scroll.height {
                    self.list_scroll.set = Some(bottom - self.list_scroll.height);
                }
            }
        }
        if let Some(offset) = self.list_scroll.set.take() {
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let count = self.view.len();
        let output = scroll.show_rows(&mut child, theme::ROW, count, |ui, range| {
            for row in range {
                // A row's action (opening a folder, say) can replace the listing mid-frame.
                if self
                    .view
                    .get(row)
                    .is_none_or(|&i| i >= self.listing.items.len())
                {
                    break;
                }
                self.list_row(ui, row, widths);
            }
        });
        self.list_scroll.offset = output.state.clamped_offset().y;
        self.list_scroll.height = output.inner_rect.height();
    }

    fn list_row(&mut self, ui: &mut Ui, row: usize, widths: [f32; 4]) {
        let p = theme::palette(ui);
        let item = self.listing.items[self.view[row]].clone();
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(vec2(width, theme::ROW), Sense::click());
        let selected = self.selection.contains(&item.key);
        let is_cursor = self.cursor.as_deref() == Some(item.key.as_str());
        let cut = self.clipboard.as_ref().is_some_and(|clip| {
            clip.mode == ClipMode::Cut
                && Some(&clip.bucket) == self.location.bucket.as_ref()
                && clip.items.iter().any(|i| i.key == item.key)
        });

        let window_focused = ui.ctx().input(|i| i.focused);
        if selected {
            ui.painter().rect_filled(
                rect,
                0,
                if window_focused {
                    p.selected
                } else {
                    p.selected_inactive
                },
            );
        } else if response.hovered() {
            ui.painter().rect_filled(rect, 0, p.hover);
        }
        if is_cursor && self.selection.len() > 1 {
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                0,
                Stroke::new(1.0, p.faint),
                StrokeKind::Inside,
            );
        }

        let fade = |color: Color32| {
            if cut {
                color.gamma_multiply(0.5)
            } else {
                color
            }
        };
        let mut x = rect.left();
        // Name with icon.
        let name_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(widths[0], theme::ROW))
            .shrink2(vec2(8.0, 0.0));
        let icon_rect = Rect::from_min_size(
            pos2(name_rect.left(), rect.center().y - 8.0),
            vec2(16.0, 16.0),
        );
        icons::paint(
            ui.painter(),
            icon_rect,
            icon_for(&item),
            fade(color_for(&item, p)),
        );
        let text_rect = Rect::from_min_max(
            pos2(icon_rect.right() + 6.0, rect.top()),
            pos2(name_rect.right(), rect.bottom()),
        );
        cell_text(ui, text_rect, &item.name, fade(p.text), false);
        x += widths[0];

        let size = if item.is_file() {
            format::size(item.size)
        } else {
            String::new()
        };
        let type_label = super::type_of(&item);
        let modified = item.modified.map(format::date).unwrap_or_default();
        for (index, (text, right)) in [(size, true), (type_label, false), (modified, false)]
            .into_iter()
            .enumerate()
        {
            let cell =
                Rect::from_min_size(pos2(x, rect.top()), vec2(widths[index + 1], theme::ROW))
                    .shrink2(vec2(8.0, 0.0));
            cell_text(ui, cell, &text, fade(p.muted), right);
            x += widths[index + 1];
        }

        // Selection: click, Ctrl/Cmd+click to toggle, Shift+click for a range.
        if response.clicked() || (response.secondary_clicked() && !selected) {
            let modifiers = ui.input(|i| i.modifiers);
            if response.secondary_clicked() {
                self.select_only(&item.key);
            } else if modifiers.shift
                && self
                    .anchor
                    .as_deref()
                    .and_then(|a| self.view_position(a))
                    .is_some()
            {
                let anchor = self
                    .anchor
                    .as_deref()
                    .and_then(|a| self.view_position(a))
                    .unwrap();
                let (from, to) = (anchor.min(row), anchor.max(row));
                let range: Vec<String> = self.view[from..=to]
                    .iter()
                    .map(|&i| self.listing.items[i].key.clone())
                    .collect();
                if !modifiers.command {
                    self.selection.clear();
                }
                self.selection.extend(range);
                self.cursor = Some(item.key.clone());
            } else if modifiers.command {
                if !self.selection.remove(&item.key) {
                    self.selection.insert(item.key.clone());
                }
                self.cursor = Some(item.key.clone());
                self.anchor = Some(item.key.clone());
            } else {
                self.select_only(&item.key);
            }
        }
        if response.double_clicked() {
            self.open_item(&item);
        }
        response.context_menu(|ui| {
            let items = self.selected_items();
            self.item_menu(ui, items);
        });
    }

    fn list_placeholder(&mut self, ui: &mut Ui, body: Rect) {
        let p = theme::palette(ui);
        let (title, detail, action): (&str, String, Option<&str>) = if self.active.is_none() {
            (
                "Connect to Cloudflare R2",
                "Add your R2 API credentials to start browsing your buckets.".into(),
                Some("Add connection"),
            )
        } else if let Some(error) = &self.listing.error {
            (
                "Could not load this location",
                error.clone(),
                Some("Try again"),
            )
        } else if self.listing.loading {
            ("", String::new(), None)
        } else if !self.filter.trim().is_empty() {
            (
                "No matches",
                format!("Nothing here matches \"{}\".", self.filter.trim()),
                None,
            )
        } else if self.location.bucket.is_none() {
            (
                "No buckets",
                "This account has no R2 buckets yet, or the token cannot list them.".into(),
                None,
            )
        } else {
            (
                "This folder is empty",
                "Drop files here, or use Upload to add some.".into(),
                None,
            )
        };
        if title.is_empty() {
            return;
        }
        let area = Rect::from_center_size(body.center(), vec2(body.width().min(420.0), 120.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(area)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        child.label(
            egui::RichText::new(title)
                .font(theme::strong(14.0))
                .color(p.text),
        );
        child.add(egui::Label::new(egui::RichText::new(detail).color(p.muted)).wrap());
        if let Some(action) = action {
            child.add_space(6.0);
            if child.button(action).clicked() {
                if self.active.is_none() {
                    self.modal = Some(super::dialogs::Modal::connection(None));
                } else {
                    self.refresh();
                }
            }
        }
    }

    /// The overlay shown while files are dragged over the window.
    pub(super) fn drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering {
            return;
        }
        let p = theme::palette_of(ctx);
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("drop"),
        ));
        let target = self
            .location
            .bucket
            .as_ref()
            .map(|bucket| format!("Upload to {bucket}/{}", self.location.prefix));
        let color = if target.is_some() { p.accent } else { p.faint };
        painter.rect_filled(screen, 0, color.gamma_multiply(0.04));
        // A dashed drop zone around the file list, where the files will land. The
        // dashes march slowly while files are held over the window.
        let zone = match self.list_rect {
            // Below the column headers, clear of the row icons.
            Some(list) => Rect::from_min_max(
                pos2(list.left() + 4.0, list.top() + theme::ROW + 4.0),
                pos2(list.right() - 4.0, list.bottom() - 4.0),
            ),
            None => screen.shrink(8.0),
        };
        painter.rect_filled(zone, 8, color.gamma_multiply(0.08));
        let march = (ctx.input(|i| i.time) as f32 * 24.0) % 17.0;
        painter.extend(egui::Shape::dashed_line_with_offset(
            &rounded_path(zone, 8.0),
            Stroke::new(2.0, color),
            &[10.0],
            &[7.0],
            17.0 - march,
        ));
        ctx.request_repaint();
        let label = target.unwrap_or_else(|| "Open a bucket to upload files".into());
        let galley = painter.layout_no_wrap(label, theme::body(), p.text);
        let card = Rect::from_center_size(
            pos2(zone.center().x, zone.bottom() - 44.0),
            galley.size() + vec2(48.0, 14.0),
        );
        painter.rect(
            card,
            3,
            p.raised,
            Stroke::new(1.0, p.border_strong),
            StrokeKind::Inside,
        );
        icons::paint(
            &painter,
            Rect::from_min_size(
                pos2(card.left() + 10.0, card.center().y - 8.0),
                vec2(16.0, 16.0),
            ),
            Icon::Upload,
            p.text,
        );
        painter.galley(
            pos2(card.left() + 34.0, card.center().y - galley.size().y / 2.0),
            galley,
            p.text,
        );
    }
}

/// The outline of a rounded rectangle as a closed polyline, for dashed strokes.
fn rounded_path(rect: Rect, radius: f32) -> Vec<egui::Pos2> {
    const STEPS: usize = 8;
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let corners = [
        (pos2(rect.right() - r, rect.top() + r), -90.0_f32),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    let mut points = Vec::with_capacity(4 * (STEPS + 1) + 1);
    for (center, start) in corners {
        for step in 0..=STEPS {
            let angle = (start + 90.0 * step as f32 / STEPS as f32).to_radians();
            points.push(center + r * vec2(angle.cos(), angle.sin()));
        }
    }
    points.push(points[0]);
    points
}
