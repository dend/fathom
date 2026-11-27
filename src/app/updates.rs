//! Updates in the app: a check as it opens and once a day after, an item in
//! the status bar while a newer release is out, and the dialog that installs it.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, pos2, vec2, Rect, RichText, Sense, Stroke, Ui};

use super::dialogs::{dialog, masthead, ButtonKind, Modal};
use super::icons::{self, Icon};
use super::{theme, Event, FathomApp, ToastKind};
use crate::format;
use crate::update::{self, Checks, GitHub, Method, Notice, Release};

/// How often to look at whether a check is due. Checks themselves run once a
/// day, shared with `fth` through the settings folder.
const LOOK_AGAIN: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
pub struct Updates {
    /// Where checks are remembered, when they run on their own.
    checks: Option<Checks>,
    /// A newer release than this copy.
    pub available: Option<Notice>,
    pub checking: bool,
    /// What a check asked for in the About dialog found: nothing newer, or
    /// why it failed.
    pub asked: Option<Result<(), String>>,
    look_at: Option<Instant>,
}

impl Updates {
    pub fn new(settings: &Path) -> Self {
        let checks = update::checks_on_its_own().then(|| Checks::new(settings));
        Self {
            available: checks.as_ref().and_then(Checks::known),
            checks,
            ..Default::default()
        }
    }
}

pub enum Reply {
    Checked {
        result: Result<Release, String>,
        asked: bool,
    },
    Downloading {
        total: u64,
    },
    Progress(u64),
    Installing,
    Failed(String),
    /// The update is in place, or on its way: the app closes to make room.
    Installed,
}

pub struct UpdateView {
    notice: Notice,
    icon: Option<egui::TextureHandle>,
    /// How this copy updates, or why it can't.
    method: Result<Method, String>,
    stage: Stage,
}

enum Stage {
    Ready,
    Downloading {
        done: u64,
        total: u64,
        cancel: Arc<AtomicBool>,
    },
    Installing,
    Failed(String),
}

impl FathomApp {
    /// Checks for a newer release when one is due: as the app opens, and
    /// then once a day while it stays open.
    pub(super) fn check_for_updates_when_due(&mut self, ctx: &egui::Context) {
        let Some(checks) = &self.updates.checks else {
            return;
        };
        let now = Instant::now();
        match self.updates.look_at {
            _ if self.updates.checking => {}
            Some(at) if now < at => ctx.request_repaint_after(at - now),
            _ if checks.due() => self.check_for_updates(false),
            _ => self.updates.look_at = Some(now + LOOK_AGAIN),
        }
    }

    /// Looks for a newer release. `asked` when the About dialog asks, which
    /// then shows what was found.
    pub(super) fn check_for_updates(&mut self, asked: bool) {
        if self.updates.checking {
            return;
        }
        self.updates.checking = true;
        if asked {
            self.updates.asked = None;
        }
        if let Some(checks) = &self.updates.checks {
            checks.postpone();
        }
        let api = std::env::var("FATHOM_UPDATE_API").ok();
        self.spawn(move || {
            let result = GitHub::new(api.as_deref()).latest();
            Event::Update(Reply::Checked { result, asked })
        });
    }

    pub(super) fn show_update(&mut self) {
        let Some(notice) = self.updates.available.clone() else {
            return;
        };
        self.modal = Some(Modal::Update(UpdateView {
            notice,
            icon: self.app_icon(),
            method: Method::detect(),
            stage: Stage::Ready,
        }));
    }

    pub(super) fn update_reply(&mut self, reply: Reply) {
        if let Reply::Checked { result, asked } = reply {
            self.updates.checking = false;
            self.updates.look_at = Some(Instant::now() + LOOK_AGAIN);
            match result {
                Ok(release) => {
                    if let Some(checks) = &self.updates.checks {
                        checks.record(&release);
                    }
                    self.updates.available = release.is_newer().then(|| Notice::from(&release));
                    if asked {
                        self.updates.asked = Some(Ok(()));
                    }
                }
                Err(error) if asked => self.updates.asked = Some(Err(error)),
                Err(_) => {}
            }
            return;
        }
        if let Reply::Installed = reply {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // The rest only matter while the dialog is following them.
        let Some(Modal::Update(view)) = self.modal.as_mut() else {
            return;
        };
        match (reply, &mut view.stage) {
            (Reply::Downloading { total }, Stage::Downloading { total: shown, .. }) => {
                *shown = total;
            }
            (Reply::Progress(done), Stage::Downloading { done: shown, .. }) => *shown = done,
            (Reply::Installing, Stage::Downloading { .. }) => view.stage = Stage::Installing,
            (Reply::Failed(error), Stage::Downloading { .. } | Stage::Installing) => {
                view.stage = Stage::Failed(error);
            }
            _ => {}
        }
    }

    fn start_update(&mut self, view: &mut UpdateView) {
        let Ok(method) = view.method.clone() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        view.stage = Stage::Downloading {
            done: 0,
            total: 0,
            cancel: cancel.clone(),
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let api = std::env::var("FATHOM_UPDATE_API").ok();
        std::thread::spawn(move || {
            let send = |reply: Reply| {
                let _ = tx.send(Event::Update(reply));
                ctx.request_repaint();
            };
            // The newest release, which may have come out since the last check.
            let result = (|| {
                let github = GitHub::new(api.as_deref());
                let release = github.latest()?;
                let name = method.asset(&release.version)?;
                let total = release.size_of(&name).unwrap_or(0);
                send(Reply::Downloading { total });
                let progress = |done: u64| send(Reply::Progress(done));
                let dir = update::download_dir();
                let file = github.download(&release, &name, &dir, &progress, &cancel)?;
                send(Reply::Installing);
                update::install(&method, &file, true)
            })();
            send(match result {
                Ok(()) => Reply::Installed,
                Err(error) => Reply::Failed(error),
            });
        });
    }

    /// The status bar item that says a newer release is out.
    pub(super) fn update_item(&mut self, ui: &mut Ui) {
        let Some(notice) = self.updates.available.clone() else {
            return;
        };
        let p = theme::palette(ui);
        let label = format!("Update to {}", notice.version);
        let galley = ui.painter().layout_no_wrap(label, theme::small(), p.accent);
        let size = vec2(galley.size().x + 34.0, ui.available_height());
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        if response.hovered() {
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
            Icon::Update,
            p.accent,
        );
        ui.painter().galley(
            pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
            galley,
            p.accent,
        );
        let tip = format!(
            "Fathom {} is available. You have {}.",
            notice.version,
            update::current()
        );
        if response.on_hover_text(tip).clicked() {
            self.show_update();
        }
    }

    pub(super) fn update_dialog(&mut self, ctx: &egui::Context, view: &mut UpdateView) -> bool {
        let p = theme::palette_of(ctx);
        // An update closes the app, so unsaved edits would be lost.
        let editing = self
            .viewer
            .as_ref()
            .filter(|viewer| viewer.is_dirty())
            .map(|viewer| viewer.item.name.clone());
        let ready = view.method.is_ok() && editing.is_none();
        let buttons: &[(&str, ButtonKind)] = match &view.stage {
            Stage::Ready if ready => &[
                ("Not now", ButtonKind::Cancel),
                ("Update", ButtonKind::Primary),
            ],
            Stage::Ready if view.method.is_err() => &[
                ("Close", ButtonKind::Cancel),
                ("Download", ButtonKind::Primary),
            ],
            Stage::Ready => &[("Close", ButtonKind::Primary)],
            Stage::Downloading { .. } | Stage::Installing => &[("Cancel", ButtonKind::Cancel)],
            Stage::Failed(_) => &[
                ("Close", ButtonKind::Cancel),
                ("Try again", ButtonKind::Primary),
            ],
        };
        let installing = matches!(view.stage, Stage::Installing);
        let transfers = self.transfers.is_busy();
        let mut open_page = false;
        let footer = dialog(ctx, "Update Fathom", 420.0, installing, buttons, |ui| {
            let line = format!("You have {}", update::current());
            masthead(
                ui,
                view.icon.as_ref(),
                &format!("Fathom {}", view.notice.version),
                &line,
            );
            ui.add_space(14.0);
            let wrapped = |ui: &mut Ui, text: RichText| ui.add(egui::Label::new(text).wrap());
            // At least two lines tall, so the dialog keeps its size from one
            // stage to the next.
            let lines = 2.0 * ui.text_style_height(&egui::TextStyle::Body) + 8.0;
            ui.vertical(|ui| {
                ui.set_min_height(lines);
                match &view.stage {
                    Stage::Ready => {
                        match (&view.method, &editing) {
                            (Err(reason), _) => wrapped(ui, RichText::new(reason)),
                            (Ok(_), Some(name)) => wrapped(
                                ui,
                                RichText::new(format!(
                                    "Save or discard your changes to {name}, then update."
                                )),
                            ),
                            (Ok(method), None) => wrapped(ui, RichText::new(method.plan())),
                        };
                        if ready && transfers {
                            wrapped(
                                ui,
                                RichText::new("Transfers that are still running will stop.")
                                    .color(p.muted),
                            );
                        }
                        ui.add_space(2.0);
                        let link = format!("What's new in {}", view.notice.version);
                        open_page = ui.link(link).on_hover_text(&view.notice.page).clicked();
                    }
                    Stage::Downloading { done, total, .. } => {
                        let text = if *total > 0 {
                            format!(
                                "Downloading {} of {}",
                                format::size(*done),
                                format::size(*total)
                            )
                        } else {
                            "Downloading...".to_string()
                        };
                        ui.label(text);
                        ui.add_space(6.0);
                        let fraction = if *total > 0 {
                            *done as f32 / *total as f32
                        } else {
                            0.0
                        };
                        progress_bar(ui, fraction.min(1.0));
                    }
                    Stage::Installing => {
                        ui.label("Installing...");
                        ui.add_space(6.0);
                        progress_bar(ui, 1.0);
                    }
                    Stage::Failed(error) => {
                        wrapped(ui, RichText::new(error).color(p.danger));
                        ui.add_space(2.0);
                        open_page = ui
                            .link("Download it from GitHub instead")
                            .on_hover_text(&view.notice.page)
                            .clicked();
                    }
                }
            });
        });

        let page = |app: &mut Self| {
            if let Err(error) = crate::system::open(&view.notice.page) {
                app.toast(error, ToastKind::Error);
            }
        };
        if open_page {
            page(self);
        }
        if footer.cancel {
            if let Stage::Downloading { cancel, .. } = &view.stage {
                cancel.store(true, Ordering::Relaxed);
            }
            return false;
        }
        if footer.submit {
            match &view.stage {
                Stage::Ready | Stage::Failed(_) if ready => self.start_update(view),
                Stage::Ready if view.method.is_err() => {
                    page(self);
                    return false;
                }
                Stage::Ready => return false,
                _ => {}
            }
        }
        true
    }
}

/// A thin bar showing how far along something is.
fn progress_bar(ui: &mut Ui, fraction: f32) {
    let p = theme::palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    ui.painter().rect_filled(rect, 2, p.inset);
    let done = Rect::from_min_size(rect.min, vec2(rect.width() * fraction, rect.height()));
    ui.painter().rect_filled(done, 2, p.accent);
}
