//! Sidebar: the connection switcher and the bucket/folder tree, loaded on demand.

use eframe::egui::{self, pos2, vec2, Rect, Sense, Stroke, Ui};

use super::icons::{self, Icon};
use super::{dialogs, theme, FathomApp, Location};
use crate::format;

/// A row of the tree, flattened for drawing.
struct Row {
    bucket: String,
    prefix: String,
    depth: usize,
    label: String,
    is_bucket: bool,
}

impl FathomApp {
    pub(super) fn sidebar(&mut self, ui: &mut Ui) {
        self.connection_switcher(ui);
        let p = theme::palette(ui);

        let mut rows = Vec::new();
        for bucket in &self.buckets {
            rows.push(Row {
                bucket: bucket.name.clone(),
                prefix: String::new(),
                depth: 0,
                label: bucket.name.clone(),
                is_bucket: true,
            });
            self.collect_rows(&bucket.name, "", 1, &mut rows);
        }

        egui::ScrollArea::vertical()
            .id_salt("tree")
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(2.0);
                if self.buckets_loading && self.buckets.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(egui::RichText::new("Loading buckets...").color(p.muted));
                    });
                } else if let Some(error) = self.buckets_error.clone() {
                    ui.add_space(4.0);
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(10, 4))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(error).color(p.danger)).wrap(),
                            );
                            ui.add_space(4.0);
                            if ui.button("Retry").clicked() {
                                self.load_buckets();
                            }
                        });
                } else if self.active.is_some() && self.buckets.is_empty() && !self.buckets_loading
                {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("No buckets").color(p.muted));
                    });
                }
                for row in rows {
                    self.tree_row(ui, row);
                }
            });
    }

    fn collect_rows(&self, bucket: &str, prefix: &str, depth: usize, rows: &mut Vec<Row>) {
        let Some(node) = self.tree.get(&(bucket.to_string(), prefix.to_string())) else {
            return;
        };
        let Some(children) = node.children.as_ref().filter(|_| node.expanded) else {
            return;
        };
        let mut sorted = children.clone();
        sorted.sort_by(|a, b| format::compare_names(format::base_name(a), format::base_name(b)));
        for child in sorted {
            rows.push(Row {
                bucket: bucket.to_string(),
                prefix: child.clone(),
                depth,
                label: format::base_name(&child).to_string(),
                is_bucket: false,
            });
            self.collect_rows(bucket, &child, depth + 1, rows);
        }
    }

    fn tree_row(&mut self, ui: &mut Ui, row: Row) {
        let p = theme::palette(ui);
        let key = (row.bucket.clone(), row.prefix.clone());
        let (expanded, loading, leaf) = match self.tree.get(&key) {
            Some(node) => (
                node.expanded,
                node.loading,
                node.children.as_ref().is_some_and(|c| c.is_empty()),
            ),
            None => (false, false, false),
        };
        let current = self.location.bucket.as_deref() == Some(row.bucket.as_str())
            && self.location.prefix == row.prefix;

        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), theme::ROW), Sense::click());
        if current {
            ui.painter().rect_filled(rect, 0, p.selected);
        } else if response.hovered() {
            ui.painter().rect_filled(rect, 0, p.hover);
        }
        let indent = 4.0 + row.depth as f32 * 14.0;
        let chevron = Rect::from_min_size(
            pos2(rect.left() + indent, rect.center().y - 8.0),
            vec2(16.0, 16.0),
        );
        let chevron_response = ui.interact(
            chevron,
            ui.scope_id().with(("chevron", &key)),
            Sense::click(),
        );
        if loading {
            let spinner = Rect::from_center_size(chevron.center(), vec2(10.0, 10.0));
            ui.place(spinner, egui::Spinner::new().size(10.0));
        } else if !leaf {
            let color = if chevron_response.hovered() {
                p.text
            } else {
                p.faint
            };
            let icon = if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            icons::paint(ui.painter(), chevron.shrink(3.0), icon, color);
        }
        let icon_rect = Rect::from_min_size(
            pos2(chevron.right() + 2.0, rect.center().y - 8.0),
            vec2(16.0, 16.0),
        );
        let (icon, color) = if row.is_bucket {
            (Icon::Bucket, p.bucket)
        } else {
            (Icon::Folder, p.folder)
        };
        icons::paint(ui.painter(), icon_rect, icon, color);
        let text = Rect::from_min_max(
            pos2(icon_rect.right() + 5.0, rect.top()),
            pos2(rect.right() - 6.0, rect.bottom()),
        );
        let painter = ui.painter().with_clip_rect(text.intersect(ui.clip_rect()));
        painter.text(
            pos2(text.left(), text.center().y),
            egui::Align2::LEFT_CENTER,
            &row.label,
            theme::body(),
            p.text,
        );

        let toggle = |app: &mut FathomApp| {
            let node = app.tree.entry(key.clone()).or_default();
            node.expanded = !node.expanded;
            if node.expanded && node.children.is_none() && !node.loading {
                app.load_tree_children(&key.0, &key.1);
            }
        };
        if (chevron_response.clicked() || response.double_clicked()) && !leaf {
            toggle(self);
        } else if response.clicked() {
            self.navigate(
                Location {
                    bucket: Some(row.bucket.clone()),
                    prefix: row.prefix.clone(),
                },
                None,
            );
        }
        let response = response.on_hover_text(if row.is_bucket {
            row.bucket.clone()
        } else {
            row.prefix.clone()
        });
        response.context_menu(|ui| {
            if row.is_bucket {
                self.bucket_menu(ui, &row.bucket);
            } else {
                if menu_item(ui, Some(Icon::Folder), "Open", "").clicked() {
                    self.navigate(
                        Location {
                            bucket: Some(row.bucket.clone()),
                            prefix: row.prefix.clone(),
                        },
                        None,
                    );
                }
                if menu_item(ui, Some(Icon::Copy), "Copy folder path", "").clicked() {
                    ui.ctx().copy_text(row.prefix.clone());
                }
            }
        });
    }

    fn connection_switcher(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let name = self.active_connection().map(|c| c.name.clone());
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), 29.0), Sense::click());
        ui.painter().rect_filled(
            rect,
            0,
            if response.hovered() {
                p.inset
            } else {
                p.chrome
            },
        );
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            Stroke::new(1.0, p.border),
        );
        icons::paint(
            ui.painter(),
            Rect::from_min_size(
                pos2(rect.left() + 8.0, rect.center().y - 8.0),
                vec2(16.0, 16.0),
            ),
            Icon::Cloud,
            p.muted,
        );
        ui.painter().text(
            pos2(rect.left() + 31.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            name.as_deref().unwrap_or("No connection"),
            theme::strong(theme::BODY),
            p.text,
        );
        icons::paint(
            ui.painter(),
            Rect::from_min_size(
                pos2(rect.right() - 20.0, rect.center().y - 5.0),
                vec2(10.0, 10.0),
            ),
            Icon::ChevronDown,
            p.muted,
        );

        egui::Popup::menu(&response)
            .width(rect.width() - 8.0)
            .show(|ui| {
                for connection in self.connections.clone() {
                    let active = self.active.as_deref() == Some(connection.id.as_str());
                    if menu_item(
                        ui,
                        if active { Some(Icon::Check) } else { None },
                        &connection.name,
                        "",
                    )
                    .clicked()
                    {
                        self.switch_connection(&connection.id);
                    }
                }
                if !self.connections.is_empty() {
                    ui.separator();
                }
                if menu_item(ui, Some(Icon::Plus), "Add connection...", "").clicked() {
                    self.modal = Some(dialogs::Modal::connection(None));
                }
                if let Some(active) = self.active_connection().cloned() {
                    if menu_item(
                        ui,
                        Some(Icon::Settings),
                        &format!("Edit \"{}\"...", active.name),
                        "",
                    )
                    .clicked()
                    {
                        self.modal = Some(dialogs::Modal::connection(Some(&active)));
                    }
                    if menu_item(
                        ui,
                        Some(Icon::Trash),
                        &format!("Remove \"{}\"...", active.name),
                        "",
                    )
                    .clicked()
                    {
                        self.modal = Some(dialogs::Modal::remove_connection(&active));
                    }
                }
            });
    }
}

/// A menu row with an optional icon and a right-aligned shortcut.
pub(super) fn menu_item(
    ui: &mut Ui,
    icon: Option<Icon>,
    label: &str,
    shortcut: &str,
) -> egui::Response {
    menu_item_styled(ui, icon, label, shortcut, true, false)
}

pub(super) fn menu_item_styled(
    ui: &mut Ui,
    icon: Option<Icon>,
    label: &str,
    shortcut: &str,
    enabled: bool,
    danger: bool,
) -> egui::Response {
    let p = theme::palette(ui);
    // Menus fill the width they are given; keep them at a native size.
    let width = ui.available_width().clamp(220.0, 270.0);
    let (rect, response) = ui.allocate_exact_size(
        vec2(width, theme::ROW),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if enabled && response.hovered() {
        ui.painter().rect_filled(rect, 2, p.selected);
    }
    let color = if !enabled {
        p.faint
    } else if danger {
        p.danger
    } else {
        p.text
    };
    if let Some(icon) = icon {
        let icon_color = if enabled && !danger { p.muted } else { color };
        icons::paint(
            ui.painter(),
            Rect::from_min_size(
                pos2(rect.left() + 6.0, rect.center().y - 8.0),
                vec2(16.0, 16.0),
            ),
            icon,
            icon_color,
        );
    }
    ui.painter().text(
        pos2(rect.left() + 30.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        theme::body(),
        color,
    );
    if !shortcut.is_empty() {
        ui.painter().text(
            pos2(rect.right() - 8.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            shortcut,
            theme::body(),
            p.muted,
        );
    }
    if response.clicked() {
        ui.close();
    }
    response
}
