//! File previews: the pane next to the list and the full-window viewer, which
//! doubles as a text editor. Video and audio play in the system player.

use std::sync::Arc;

use eframe::egui::{
    self, pos2, vec2, Align, Color32, Key, Layout, Rect, Sense, Stroke, StrokeKind, TextureHandle,
    Ui,
};

use super::dialogs::{Modal, ViewerIntent};
use super::icons::{self, Icon};
use super::list::{color_for, icon_for};
use super::{theme, Event, FathomApp, Item, ItemKind, ToastKind};
use crate::format::{self, Kind};
use crate::s3::S3Client;

const PANE_TEXT_BYTES: u64 = 64 * 1024;
const VIEWER_TEXT_BYTES: u64 = 2 * 1024 * 1024;
/// Images bigger than this are not downloaded for a preview.
const MAX_IMAGE_BYTES: u64 = 40 * 1024 * 1024;

pub enum Content {
    Image {
        texture: TextureHandle,
        width: u32,
        height: u32,
    },
    Text {
        text: String,
        size: u64,
        truncated: bool,
    },
    Binary,
}

pub struct Loaded {
    signature: String,
    result: Result<Content, String>,
}

#[derive(Default)]
pub struct Pane {
    signature: String,
    content: Option<Result<Content, String>>,
}

pub struct Viewer {
    pub item: Item,
    signature: String,
    content: Option<Result<Content, String>>,
    force_text: bool,
    /// The text being edited, and the text as loaded.
    editing: Option<String>,
    original: String,
    saving: bool,
}

impl Viewer {
    pub fn is_dirty(&self) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|text| *text != self.original)
    }
}

/// What a file can be previewed as.
fn preview_kind(item: &Item, force_text: bool) -> Option<Kind> {
    let kind = format::kind_of(&item.name);
    if force_text || kind.is_text() {
        Some(Kind::Text)
    } else if kind == Kind::Image && format::can_decode_image(&item.name) {
        Some(Kind::Image)
    } else {
        None
    }
}

fn load(
    client: Arc<S3Client>,
    ctx: egui::Context,
    bucket: String,
    item: Item,
    kind: Kind,
    max_text: u64,
    max_side: u32,
) -> Result<Content, String> {
    if kind == Kind::Image {
        if item.size > MAX_IMAGE_BYTES {
            return Err(format!(
                "Images over {} are not previewed",
                format::size(MAX_IMAGE_BYTES)
            ));
        }
        let (bytes, _) = client
            .read_head(&bucket, &item.key, MAX_IMAGE_BYTES)
            .map_err(|e| e.to_string())?;
        let image = image::load_from_memory(&bytes)
            .map_err(|_| "This image could not be decoded".to_string())?;
        let (width, height) = (image.width(), image.height());
        let image = if width.max(height) > max_side {
            image.thumbnail(max_side, max_side)
        } else {
            image
        };
        let rgba = image.to_rgba8();
        let pixels = egui::ColorImage::from_rgba_unmultiplied(
            [rgba.width() as usize, rgba.height() as usize],
            &rgba,
        );
        let texture = ctx.load_texture(&item.key, pixels, egui::TextureOptions::LINEAR);
        return Ok(Content::Image {
            texture,
            width,
            height,
        });
    }
    let (bytes, size) = client
        .read_head(&bucket, &item.key, max_text)
        .map_err(|e| e.to_string())?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Ok(Content::Binary);
    }
    Ok(Content::Text {
        truncated: size > bytes.len() as u64,
        text: String::from_utf8_lossy(&bytes).into_owned(),
        size,
    })
}

/// Paints an image texture fitted into `rect`.
fn paint_image(ui: &Ui, rect: Rect, texture: &TextureHandle) {
    let size = texture.size_vec2();
    let scale = (rect.width() / size.x).min(rect.height() / size.y).min(1.0);
    let target = Rect::from_center_size(rect.center(), size * scale);
    ui.painter().image(
        texture.id(),
        target,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

impl FathomApp {
    fn signature(&self, item: &Item, mode: &str) -> String {
        format!(
            "{}|{}|{}|{}|{}|{mode}",
            self.active.as_deref().unwrap_or_default(),
            self.location.bucket.as_deref().unwrap_or_default(),
            item.key,
            item.etag,
            item.size
        )
    }

    /// Starts loading the preview for the current selection if it changed.
    pub(super) fn sync_preview(&mut self, ctx: &egui::Context) {
        if !self.show_preview {
            return;
        }
        let selected = self.selected_items();
        let item = match selected.as_slice() {
            [item] if item.is_file() => item.clone(),
            _ => {
                self.preview.signature.clear();
                self.preview.content = None;
                return;
            }
        };
        let signature = self.signature(&item, "pane");
        if signature == self.preview.signature {
            return;
        }
        self.preview.signature = signature.clone();
        self.preview.content = None;
        if let Some(kind) = preview_kind(&item, false) {
            self.start_load(ctx, signature, item, kind, PANE_TEXT_BYTES, 1024);
        }
    }

    fn start_load(
        &self,
        ctx: &egui::Context,
        signature: String,
        item: Item,
        kind: Kind,
        max_text: u64,
        max_side: u32,
    ) {
        let (Ok(client), Some(bucket)) = (self.client(), self.location.bucket.clone()) else {
            return;
        };
        let ctx = ctx.clone();
        self.spawn(move || {
            let result = load(client, ctx, bucket, item, kind, max_text, max_side);
            Event::Preview(Loaded { signature, result })
        });
    }

    pub(super) fn preview_loaded(&mut self, loaded: Loaded) {
        if loaded.signature == self.preview.signature {
            self.preview.content = Some(loaded.result);
        } else if let Some(viewer) = self
            .viewer
            .as_mut()
            .filter(|v| v.signature == loaded.signature)
        {
            if let Ok(Content::Text { text, .. }) = &loaded.result {
                viewer.original = text.clone();
            }
            viewer.content = Some(loaded.result);
        }
    }

    // ----- pane ------------------------------------------------------------------------

    pub(super) fn preview_pane(&mut self, ui: &mut Ui) {
        let p = theme::palette(ui);
        let selected = self.selected_items();
        if selected.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("Select a file to preview it").color(p.faint))
            });
            return;
        }
        if selected.len() > 1 {
            let files = selected.iter().filter(|i| i.is_file()).count();
            let size: u64 = selected.iter().map(|i| i.size).sum();
            ui.add_space(ui.available_height() / 2.0 - 24.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} selected",
                        format::plural(selected.len(), "item")
                    ))
                    .font(theme::strong(theme::BODY)),
                );
                if files > 0 {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}, {}",
                            format::plural(files, "file"),
                            format::size(size)
                        ))
                        .color(p.muted),
                    );
                }
            });
            return;
        }
        let item = selected[0].clone();
        egui::ScrollArea::vertical()
            .id_salt("preview")
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                self.media_box(ui, &item);
                ui.add(
                    egui::Label::new(egui::RichText::new(&item.name).font(theme::strong(13.5)))
                        .wrap(),
                );
                self.details(ui, &item);
                if item.kind == ItemKind::File {
                    self.public_url_row(ui, &item);
                    ui.horizontal(|ui| {
                        let media = format::kind_of(&item.name).is_media();
                        if media {
                            if ui
                                .button("Play")
                                .on_hover_text("Play in the default player")
                                .clicked()
                            {
                                self.play(&item);
                            }
                        } else if ui.button("Open").clicked() {
                            self.open_viewer(item.clone(), false);
                        }
                        if ui.button("Download").clicked() {
                            self.download(vec![item.clone()]);
                        }
                        if icons::button(ui, Icon::Info, "Properties", true).clicked() {
                            self.show_properties(&item);
                        }
                    });
                } else if item.kind == ItemKind::Bucket && ui.button("Public domain...").clicked() {
                    self.modal = Some(Modal::domain(
                        &item.name,
                        self.domain_for(&item.name),
                        Vec::new(),
                    ));
                }
            });
    }

    fn media_box(&mut self, ui: &mut Ui, item: &Item) {
        let p = theme::palette(ui);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 200.0), Sense::hover());
        ui.painter().rect(
            rect,
            0,
            p.inset,
            Stroke::new(1.0, p.border),
            StrokeKind::Inside,
        );
        let inner = rect.shrink(1.0);
        let notice = |ui: &Ui, icon: Icon, text: &str| {
            let icon_rect =
                Rect::from_center_size(inner.center() - vec2(0.0, 14.0), vec2(34.0, 34.0));
            icons::paint(ui.painter(), icon_rect, icon, p.faint);
            ui.painter().text(
                inner.center() + vec2(0.0, 22.0),
                egui::Align2::CENTER_CENTER,
                text,
                theme::body(),
                p.muted,
            );
        };
        if item.kind != ItemKind::File {
            icons::paint(
                ui.painter(),
                Rect::from_center_size(inner.center(), vec2(40.0, 40.0)),
                icon_for(item),
                color_for(item, p),
            );
            return;
        }
        let kind = format::kind_of(&item.name);
        if kind.is_media() {
            let icon_rect =
                Rect::from_center_size(inner.center() - vec2(0.0, 12.0), vec2(36.0, 36.0));
            icons::paint(ui.painter(), icon_rect, icon_for(item), color_for(item, p));
            let play = Rect::from_center_size(inner.center() + vec2(0.0, 30.0), vec2(92.0, 26.0));
            let response = ui.interact(play, ui.scope_id().with("play"), Sense::click());
            ui.painter().rect(
                play,
                3,
                if response.hovered() {
                    p.chrome
                } else {
                    p.raised
                },
                Stroke::new(1.0, p.border_strong),
                StrokeKind::Inside,
            );
            icons::paint(
                ui.painter(),
                Rect::from_min_size(
                    pos2(play.left() + 12.0, play.center().y - 7.0),
                    vec2(14.0, 14.0),
                ),
                Icon::Play,
                p.text,
            );
            ui.painter().text(
                pos2(play.left() + 34.0, play.center().y),
                egui::Align2::LEFT_CENTER,
                "Play",
                theme::body(),
                p.text,
            );
            if response
                .on_hover_text("Opens in your default player")
                .clicked()
            {
                self.play(item);
            }
            return;
        }
        match &self.preview.content {
            None if preview_kind(item, false).is_some() => {
                ui.place(
                    Rect::from_center_size(inner.center(), vec2(18.0, 18.0)),
                    egui::Spinner::new().size(18.0),
                );
            }
            None => notice(ui, icon_for(item), "No preview for this file type"),
            Some(Err(error)) => notice(ui, Icon::Alert, error),
            Some(Ok(Content::Binary)) => notice(ui, Icon::File, "This file does not contain text"),
            Some(Ok(Content::Image { texture, .. })) => paint_image(ui, inner.shrink(6.0), texture),
            Some(Ok(Content::Text { text, .. })) => {
                let painter = ui.painter().with_clip_rect(inner);
                ui.painter().rect_filled(inner, 0, p.bg);
                let galley = painter.layout(
                    text.chars().take(4000).collect(),
                    theme::mono(11.0),
                    p.text,
                    f32::INFINITY,
                );
                painter.galley(inner.min + vec2(8.0, 6.0), galley, p.text);
            }
        }
    }

    fn details(&self, ui: &mut Ui, item: &Item) {
        let p = theme::palette(ui);
        let mut rows: Vec<(&str, String)> = vec![("Type", super::type_of(item))];
        match item.kind {
            ItemKind::File => {
                rows.push(("Size", format::size(item.size)));
                if let Some(modified) = item.modified {
                    rows.push(("Modified", format::date(modified)));
                }
                if let Some(Ok(Content::Image { width, height, .. })) = &self.preview.content {
                    rows.push(("Dimensions", format!("{width} x {height}")));
                }
            }
            ItemKind::Bucket => {
                if let Some(created) = item.modified {
                    rows.push(("Created", format::date(created)));
                }
                rows.push((
                    "Public domain",
                    self.domain_for(&item.name)
                        .unwrap_or_else(|| "Not set".into()),
                ));
            }
            ItemKind::Folder => {}
        }
        egui::Grid::new("details")
            .num_columns(2)
            .spacing(vec2(12.0, 4.0))
            .show(ui, |ui| {
                for (label, value) in rows {
                    ui.label(egui::RichText::new(label).color(p.muted));
                    ui.add(egui::Label::new(value).wrap());
                    ui.end_row();
                }
            });
    }

    fn public_url_row(&mut self, ui: &mut Ui, item: &Item) {
        let p = theme::palette(ui);
        let Some(bucket) = self.location.bucket.clone() else {
            return;
        };
        ui.separator();
        ui.label(egui::RichText::new("Public URL").color(p.muted));
        match self.domain_for(&bucket) {
            Some(base) => {
                let url = format::public_url(&base, &item.key);
                let (rect, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                ui.painter().rect(
                    rect,
                    3,
                    p.bg,
                    Stroke::new(1.0, p.border_strong),
                    StrokeKind::Inside,
                );
                let copy = Rect::from_center_size(
                    pos2(rect.right() - 13.0, rect.center().y),
                    vec2(20.0, 20.0),
                );
                let text = Rect::from_min_max(
                    pos2(rect.left() + 7.0, rect.top()),
                    pos2(copy.left() - 4.0, rect.bottom()),
                );
                ui.painter().with_clip_rect(text).text(
                    pos2(text.left(), rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &url,
                    theme::small(),
                    p.muted,
                );
                let mut copy_ui = ui.new_child(egui::UiBuilder::new().max_rect(copy));
                if icons::inline_button(&mut copy_ui, Icon::Copy, "Copy public URL").clicked() {
                    ui.ctx().copy_text(url);
                    self.toast("Public URL copied", ToastKind::Success);
                }
            }
            None => {
                if ui
                    .link("Set a public domain to get shareable URLs")
                    .clicked()
                {
                    self.modal = Some(Modal::domain(&bucket, None, Vec::new()));
                }
            }
        }
    }

    // ----- viewer ------------------------------------------------------------------------

    pub(super) fn open_viewer(&mut self, item: Item, force_text: bool) {
        if self.viewer.as_ref().is_some_and(Viewer::is_dirty) {
            self.modal = Some(Modal::discard(ViewerIntent::Open(item)));
            return;
        }
        let signature = self.signature(&item, if force_text { "viewer-text" } else { "viewer" });
        let kind = preview_kind(&item, force_text);
        self.viewer = Some(Viewer {
            item: item.clone(),
            signature: signature.clone(),
            content: None,
            force_text,
            editing: None,
            original: String::new(),
            saving: false,
        });
        if let Some(kind) = kind {
            let ctx = self.ctx.clone();
            self.start_load(&ctx, signature, item, kind, VIEWER_TEXT_BYTES, 4096);
        }
    }

    pub(super) fn close_viewer(&mut self, force: bool) {
        if !force && self.viewer.as_ref().is_some_and(Viewer::is_dirty) {
            self.modal = Some(Modal::discard(ViewerIntent::Close));
            return;
        }
        self.viewer = None;
    }

    fn step_viewer(&mut self, delta: isize) {
        let Some(current) = self.viewer.as_ref().map(|v| v.item.key.clone()) else {
            return;
        };
        let files: Vec<Item> = self
            .view
            .iter()
            .map(|&i| &self.listing.items[i])
            .filter(|i| i.is_file())
            .cloned()
            .collect();
        let Some(index) = files.iter().position(|i| i.key == current) else {
            return;
        };
        let next = index as isize + delta;
        if next < 0 || next as usize >= files.len() {
            return;
        }
        let item = files[next as usize].clone();
        self.select_only(&item.key);
        self.reveal = Some(item.key.clone());
        self.open_viewer(item, false);
    }

    pub(super) fn viewer_keys(&mut self, ctx: &egui::Context) {
        if self.modal.is_some() {
            return;
        }
        let editing = self.viewer.as_ref().is_some_and(|v| v.editing.is_some());
        let shortcut = |modifiers, key| {
            ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(modifiers, key)))
        };
        if shortcut(egui::Modifiers::NONE, Key::Escape) {
            self.close_viewer(false);
        } else if editing {
            if shortcut(egui::Modifiers::COMMAND, Key::S) {
                self.save_text();
            }
        } else if !ctx.egui_wants_keyboard_input() {
            if shortcut(egui::Modifiers::NONE, Key::ArrowLeft) {
                self.step_viewer(-1);
            } else if shortcut(egui::Modifiers::NONE, Key::ArrowRight) {
                self.step_viewer(1);
            }
        }
    }

    fn save_text(&mut self) {
        let (Ok(client), Some(bucket)) = (self.client(), self.location.bucket.clone()) else {
            return;
        };
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        let Some(text) = viewer.editing.clone() else {
            return;
        };
        viewer.saving = true;
        let key = viewer.item.key.clone();
        self.spawn(move || {
            // Keep the content type of an object that is being edited.
            let existing = client
                .head(&bucket, &key)
                .map(|info| info.content_type)
                .unwrap_or_default();
            let content_type = match existing.as_str() {
                "" | "application/octet-stream" => match crate::s3::content_type_for(&key) {
                    "application/octet-stream" => "text/plain; charset=utf-8".to_string(),
                    guessed => guessed.to_string(),
                },
                existing => existing.to_string(),
            };
            let result = client
                .put_bytes(&bucket, &key, text.as_bytes(), &content_type)
                .map_err(|e| e.to_string());
            Event::Dialog(super::dialogs::Reply::TextSaved { key, text, result })
        });
    }

    pub(super) fn text_saved(&mut self, key: String, text: String, result: Result<(), String>) {
        if let Some(viewer) = self.viewer.as_mut().filter(|v| v.item.key == key) {
            viewer.saving = false;
            if result.is_ok() {
                viewer.original = text;
            }
        }
        match result {
            Ok(()) => {
                self.toast(
                    format!("Saved \"{}\"", format::base_name(&key)),
                    ToastKind::Success,
                );
                self.load_listing(true, None);
            }
            Err(error) => self.toast(error, ToastKind::Error),
        }
    }

    pub(super) fn viewer_overlay(&mut self, ctx: &egui::Context) {
        if self.viewer.is_none() {
            return;
        }
        let p = theme::palette_of(ctx);
        let screen = ctx.content_rect();
        let files: Vec<String> = self
            .view
            .iter()
            .map(|&i| &self.listing.items[i])
            .filter(|i| i.is_file())
            .map(|i| i.key.clone())
            .collect();
        egui::Area::new(egui::Id::unique("viewer"))
            .order(egui::Order::Middle)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.set_min_size(screen.size());
                ui.painter().rect_filled(screen, 0, p.inset);
                // Swallow clicks so nothing underneath reacts.
                ui.interact(
                    screen,
                    ui.scope_id().with("blocker"),
                    Sense::click_and_drag(),
                );

                let header = Rect::from_min_size(screen.min, vec2(screen.width(), 34.0));
                ui.painter().rect_filled(header, 0, p.chrome);
                ui.painter().hline(
                    header.x_range(),
                    header.bottom() - 0.5,
                    Stroke::new(1.0, p.border),
                );
                let body = Rect::from_min_max(pos2(screen.left(), header.bottom()), screen.max);

                let mut header_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(header.shrink2(vec2(6.0, 0.0)))
                        .layout(Layout::left_to_right(Align::Center)),
                );
                self.viewer_header(&mut header_ui, &files);
                let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body));
                self.viewer_body(&mut body_ui, body);
            });
    }

    fn viewer_header(&mut self, ui: &mut Ui, files: &[String]) {
        let p = theme::palette(ui);
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        let item = viewer.item.clone();
        let editing = viewer.editing.is_some();
        let saving = viewer.saving;
        let can_edit = matches!(
            &viewer.content,
            Some(Ok(Content::Text {
                truncated: false,
                ..
            }))
        );
        let index = files.iter().position(|key| *key == item.key);

        if icons::button(ui, Icon::ArrowLeft, "Close (Esc)", true).clicked() {
            self.close_viewer(false);
            return;
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&item.name).font(theme::strong(theme::BODY)));
        let mut meta = Vec::new();
        if let Some(index) = index {
            meta.push(format!("{} of {}", index + 1, files.len()));
        }
        meta.push(format::size(item.size));
        if let Some(modified) = item.modified {
            meta.push(format::date(modified));
        }
        ui.add_space(8.0);
        ui.label(egui::RichText::new(meta.join(", ")).color(p.muted));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let next = index.is_some_and(|i| i + 1 < files.len()) && !editing;
            let previous = index.is_some_and(|i| i > 0) && !editing;
            if icons::button(ui, Icon::ChevronRight, "Next file (Right)", next).clicked() {
                self.step_viewer(1);
            }
            if icons::button(ui, Icon::ChevronLeft, "Previous file (Left)", previous).clicked() {
                self.step_viewer(-1);
            }
            ui.add_space(6.0);
            if editing {
                let save = egui::Button::new(egui::RichText::new("Save").color(p.accent_text))
                    .fill(p.accent)
                    .stroke(Stroke::new(1.0, p.accent));
                if ui
                    .add_enabled(!saving, save)
                    .on_hover_text("Ctrl+S")
                    .clicked()
                {
                    self.save_text();
                }
                if ui
                    .add_enabled(!saving, egui::Button::new("Cancel"))
                    .clicked()
                {
                    if self.viewer.as_ref().is_some_and(Viewer::is_dirty) {
                        self.modal = Some(Modal::discard(ViewerIntent::StopEditing));
                    } else if let Some(viewer) = self.viewer.as_mut() {
                        viewer.editing = None;
                    }
                }
            } else {
                if ui.button("Download").clicked() {
                    self.download(vec![item.clone()]);
                }
                if ui.button("Copy URL").clicked() {
                    self.copy_public_urls(std::slice::from_ref(&item));
                }
                if can_edit && ui.button("Edit").clicked() {
                    if let Some(viewer) = self.viewer.as_mut() {
                        viewer.editing = Some(viewer.original.clone());
                    }
                }
            }
        });
    }

    fn viewer_body(&mut self, ui: &mut Ui, body: Rect) {
        let p = theme::palette(ui);
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        let item = viewer.item.clone();
        if let Some(text) = viewer.editing.as_mut() {
            let frame = egui::Frame::new()
                .fill(p.bg)
                .inner_margin(egui::Margin::symmetric(14, 10));
            frame.show(ui, |ui| {
                ui.set_min_size(body.size() - vec2(28.0, 20.0));
                egui::ScrollArea::both()
                    .id_salt("editor")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(text)
                                .code_editor()
                                .font(theme::mono(12.5))
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .desired_rows(40)
                                .lock_focus(true),
                        );
                    });
            });
            return;
        }
        let kind = format::kind_of(&item.name);
        let center = body.center();
        let notice = |ui: &Ui, icon: Icon, text: &str| {
            icons::paint(
                ui.painter(),
                Rect::from_center_size(center - vec2(0.0, 20.0), vec2(40.0, 40.0)),
                icon,
                p.faint,
            );
            ui.painter().text(
                center + vec2(0.0, 22.0),
                egui::Align2::CENTER_CENTER,
                text,
                theme::body(),
                p.muted,
            );
        };
        if kind.is_media() && !viewer.force_text {
            icons::paint(
                ui.painter(),
                Rect::from_center_size(center - vec2(0.0, 40.0), vec2(56.0, 56.0)),
                icon_for(&item),
                color_for(&item, p),
            );
            let play = Rect::from_center_size(center + vec2(0.0, 22.0), vec2(150.0, 30.0));
            let response = ui.interact(play, ui.scope_id().with("viewer-play"), Sense::click());
            ui.painter().rect(
                play,
                3,
                if response.hovered() {
                    p.chrome
                } else {
                    p.raised
                },
                Stroke::new(1.0, p.border_strong),
                StrokeKind::Inside,
            );
            icons::paint(
                ui.painter(),
                Rect::from_min_size(
                    pos2(play.left() + 14.0, play.center().y - 8.0),
                    vec2(16.0, 16.0),
                ),
                Icon::Play,
                p.text,
            );
            ui.painter().text(
                pos2(play.left() + 40.0, play.center().y),
                egui::Align2::LEFT_CENTER,
                "Play in player",
                theme::body(),
                p.text,
            );
            ui.painter().text(
                center + vec2(0.0, 56.0),
                egui::Align2::CENTER_CENTER,
                "Streams through Fathom; no keys leave the app.",
                theme::small(),
                p.faint,
            );
            if response.clicked() {
                self.play(&item);
            }
            return;
        }
        match &viewer.content {
            None if preview_kind(&item, viewer.force_text).is_some() => {
                ui.place(
                    Rect::from_center_size(center, vec2(22.0, 22.0)),
                    egui::Spinner::new().size(22.0),
                );
            }
            None => {
                notice(ui, icon_for(&item), "No preview for this file type");
                let button = Rect::from_center_size(center + vec2(0.0, 52.0), vec2(110.0, 24.0));
                if ui
                    .place(button, egui::Button::new("Open as text"))
                    .clicked()
                {
                    self.open_viewer(item, true);
                }
            }
            Some(Err(error)) => notice(ui, Icon::Alert, &error.clone()),
            Some(Ok(Content::Binary)) => notice(ui, Icon::File, "This file does not contain text"),
            Some(Ok(Content::Image { texture, .. })) => paint_image(ui, body.shrink(16.0), texture),
            Some(Ok(Content::Text {
                text,
                size,
                truncated,
            })) => {
                let (text, size, truncated) = (text.clone(), *size, *truncated);
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_min_size(body.size() - vec2(28.0, 20.0));
                        if truncated {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Showing the first {} of {}",
                                    format::size(text.len() as u64),
                                    format::size(size)
                                ))
                                .color(p.muted)
                                .font(theme::small()),
                            );
                        }
                        egui::ScrollArea::both()
                            .id_salt("viewer-text")
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(text).font(theme::mono(12.5)),
                                    )
                                    .selectable(true)
                                    .extend(),
                                );
                            });
                    });
            }
        }
    }

    /// Streams a video or audio object to the system player through the local proxy.
    pub(super) fn play(&mut self, item: &Item) {
        let (Ok(client), Some(bucket)) = (self.client(), self.location.bucket.clone()) else {
            return;
        };
        let url = self.proxy.link(client, &bucket, &item.key);
        if let Err(error) = crate::system::play(&url, &format::extension_of(&item.name)) {
            self.toast(error, ToastKind::Error);
        }
    }

    pub(super) fn discard_confirmed(&mut self, intent: ViewerIntent) {
        match intent {
            ViewerIntent::Close => self.viewer = None,
            ViewerIntent::StopEditing => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.editing = None;
                }
            }
            ViewerIntent::Open(item) => {
                self.viewer = None;
                self.open_viewer(item, false);
            }
        }
    }
}
