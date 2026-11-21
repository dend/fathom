//! Toolbar, status bar, transfers panel and notifications.

use eframe::egui::{self, pos2, vec2, Align, Layout, Rect, Sense, Stroke, StrokeKind, Ui};

use super::icons::{self, Icon};
use super::tree::menu_item;
use super::{theme, FathomApp, Location, ToastKind};
use crate::format;
use crate::transfers::{Kind, Status, TransferView};

pub const FILTER_ID: &str = "filter";

impl FathomApp {
    pub(super) fn toolbar(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let connected = self.active.is_some();
        let in_bucket = self.location.bucket.is_some();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 1.0;
            if icons::button(
                ui,
                Icon::ArrowLeft,
                "Back (Alt+Left)",
                self.history_index > 0,
            )
            .clicked()
            {
                self.go_back();
            }
            let forward = self.history_index + 1 < self.history.len();
            if icons::button(ui, Icon::ArrowRight, "Forward (Alt+Right)", forward).clicked() {
                self.go_forward();
            }
            if icons::button(ui, Icon::ArrowUp, "Up one level (Backspace)", in_bucket).clicked() {
                self.go_up();
            }
            if icons::button(ui, Icon::Refresh, "Refresh (F5)", connected).clicked() {
                self.refresh();
            }
            ui.add_space(5.0);

            // Right-hand group first, so the address bar can take what is left.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 1.0;
                if icons::toggle(
                    ui,
                    Icon::Panel,
                    "Show or hide the preview pane",
                    self.show_preview,
                )
                .clicked()
                {
                    self.show_preview = !self.show_preview;
                }
                let (divider, _) = ui.allocate_exact_size(vec2(9.0, 16.0), Sense::hover());
                ui.painter().vline(
                    divider.center().x,
                    divider.y_range(),
                    Stroke::new(1.0, p.border_strong),
                );
                if icons::tool(ui, Icon::FolderPlus, "New folder", in_bucket).clicked() {
                    self.new_folder();
                }
                let more = icons::button(ui, Icon::ChevronDown, "More upload options", in_bucket);
                egui::Popup::menu(&more).show(|ui| {
                    if menu_item(ui, Some(Icon::Upload), "Upload files...", "").clicked() {
                        self.upload_pick(false);
                    }
                    if menu_item(ui, Some(Icon::Folder), "Upload folder...", "").clicked() {
                        self.upload_pick(true);
                    }
                });
                if icons::tool(ui, Icon::Upload, "Upload", in_bucket)
                    .on_hover_text("Upload files")
                    .clicked()
                {
                    self.upload_pick(false);
                }
                ui.add_space(6.0);
                self.filter_box(ui, connected);
                ui.add_space(6.0);
                self.breadcrumbs(ui);
            });
        });
    }

    fn filter_box(&mut self, ui: &mut Ui, enabled: bool) {
        let p = theme::palette(ui);
        let (rect, _) = ui.allocate_exact_size(vec2(190.0, 26.0), Sense::hover());
        let id = egui::Id::unique(FILTER_ID);
        let focused = ui.ctx().memory(|m| m.has_focus(id));
        ui.painter().rect(
            rect,
            3,
            p.bg,
            Stroke::new(1.0, if focused { p.accent } else { p.border_strong }),
            StrokeKind::Inside,
        );
        icons::paint(
            ui.painter(),
            Rect::from_min_size(
                pos2(rect.left() + 7.0, rect.center().y - 7.0),
                vec2(14.0, 14.0),
            ),
            Icon::Search,
            p.faint,
        );
        let edit_rect = Rect::from_min_max(
            pos2(rect.left() + 26.0, rect.top() + 1.0),
            pos2(rect.right() - 4.0, rect.bottom() - 1.0),
        );
        let response = ui.place(
            edit_rect,
            egui::TextEdit::singleline(&mut self.filter)
                .id(id)
                .hint_text("Filter")
                .frame(egui::Frame::NONE)
                .margin(egui::Margin {
                    left: 0,
                    right: 0,
                    top: 5,
                    bottom: 3,
                })
                .desired_width(edit_rect.width())
                .interactive(enabled),
        );
        if response.changed() {
            self.view_dirty = true;
        }
        if response.has_focus() {
            let (escape, down) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Escape),
                    i.key_pressed(egui::Key::ArrowDown),
                )
            });
            if escape {
                self.filter.clear();
                self.view_dirty = true;
                response.surrender_focus();
            } else if down {
                response.surrender_focus();
                if self.selection.is_empty() {
                    self.move_cursor(1, false);
                }
            }
        }
    }

    fn breadcrumbs(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
        ui.painter().rect(
            rect,
            3,
            p.bg,
            Stroke::new(1.0, p.border_strong),
            StrokeKind::Inside,
        );
        let Some(connection) = self.active_connection().cloned() else {
            return;
        };

        let mut crumbs: Vec<(String, Option<Icon>, Location)> = vec![(
            connection.name.clone(),
            Some(Icon::Cloud),
            Location::default(),
        )];
        if let Some(bucket) = &self.location.bucket {
            crumbs.push((
                bucket.clone(),
                Some(Icon::Bucket),
                Location {
                    bucket: Some(bucket.clone()),
                    prefix: String::new(),
                },
            ));
            let parts: Vec<&str> = self
                .location
                .prefix
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            for (index, part) in parts.iter().enumerate() {
                crumbs.push((
                    part.to_string(),
                    None,
                    Location {
                        bucket: Some(bucket.clone()),
                        prefix: format!("{}/", parts[..=index].join("/")),
                    },
                ));
            }
        }

        // Lay out right to left so the end of a deep path stays visible.
        let font = theme::body();
        let mut widths: Vec<f32> = crumbs
            .iter()
            .map(|(label, icon, _)| {
                let text = ui
                    .painter()
                    .layout_no_wrap(label.clone(), font.clone(), p.text)
                    .size()
                    .x
                    .min(220.0);
                text + 12.0 + if icon.is_some() { 19.0 } else { 0.0 }
            })
            .collect();
        let separator = 12.0;
        let available = rect.width() - 6.0;
        let mut first = 0;
        while first + 1 < crumbs.len()
            && widths[first..].iter().sum::<f32>() + separator * (crumbs.len() - first - 1) as f32
                > available
        {
            first += 1;
        }
        widths.drain(..first);

        let painter = ui.painter().with_clip_rect(rect.shrink(1.0));
        let mut x = rect.left() + 3.0;
        let last = crumbs.len() - 1;
        for (index, ((label, icon, target), width)) in
            crumbs.into_iter().skip(first).zip(widths).enumerate()
        {
            let index = index + first;
            if index > first {
                icons::paint(
                    &painter,
                    Rect::from_center_size(
                        pos2(x + separator / 2.0, rect.center().y),
                        vec2(10.0, 10.0),
                    ),
                    Icon::ChevronRight,
                    p.faint,
                );
                x += separator;
            }
            let crumb =
                Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(width, rect.height() - 6.0));
            let response = ui.interact(crumb, ui.scope_id().with(("crumb", index)), Sense::click());
            if response.hovered() {
                painter.rect_filled(crumb, 2, p.hover);
            }
            let mut text_x = crumb.left() + 6.0;
            if let Some(icon) = icon {
                icons::paint(
                    &painter,
                    Rect::from_min_size(pos2(text_x, crumb.center().y - 7.0), vec2(14.0, 14.0)),
                    icon,
                    p.muted,
                );
                text_x += 19.0;
            }
            let text_rect = Rect::from_min_max(
                pos2(text_x, crumb.top()),
                pos2(crumb.right() - 6.0, crumb.bottom()),
            );
            painter.with_clip_rect(text_rect.intersect(rect)).text(
                pos2(text_x, crumb.center().y),
                egui::Align2::LEFT_CENTER,
                &label,
                font.clone(),
                p.text,
            );
            if response.clicked() {
                if index == last {
                    self.refresh();
                } else {
                    self.navigate(target, None);
                }
            }
            x += width;
        }
    }

    pub(super) fn status_bar(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let full = ui.max_rect();
        ui.painter()
            .hline(full.x_range(), full.top(), Stroke::new(1.0, p.border));

        let mut parts = Vec::new();
        if self.active.is_some() {
            let total = self.listing.items.len();
            let shown = self.view.len();
            let noun = if self.location.bucket.is_some() {
                "item"
            } else {
                "bucket"
            };
            if self.listing.loading {
                parts.push(if total > 0 {
                    format!("Loading... {}", format::plural(total, noun))
                } else {
                    "Loading...".into()
                });
            } else if shown != total {
                parts.push(format!(
                    "{} of {}",
                    format::thousands(shown as u64),
                    format::plural(total, noun)
                ));
            } else {
                parts.push(format::plural(total, noun));
            }
            if self.listing.capped {
                parts.push("list truncated".into());
            }
            let selected = self.selected_items();
            if !selected.is_empty() {
                let size: u64 = selected.iter().map(|i| i.size).sum();
                let files = selected.iter().any(|i| i.is_file());
                let suffix = if files {
                    format!(" ({})", format::size(size))
                } else {
                    String::new()
                };
                parts.push(format!(
                    "{} selected{suffix}",
                    format::thousands(selected.len() as u64)
                ));
            }
        }

        ui.horizontal_centered(|ui| {
            ui.label(
                egui::RichText::new(parts.join(", "))
                    .color(p.muted)
                    .font(theme::small()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.transfers_button(ui);
            });
        });
    }

    fn transfer_summary(&self) -> (usize, usize, f32) {
        let mut pending = 0;
        let mut failed = 0;
        let (mut total, mut done) = (0u64, 0u64);
        for transfer in self.transfer_views.values() {
            match transfer.status {
                Status::Active | Status::Queued => pending += 1,
                Status::Failed => failed += 1,
                _ => {}
            }
            if transfer.id >= self.batch_start
                && !matches!(transfer.status, Status::Failed | Status::Cancelled)
            {
                total += transfer.size;
                done += if transfer.status == Status::Done {
                    transfer.size
                } else {
                    transfer.done.min(transfer.size)
                };
            }
        }
        let fraction = if total > 0 {
            done as f32 / total as f32
        } else if pending > 0 {
            0.0
        } else {
            1.0
        };
        (pending, failed, fraction)
    }

    fn transfers_button(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let (pending, failed, fraction) = self.transfer_summary();
        let label = if pending > 0 {
            format!(
                "{}, {}%",
                format::plural(pending, "transfer"),
                (fraction * 100.0).floor()
            )
        } else if failed > 0 {
            format!("{} failed", format::plural(failed, "transfer"))
        } else {
            "Transfers".into()
        };
        let color = if failed > 0 && pending == 0 {
            p.danger
        } else if pending > 0 {
            p.text
        } else {
            p.muted
        };
        let galley = ui.painter().layout_no_wrap(label, theme::small(), color);
        let size = vec2(galley.size().x + 34.0, ui.available_height());
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        if response.hovered() || self.show_transfers {
            ui.painter().rect_filled(rect, 0, p.hover);
        }
        ui.painter()
            .vline(rect.left(), rect.y_range(), Stroke::new(1.0, p.border));
        icons::paint(
            ui.painter(),
            Rect::from_min_size(
                pos2(rect.left() + 9.0, rect.center().y - 6.0),
                vec2(12.0, 12.0),
            ),
            Icon::Transfers,
            color,
        );
        ui.painter().galley(
            pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        if pending > 0 {
            let bar = Rect::from_min_max(
                pos2(rect.left(), rect.bottom() - 2.0),
                pos2(rect.left() + rect.width() * fraction, rect.bottom()),
            );
            ui.painter().rect_filled(bar, 0, p.accent);
        }
        if response.on_hover_text("Show or hide transfers").clicked() {
            self.show_transfers = !self.show_transfers;
        }
    }

    pub(super) fn transfers_panel(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let (pending, failed, _) = self.transfer_summary();
        let done = self
            .transfer_views
            .values()
            .filter(|t| t.status == Status::Done)
            .count();
        let cancelled = self
            .transfer_views
            .values()
            .filter(|t| t.status == Status::Cancelled)
            .count();

        let header = Rect::from_min_size(ui.max_rect().min, vec2(ui.max_rect().width(), 29.0));
        ui.painter().rect_filled(header, 0, p.chrome);
        ui.painter().hline(
            header.x_range(),
            header.bottom() - 0.5,
            Stroke::new(1.0, p.border),
        );
        ui.painter().hline(
            header.x_range(),
            header.top() + 0.5,
            Stroke::new(1.0, p.border),
        );
        let mut header_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(header.shrink2(vec2(8.0, 0.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        header_ui.label(egui::RichText::new("Transfers").font(theme::strong(theme::BODY)));
        let mut parts = Vec::new();
        if pending > 0 {
            parts.push(format!("{} in progress", format::thousands(pending as u64)));
        }
        if done > 0 {
            parts.push(format!("{} done", format::thousands(done as u64)));
        }
        if failed > 0 {
            parts.push(format!("{} failed", format::thousands(failed as u64)));
        }
        if cancelled > 0 {
            parts.push(format!("{} cancelled", format::thousands(cancelled as u64)));
        }
        header_ui.label(egui::RichText::new(parts.join(", ")).color(p.muted));
        header_ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icons::button(ui, Icon::Close, "Hide transfers", true).clicked() {
                self.show_transfers = false;
            }
            if ui
                .add_enabled(
                    self.transfer_views.len() > pending,
                    egui::Button::new("Clear finished").small(),
                )
                .clicked()
            {
                self.transfers.clear_finished();
                self.transfer_views
                    .retain(|_, t| matches!(t.status, Status::Queued | Status::Active));
            }
            if ui
                .add_enabled(pending > 0, egui::Button::new("Cancel all").small())
                .clicked()
            {
                self.transfers.cancel(None);
            }
        });

        let body = Rect::from_min_max(pos2(header.left(), header.bottom()), ui.max_rect().max);
        let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(body));
        if self.transfer_views.is_empty() {
            list_ui.add_space(16.0);
            list_ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("Uploads and downloads will show up here.").color(p.faint),
                )
            });
            return;
        }
        let order = |t: &TransferView| match t.status {
            Status::Active => 0,
            Status::Queued => 1,
            Status::Failed => 2,
            Status::Cancelled => 3,
            Status::Done => 4,
        };
        let mut sorted: Vec<TransferView> = self.transfer_views.values().cloned().collect();
        sorted.sort_by(|a, b| {
            order(a).cmp(&order(b)).then_with(|| {
                if order(a) >= 2 {
                    b.id.cmp(&a.id)
                } else {
                    a.id.cmp(&b.id)
                }
            })
        });
        // As in the file list, show_rows reads the spacing from this ui.
        list_ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .id_salt("transfers")
            .auto_shrink(false)
            .show_rows(&mut list_ui, theme::ROW, sorted.len(), |ui, range| {
                for transfer in &sorted[range] {
                    self.transfer_row(ui, transfer);
                }
            });
    }

    fn transfer_row(&mut self, ui: &mut Ui, t: &TransferView) {
        let p = theme::palette(ui);
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), theme::ROW), Sense::hover());
        if response.hovered() {
            ui.painter().rect_filled(rect, 0, p.hover);
        }
        let upload = t.kind == Kind::Upload;
        let kind_color = if t.status == Status::Active {
            p.accent
        } else {
            p.muted
        };
        icons::paint(
            ui.painter(),
            Rect::from_min_size(
                pos2(rect.left() + 8.0, rect.center().y - 7.0),
                vec2(14.0, 14.0),
            ),
            if upload { Icon::Upload } else { Icon::Download },
            kind_color,
        );

        let status = match t.status {
            Status::Queued => "Waiting".to_string(),
            Status::Active => format!("{} of {}", format::size(t.done), format::size(t.size)),
            Status::Done => format::size(t.size),
            Status::Cancelled => "Cancelled".to_string(),
            Status::Failed => {
                if t.error.is_empty() {
                    "Failed".to_string()
                } else {
                    t.error.clone()
                }
            }
        };
        let action_rect = Rect::from_min_size(
            pos2(rect.right() - 26.0, rect.center().y - 10.0),
            vec2(20.0, 20.0),
        );
        let status_rect = Rect::from_min_max(
            pos2(action_rect.left() - 166.0, rect.top()),
            pos2(action_rect.left() - 8.0, rect.bottom()),
        );
        let progress_rect = Rect::from_min_size(
            pos2(status_rect.left() - 170.0, rect.center().y - 3.0),
            vec2(160.0, 6.0),
        );
        let text_rect = Rect::from_min_max(
            pos2(rect.left() + 30.0, rect.top()),
            pos2(progress_rect.left() - 10.0, rect.bottom()),
        );

        let name = format::base_name(&t.key).to_string();
        let place = if upload {
            format!("to {}/{}", t.bucket, format::parent_prefix(&t.key))
        } else {
            format!("to {}", t.path)
        };
        let painter = ui.painter().with_clip_rect(text_rect);
        let name_galley = painter.layout_no_wrap(name, theme::body(), p.text);
        let name_width = name_galley.size().x.min(text_rect.width() * 0.55);
        painter
            .with_clip_rect(Rect::from_min_size(
                text_rect.min,
                vec2(name_width, text_rect.height()),
            ))
            .galley(
                pos2(
                    text_rect.left(),
                    rect.center().y - name_galley.size().y / 2.0,
                ),
                name_galley,
                p.text,
            );
        painter.text(
            pos2(text_rect.left() + name_width + 8.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            place,
            theme::small(),
            p.faint,
        );

        if matches!(t.status, Status::Active | Status::Queued) {
            ui.painter().rect_filled(progress_rect, 0, p.pressed);
            let fraction = if t.size > 0 {
                (t.done as f32 / t.size as f32).min(1.0)
            } else {
                0.0
            };
            if t.status == Status::Active {
                let fill = Rect::from_min_size(
                    progress_rect.min,
                    vec2(progress_rect.width() * fraction, progress_rect.height()),
                );
                ui.painter().rect_filled(fill, 0, p.accent);
            }
        }
        let status_color = if t.status == Status::Failed {
            p.danger
        } else {
            p.muted
        };
        let status_painter = ui.painter().with_clip_rect(status_rect);
        status_painter.text(
            pos2(status_rect.right(), rect.center().y),
            egui::Align2::RIGHT_CENTER,
            &status,
            theme::small(),
            status_color,
        );
        if t.status == Status::Failed {
            ui.interact(
                status_rect,
                ui.scope_id().with(("status", t.id)),
                Sense::hover(),
            )
            .on_hover_text(&t.error);
        }

        let mut action_ui = ui.new_child(egui::UiBuilder::new().max_rect(action_rect));
        match t.status {
            Status::Done => icons::paint(
                ui.painter(),
                action_rect.shrink(3.0),
                Icon::Check,
                p.success,
            ),
            Status::Failed | Status::Cancelled => {
                if icons::button(&mut action_ui, Icon::Retry, "Try again", true).clicked() {
                    self.transfers.retry(t.id);
                }
            }
            Status::Active | Status::Queued => {
                if icons::button(&mut action_ui, Icon::Close, "Cancel", true).clicked() {
                    self.transfers.cancel(Some(t.id));
                }
            }
        }
    }

    pub(super) fn toasts_overlay(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() && self.ops.is_empty() {
            return;
        }
        let p = theme::palette_of(ctx);
        let bottom = if self.show_transfers { 240.0 } else { 34.0 };
        egui::Area::new(egui::Id::unique("toasts"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::RIGHT_BOTTOM, vec2(-10.0, -bottom))
            .show(ctx, |ui| {
                ui.set_max_width(400.0);
                let frame = egui::Frame::new()
                    .fill(p.raised)
                    .stroke(Stroke::new(1.0, p.border_strong))
                    .corner_radius(3)
                    .shadow(ui.visuals().popup_shadow)
                    .inner_margin(egui::Margin::symmetric(10, 7));
                let mut cancel = None;
                for op in &self.ops {
                    frame.show(ui, |ui| {
                        ui.set_width(340.0);
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(13.0));
                            ui.label(&op.label);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("Cancel").clicked() {
                                    cancel = Some(op.cancel.clone());
                                }
                                if op.total > 0 {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} / {}",
                                            format::thousands(op.done as u64),
                                            format::thousands(op.total as u64)
                                        ))
                                        .color(p.muted)
                                        .font(theme::small()),
                                    );
                                }
                            });
                        });
                        let fraction = if op.total > 0 {
                            op.done as f32 / op.total as f32
                        } else {
                            0.0
                        };
                        let (bar, _) =
                            ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
                        ui.painter().rect_filled(bar, 0, p.pressed);
                        ui.painter().rect_filled(
                            Rect::from_min_size(bar.min, vec2(bar.width() * fraction, 4.0)),
                            0,
                            p.accent,
                        );
                    });
                    ui.add_space(6.0);
                }
                if let Some(flag) = cancel {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                let mut dismissed = None;
                for toast in &self.toasts {
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (icon, color) = match toast.kind {
                                ToastKind::Info => (Icon::Info, p.muted),
                                ToastKind::Success => (Icon::Check, p.success),
                                ToastKind::Error => (Icon::Alert, p.danger),
                            };
                            let (rect, _) =
                                ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                            icons::paint(ui.painter(), rect, icon, color);
                            ui.add(egui::Label::new(&toast.text).wrap());
                            if icons::button(ui, Icon::Close, "Dismiss", true).clicked() {
                                dismissed = Some(toast.id);
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
                if let Some(id) = dismissed {
                    self.toasts.retain(|t| t.id != id);
                }
            });
    }
}
