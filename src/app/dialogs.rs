//! Modal dialogs.

use std::collections::HashSet;

use eframe::egui::{self, vec2, Align, Key, Layout, Rect, RichText, Stroke, Ui};

use super::icons::{self, Icon};
use super::{theme, Event, FathomApp, Item, ItemKind, Location, ToastKind};
use crate::config::{self, Connection, Credentials, Store};
use crate::format;
use crate::ops::Move;
use crate::s3::{ObjectInfo, S3Client};

pub enum Modal {
    Connection(ConnectionForm),
    Prompt(PromptForm),
    Confirm(ConfirmForm),
    Domain(DomainForm),
    Properties(PropertiesView),
}

/// Results of work started from a dialog.
pub enum Reply {
    Tested(Result<usize, String>),
    Saved(Result<Connection, String>),
    PromptFinished {
        result: Result<String, String>,
    },
    Properties {
        key: String,
        result: Result<ObjectInfo, String>,
    },
    TextSaved {
        key: String,
        text: String,
        result: Result<(), String>,
    },
}

#[derive(Default)]
pub struct ConnectionForm {
    id: Option<String>,
    name: String,
    account_id: String,
    api_token: String,
    access_key_id: String,
    secret: String,
    /// Whether the connection being edited was saved with an API token.
    uses_token: bool,
    jurisdiction: String,
    endpoint: String,
    endpoint_touched: bool,
    has_endpoint: bool,
    buckets: String,
    advanced: bool,
    status: Option<String>,
    busy: bool,
    error: Option<String>,
    focused: bool,
}

pub enum PromptPurpose {
    NewFolder,
    NewFile,
    Rename(Item),
}

pub struct PromptForm {
    title: String,
    label: String,
    value: String,
    hint: Option<String>,
    confirm: &'static str,
    purpose: PromptPurpose,
    taken: HashSet<String>,
    select_stem: bool,
    focused: bool,
    busy: bool,
    error: Option<String>,
}

pub enum ViewerIntent {
    Close,
    StopEditing,
    Open(Item),
}

pub enum ConfirmAction {
    Delete(Vec<Item>),
    Paste {
        src_bucket: String,
        moves: Vec<Move>,
        delete_source: bool,
        label: String,
        target: String,
    },
    RemoveConnection(String),
    DiscardEdits(ViewerIntent),
}

pub struct ConfirmForm {
    title: String,
    message: String,
    detail: String,
    confirm: String,
    danger: bool,
    action: Option<ConfirmAction>,
}

pub struct DomainForm {
    bucket: String,
    value: String,
    /// Items whose public URLs are copied once a domain is saved.
    then_copy: Vec<Item>,
    error: Option<String>,
    focused: bool,
}

pub struct PropertiesView {
    item: Item,
    bucket: String,
    info: Option<Result<ObjectInfo, String>>,
}

impl Modal {
    pub fn connection(existing: Option<&Connection>) -> Self {
        let mut form = ConnectionForm::default();
        if let Some(c) = existing {
            form.id = Some(c.id.clone());
            form.name = c.name.clone();
            form.jurisdiction = c.jurisdiction.clone();
            form.buckets = c.buckets.join(", ");
            form.has_endpoint = c.custom_endpoint;
            form.uses_token = c.uses_token;
            // Connections saved with access keys show them under Advanced.
            form.advanced = !c.jurisdiction.is_empty()
                || !c.buckets.is_empty()
                || c.custom_endpoint
                || !c.uses_token;
        }
        Modal::Connection(form)
    }

    pub fn remove_connection(connection: &Connection) -> Self {
        Modal::Confirm(ConfirmForm {
            title: format!("Remove \"{}\"?", connection.name),
            message: "This removes the connection and its saved credentials from this computer."
                .into(),
            detail: "Nothing in your buckets is changed.".into(),
            confirm: "Remove".into(),
            danger: true,
            action: Some(ConfirmAction::RemoveConnection(connection.id.clone())),
        })
    }

    pub fn discard(intent: ViewerIntent) -> Self {
        Modal::Confirm(ConfirmForm {
            title: "Discard changes?".into(),
            message: "Your edits have not been saved.".into(),
            detail: String::new(),
            confirm: "Discard".into(),
            danger: true,
            action: Some(ConfirmAction::DiscardEdits(intent)),
        })
    }

    pub fn confirm(
        title: String,
        message: String,
        detail: String,
        confirm: &str,
        danger: bool,
        action: ConfirmAction,
    ) -> Self {
        Modal::Confirm(ConfirmForm {
            title,
            message,
            detail,
            confirm: confirm.into(),
            danger,
            action: Some(action),
        })
    }

    pub fn prompt(
        title: &str,
        label: &str,
        value: &str,
        confirm: &'static str,
        purpose: PromptPurpose,
        taken: HashSet<String>,
        hint: Option<String>,
    ) -> Self {
        Modal::Prompt(PromptForm {
            title: title.into(),
            label: label.into(),
            value: value.into(),
            hint,
            confirm,
            select_stem: !matches!(purpose, PromptPurpose::NewFolder)
                && !matches!(&purpose, PromptPurpose::Rename(item) if item.kind == ItemKind::Folder),
            purpose,
            taken,
            focused: false,
            busy: false,
            error: None,
        })
    }

    pub fn domain(bucket: &str, current: Option<String>, then_copy: Vec<Item>) -> Self {
        Modal::Domain(DomainForm {
            bucket: bucket.into(),
            value: current.unwrap_or_default(),
            then_copy,
            error: None,
            focused: false,
        })
    }

    pub fn properties(item: Item, bucket: String) -> Self {
        Modal::Properties(PropertiesView {
            item,
            bucket,
            info: None,
        })
    }
}

/// Merges what the connection dialog submitted with the stored values. Account
/// details are never sent back to the UI, so an empty field keeps what is saved.
/// The flag says whether the S3 keys still have to be derived from the API token,
/// which takes a call to Cloudflare.
fn resolve(
    store: &Store,
    form: &ConnectionForm,
) -> Result<(Connection, Credentials, bool), String> {
    let (mut connection, mut credentials) = match &form.id {
        Some(id) => (
            store.connection(id)?,
            store.credentials(id).unwrap_or_default(),
        ),
        None => (Connection::default(), Credentials::default()),
    };
    if !form.account_id.trim().is_empty() {
        credentials.account_id = form.account_id.clone();
    }
    let token = form.api_token.trim();
    let keys = !form.access_key_id.trim().is_empty() || !form.secret.trim().is_empty();
    if !token.is_empty() && keys {
        return Err("Enter an API token or access keys, not both".into());
    }
    if !token.is_empty() {
        credentials.api_token = token.to_string();
        credentials.access_key_id.clear();
        credentials.secret_access_key.clear();
    } else if keys {
        credentials.api_token.clear();
        if !form.access_key_id.trim().is_empty() {
            credentials.access_key_id = form.access_key_id.clone();
        }
        if !form.secret.trim().is_empty() {
            credentials.secret_access_key = form.secret.clone();
        }
    }
    if form.id.is_none() || form.endpoint_touched {
        credentials.endpoint = form.endpoint.clone();
    }
    credentials.normalize();
    if credentials.account_id.is_empty() && credentials.endpoint.is_empty() {
        return Err("Enter your Cloudflare account ID".into());
    }
    if credentials.api_token.is_empty() {
        if credentials.access_key_id.is_empty() && credentials.secret_access_key.is_empty() {
            return Err("Enter your API token".into());
        }
        if credentials.access_key_id.is_empty() {
            return Err("Enter the access key ID".into());
        }
        if credentials.secret_access_key.is_empty() {
            return Err("Enter the secret access key".into());
        }
    } else if credentials.account_id.is_empty() {
        return Err("Enter your Cloudflare account ID".into());
    }
    let derive = !credentials.api_token.is_empty()
        && (credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty());
    connection.uses_token = !credentials.api_token.is_empty();
    connection.name = form.name.clone();
    connection.jurisdiction = form.jurisdiction.clone();
    connection.buckets = form
        .buckets
        .split(',')
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect();
    connection.custom_endpoint = !credentials.endpoint.is_empty();
    connection.normalize();
    Ok((connection, credentials, derive))
}

fn client_for(connection: &Connection, credentials: &Credentials) -> Result<S3Client, String> {
    S3Client::new(
        &credentials.endpoint_url(&connection.jurisdiction),
        &credentials.access_key_id,
        &credentials.secret_access_key,
    )
    .map_err(|e| e.to_string())
}

fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn name_problem(name: &str, taken: &HashSet<String>) -> Option<&'static str> {
    if name.is_empty() {
        Some("Enter a name")
    } else if name.contains('/') {
        Some("Names cannot contain \"/\"")
    } else if name == "." || name == ".." {
        Some("That name is not allowed")
    } else if taken.contains(name) {
        Some("Something with that name already exists here")
    } else {
        None
    }
}

/// What the footer of a dialog asked for this frame.
#[derive(Default)]
struct Footer {
    submit: bool,
    secondary: bool,
    cancel: bool,
}

/// Draws the common dialog chrome around `body`.
fn dialog(
    ctx: &egui::Context,
    title: &str,
    width: f32,
    busy: bool,
    buttons: &[(&str, ButtonKind)],
    body: impl FnOnce(&mut Ui),
) -> Footer {
    let p = theme::palette_of(ctx);
    let mut footer = Footer::default();
    let response = egui::Modal::new(egui::Id::unique("dialog"))
        .backdrop_color(egui::Color32::from_black_alpha(if p.bg == theme::DARK.bg {
            120
        } else {
            70
        }))
        .frame(
            egui::Frame::new()
                .fill(p.raised)
                .stroke(Stroke::new(1.0, p.border_strong))
                .corner_radius(4)
                .shadow(ctx.global_style().visuals.window_shadow),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            // Header.
            let (header, _) = ui.allocate_exact_size(vec2(width, 31.0), egui::Sense::hover());
            ui.painter().rect_filled(
                header,
                egui::CornerRadius {
                    nw: 4,
                    ne: 4,
                    sw: 0,
                    se: 0,
                },
                p.chrome,
            );
            ui.painter().hline(
                header.x_range(),
                header.bottom() - 0.5,
                Stroke::new(1.0, p.border),
            );
            ui.painter().text(
                header.left_center() + vec2(12.0, 0.0),
                egui::Align2::LEFT_CENTER,
                title,
                theme::strong(theme::BODY),
                p.text,
            );
            let close =
                Rect::from_center_size(header.right_center() - vec2(17.0, 0.0), vec2(26.0, 26.0));
            let mut close_ui = ui.new_child(egui::UiBuilder::new().max_rect(close));
            if icons::button(&mut close_ui, Icon::Close, "Close", !busy).clicked() {
                footer.cancel = true;
            }
            // Body.
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: 14,
                    right: 14,
                    top: 12,
                    bottom: 14,
                })
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.set_width(width - 28.0);
                    ui.add_enabled_ui(!busy, body);
                });
            // Footer.
            let (bar, _) = ui.allocate_exact_size(vec2(width, 42.0), egui::Sense::hover());
            ui.painter().rect_filled(
                bar,
                egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 4,
                    se: 4,
                },
                p.chrome,
            );
            ui.painter()
                .hline(bar.x_range(), bar.top() + 0.5, Stroke::new(1.0, p.border));
            let mut bar_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(bar.shrink2(vec2(14.0, 0.0)))
                    .layout(Layout::right_to_left(Align::Center)),
            );
            bar_ui.spacing_mut().item_spacing.x = 6.0;
            for (label, kind) in buttons.iter().rev() {
                let button = match kind {
                    ButtonKind::Primary => {
                        egui::Button::new(RichText::new(*label).color(p.accent_text))
                            .fill(p.accent)
                            .stroke(Stroke::new(1.0, p.accent))
                    }
                    ButtonKind::Danger => egui::Button::new(RichText::new(*label).color(p.bg))
                        .fill(p.danger)
                        .stroke(Stroke::new(1.0, p.danger)),
                    _ => egui::Button::new(*label),
                }
                .min_size(vec2(76.0, 26.0));
                if matches!(kind, ButtonKind::Secondary) {
                    // The secondary action sits on the left edge of the footer.
                    let mut left = bar_ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(bar.shrink2(vec2(14.0, 0.0)))
                            .layout(Layout::left_to_right(Align::Center)),
                    );
                    if left.add_enabled(!busy, button).clicked() {
                        footer.secondary = true;
                    }
                    continue;
                }
                if bar_ui.add_enabled(!busy, button).clicked() {
                    match kind {
                        ButtonKind::Cancel => footer.cancel = true,
                        _ => footer.submit = true,
                    }
                }
            }
            if busy {
                bar_ui.add(egui::Spinner::new().size(14.0));
            }
        });
    if response.should_close() && !busy {
        footer.cancel = true;
    }
    let enter = ctx.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
    if enter && !busy && !footer.cancel {
        footer.submit = true;
    }
    footer
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    Primary,
    Danger,
    Cancel,
    Secondary,
}

fn field(
    ui: &mut Ui,
    label: &str,
    hint: Option<&str>,
    add: impl FnOnce(&mut Ui) -> egui::Response,
) -> egui::Response {
    let p = theme::palette(ui);
    ui.add_space(4.0);
    // Screen readers announce the control by this label (masked fields have no
    // text of their own to announce).
    let label = ui.label(label);
    let response = add(ui).labelled_by(label.id);
    if let Some(hint) = hint {
        ui.label(RichText::new(hint).color(p.muted).font(theme::small()));
    }
    response
}

fn text_input(ui: &mut Ui, value: &mut String, hint: &str, password: bool) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .password(password)
            .desired_width(f32::INFINITY)
            .margin(egui::Margin::symmetric(6, 4)),
    )
}

impl FathomApp {
    pub(super) fn modals(&mut self, ctx: &egui::Context) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        let keep = match &mut modal {
            Modal::Connection(form) => self.connection_dialog(ctx, form),
            Modal::Prompt(form) => self.prompt_dialog(ctx, form),
            Modal::Confirm(form) => self.confirm_dialog(ctx, form),
            Modal::Domain(form) => self.domain_dialog(ctx, form),
            Modal::Properties(view) => self.properties_dialog(ctx, view),
        };
        // A dialog may have opened another one (for example remove -> add connection).
        if keep && self.modal.is_none() {
            self.modal = Some(modal);
        }
    }

    fn connection_dialog(&mut self, ctx: &egui::Context, form: &mut ConnectionForm) -> bool {
        let p = theme::palette_of(ctx);
        let editing = form.id.is_some();
        let title = if editing {
            "Edit connection"
        } else {
            "Connect to Cloudflare R2"
        };
        let keep_hint = editing.then_some("Leave empty to keep the saved value.");
        let footer = dialog(
            ctx,
            title,
            460.0,
            form.busy,
            &[
                ("Test connection", ButtonKind::Secondary),
                ("Cancel", ButtonKind::Cancel),
                (
                    if editing { "Save" } else { "Connect" },
                    ButtonKind::Primary,
                ),
            ],
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let bold = |text: &str| RichText::new(text).font(theme::strong(theme::BODY)).color(p.muted);
                    ui.label(RichText::new("Create an API token under ").color(p.muted));
                    ui.label(bold("R2 > Manage API tokens"));
                    ui.label(RichText::new(" in the Cloudflare dashboard, then paste your account ID and the token's ").color(p.muted));
                    ui.label(bold("Token value"));
                    ui.label(RichText::new(" here. Both are kept in your system's credential store.").color(p.muted));
                });
                let name = field(ui, "Name", None, |ui| {
                    text_input(ui, &mut form.name, "Personal R2", false)
                });
                if !form.focused {
                    name.request_focus();
                    form.focused = true;
                }
                let unchanged = if editing { "Unchanged" } else { "" };
                field(ui, "Account ID", keep_hint, |ui| {
                    text_input(
                        ui,
                        &mut form.account_id,
                        if editing {
                            unchanged
                        } else {
                            "32-character account ID"
                        },
                        false,
                    )
                });
                let token_placeholder = match (editing, form.uses_token) {
                    (true, true) => "Unchanged",
                    (true, false) => "Not set",
                    _ => "",
                };
                field(
                    ui,
                    "API token",
                    keep_hint.filter(|_| form.uses_token),
                    |ui| text_input(ui, &mut form.api_token, token_placeholder, true),
                );
                ui.add_space(8.0);
                ui.separator();
                let advanced = egui::CollapsingHeader::new(RichText::new("Advanced").color(p.muted))
                    .default_open(form.advanced)
                    .show(ui, |ui| {
                        field(ui, "Jurisdiction", None, |ui| {
                            egui::ComboBox::from_id_salt("jurisdiction")
                                .width(ui.available_width())
                                .selected_text(match form.jurisdiction.as_str() {
                                    "eu" => "European Union (EU)",
                                    "fedramp" => "FedRAMP",
                                    _ => "Default",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut form.jurisdiction, String::new(), "Default");
                                    ui.selectable_value(&mut form.jurisdiction, "eu".into(), "European Union (EU)");
                                    ui.selectable_value(&mut form.jurisdiction, "fedramp".into(), "FedRAMP");
                                })
                                .response
                        });
                        field(
                            ui,
                            "Buckets",
                            Some("Comma-separated. Only needed when the token is limited to specific buckets and cannot list them."),
                            |ui| text_input(ui, &mut form.buckets, "All buckets", false),
                        );
                        let keys_placeholder = if editing && !form.uses_token { "Unchanged" } else { "" };
                        field(ui, "Access key ID", Some("Instead of an API token. Public URLs then need a domain set by hand."), |ui| {
                            text_input(ui, &mut form.access_key_id, keys_placeholder, true)
                        });
                        field(ui, "Secret access key", None, |ui| {
                            text_input(ui, &mut form.secret, keys_placeholder, true)
                        });
                        let placeholder = if form.has_endpoint { "Unchanged" } else { "Derived from the account ID" };
                        let endpoint = field(ui, "Custom endpoint", Some("For S3-compatible endpoints other than the default R2 one."), |ui| {
                            text_input(ui, &mut form.endpoint, placeholder, false)
                        });
                        if endpoint.changed() {
                            form.endpoint_touched = true;
                        }
                    });
                form.advanced = advanced.openness > 0.5;
                if let Some(status) = &form.status {
                    ui.add_space(6.0);
                    ui.label(RichText::new(status).color(p.success));
                }
                if let Some(error) = &form.error {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(error).color(p.danger)).wrap());
                }
            },
        );
        if footer.cancel {
            return false;
        }
        if footer.secondary || footer.submit {
            let saving = footer.submit;
            form.busy = true;
            form.error = None;
            form.status = saving
                .then(|| "Saving...".to_string())
                .or(Some("Testing...".into()));
            let resolved = resolve(&self.store, form);
            let store = self.store.clone();
            let existing_id = form.id.clone();
            self.spawn(move || {
                let reply = match resolved {
                    Err(error) if saving => Reply::Saved(Err(error)),
                    Err(error) => Reply::Tested(Err(error)),
                    Ok((mut connection, mut credentials, derive)) => {
                        let keys = if derive {
                            crate::cloudflare::Api::new(
                                &credentials.account_id,
                                &credentials.api_token,
                            )
                            .s3_keys()
                            .map(|(id, secret)| {
                                credentials.access_key_id = id;
                                credentials.secret_access_key = secret;
                            })
                        } else {
                            Ok(())
                        };
                        let client = keys.and_then(|()| client_for(&connection, &credentials));
                        if saving {
                            Reply::Saved((|| {
                                client?;
                                connection.id = existing_id.unwrap_or_else(new_id);
                                store.save_credentials(&connection.id, &credentials)?;
                                let saved = connection.clone();
                                store.update(move |config| {
                                    match config
                                        .connections
                                        .iter_mut()
                                        .find(|c| c.id == connection.id)
                                    {
                                        Some(slot) => *slot = connection.clone(),
                                        None => config.connections.push(connection.clone()),
                                    }
                                    config.active = Some(connection.id);
                                })?;
                                Ok(saved)
                            })())
                        } else {
                            Reply::Tested((|| {
                                let client = client?;
                                // Tokens scoped to specific buckets cannot list buckets,
                                // so check the ones the user named instead.
                                if connection.buckets.is_empty() {
                                    return client
                                        .list_buckets()
                                        .map(|b| b.len())
                                        .map_err(|e| e.to_string());
                                }
                                for bucket in &connection.buckets {
                                    client
                                        .list(bucket, "", Some("/"), None)
                                        .map_err(|e| format!("{bucket}: {e}"))?;
                                }
                                Ok(connection.buckets.len())
                            })())
                        }
                    }
                };
                Event::Dialog(reply)
            });
        }
        true
    }

    fn prompt_dialog(&mut self, ctx: &egui::Context, form: &mut PromptForm) -> bool {
        let p = theme::palette_of(ctx);
        let input_id = egui::Id::unique("prompt-input");
        let footer = dialog(
            ctx,
            &form.title.clone(),
            380.0,
            form.busy,
            &[
                ("Cancel", ButtonKind::Cancel),
                (form.confirm, ButtonKind::Primary),
            ],
            |ui| {
                ui.label(&form.label);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut form.value)
                        .id(input_id)
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(6, 4)),
                );
                if !form.focused {
                    response.request_focus();
                    // Select the name without its extension, like a file manager does.
                    let end = match form.value.rfind('.') {
                        Some(dot) if dot > 0 && form.select_stem => dot,
                        _ => form.value.chars().count(),
                    };
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), input_id) {
                        let range = egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(end),
                        );
                        state.cursor.set_char_range(Some(range));
                        state.store(ui.ctx(), input_id);
                        form.focused = true;
                    }
                }
                if let Some(hint) = &form.hint {
                    ui.add(
                        egui::Label::new(RichText::new(hint).color(p.muted).font(theme::small()))
                            .wrap(),
                    );
                }
                if let Some(error) = &form.error {
                    ui.label(RichText::new(error).color(p.danger));
                }
            },
        );
        if footer.cancel {
            return false;
        }
        if !footer.submit {
            return true;
        }
        let value = form.value.trim().to_string();
        if let PromptPurpose::Rename(item) = &form.purpose {
            if value == item.name {
                return false;
            }
        }
        if let Some(problem) = name_problem(&value, &form.taken) {
            form.error = Some(problem.into());
            return true;
        }
        let (Some(bucket), Ok(client)) = (self.location.bucket.clone(), self.client()) else {
            return false;
        };
        let prefix = self.location.prefix.clone();
        match &form.purpose {
            PromptPurpose::NewFolder | PromptPurpose::NewFile => {
                let folder = matches!(form.purpose, PromptPurpose::NewFolder);
                form.busy = true;
                form.error = None;
                self.spawn(move || {
                    let key = if folder {
                        format!("{prefix}{value}/")
                    } else {
                        format!("{prefix}{value}")
                    };
                    let result = if folder {
                        client.put_bytes(&bucket, &key, &[], "application/x-directory")
                    } else {
                        let content_type = match crate::s3::content_type_for(&key) {
                            "application/octet-stream" => "text/plain; charset=utf-8",
                            guessed => guessed,
                        };
                        client.put_bytes(&bucket, &key, &[], content_type)
                    };
                    Event::Dialog(Reply::PromptFinished {
                        result: result.map(|_| key).map_err(|e| e.to_string()),
                    })
                });
                true
            }
            PromptPurpose::Rename(item) => {
                let folder = item.kind == ItemKind::Folder;
                let to = format!("{prefix}{value}{}", if folder { "/" } else { "" });
                let moves = vec![Move {
                    from: item.key.clone(),
                    to: to.clone(),
                    folder,
                    size: item.size,
                }];
                let label = format!("Renaming \"{}\"...", item.name);
                self.run_transfer(label, bucket.clone(), bucket, moves, true, Some(to), prefix);
                false
            }
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, form: &mut ConfirmForm) -> bool {
        let p = theme::palette_of(ctx);
        let kind = if form.danger {
            ButtonKind::Danger
        } else {
            ButtonKind::Primary
        };
        let footer = dialog(
            ctx,
            &form.title.clone(),
            400.0,
            false,
            &[
                ("Cancel", ButtonKind::Cancel),
                (&form.confirm.clone(), kind),
            ],
            |ui| {
                ui.add(egui::Label::new(&form.message).wrap());
                if !form.detail.is_empty() {
                    ui.add(egui::Label::new(RichText::new(&form.detail).color(p.muted)).wrap());
                }
            },
        );
        if footer.cancel {
            return false;
        }
        if footer.submit {
            if let Some(action) = form.action.take() {
                self.confirmed(action);
            }
            return false;
        }
        true
    }

    fn confirmed(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Delete(items) => self.run_delete(items),
            ConfirmAction::Paste {
                src_bucket,
                moves,
                delete_source,
                label,
                target,
            } => {
                let Some(bucket) = self.location.bucket.clone() else {
                    return;
                };
                self.run_transfer(
                    label,
                    src_bucket,
                    bucket,
                    moves,
                    delete_source,
                    None,
                    target,
                );
                if delete_source {
                    self.clipboard = None;
                }
            }
            ConfirmAction::RemoveConnection(id) => {
                let name = self
                    .connections
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                let removed = self.store.update(|config| {
                    config.connections.retain(|c| c.id != id);
                    if config.active.as_deref() == Some(id.as_str()) {
                        config.active = config.connections.first().map(|c| c.id.clone());
                    }
                });
                if let Err(error) = removed {
                    self.toast(error, ToastKind::Error);
                    return;
                }
                self.store.delete_credentials(&id);
                self.load_connections();
                self.toast(format!("Removed \"{name}\""), ToastKind::Info);
                match self.active.clone() {
                    Some(next) => self.switch_connection(&next),
                    None => {
                        self.buckets.clear();
                        self.tree.clear();
                        self.navigate(Location::default(), None);
                        self.modal = Some(Modal::connection(None));
                    }
                }
            }
            ConfirmAction::DiscardEdits(intent) => self.discard_confirmed(intent),
        }
    }

    fn domain_dialog(&mut self, ctx: &egui::Context, form: &mut DomainForm) -> bool {
        let p = theme::palette_of(ctx);
        let footer = dialog(
            ctx,
            &format!("Public domain for \"{}\"", form.bucket),
            420.0,
            false,
            &[
                ("Cancel", ButtonKind::Cancel),
                ("Save", ButtonKind::Primary),
            ],
            |ui| {
                ui.add(
                egui::Label::new(
                    RichText::new("Enter the custom domain (or r2.dev address) connected to this bucket. Fathom uses it to build shareable URLs for your files.")
                        .color(p.muted),
                )
                .wrap(),
            );
                ui.add_space(4.0);
                ui.label("Domain");
                let response = text_input(ui, &mut form.value, "cdn.example.com", false);
                if !form.focused {
                    response.request_focus();
                    form.focused = true;
                }
                let value = form.value.trim().trim_end_matches('/');
                let base = if value.is_empty() {
                    "https://cdn.example.com".to_string()
                } else if value.contains("://") {
                    value.to_string()
                } else {
                    format!("https://{value}")
                };
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label(
                        RichText::new("URLs will look like ")
                            .color(p.muted)
                            .font(theme::small()),
                    );
                    ui.label(
                        RichText::new(format!("{base}/path/to/file.png"))
                            .font(theme::mono(11.5))
                            .color(p.muted),
                    );
                });
                if let Some(error) = &form.error {
                    ui.label(RichText::new(error).color(p.danger));
                }
            },
        );
        if footer.cancel {
            return false;
        }
        if !footer.submit {
            return true;
        }
        let domain = match config::normalize_domain(&form.value) {
            Ok(domain) => domain,
            Err(error) => {
                form.error = Some(error);
                return true;
            }
        };
        let Some(conn) = self.active.clone() else {
            return false;
        };
        let (bucket, saved) = (form.bucket.clone(), domain.clone());
        let result = self.store.update(move |config| {
            if let Some(connection) = config.connections.iter_mut().find(|c| c.id == conn) {
                if saved.is_empty() {
                    connection.domains.remove(&bucket);
                } else {
                    connection.domains.insert(bucket, saved);
                }
            }
        });
        if let Err(error) = result {
            form.error = Some(error);
            return true;
        }
        self.load_connections();
        if domain.is_empty() {
            self.toast("Public domain removed", ToastKind::Info);
        } else if !form.then_copy.is_empty() {
            let items = std::mem::take(&mut form.then_copy);
            self.copy_public_urls(&items);
        }
        false
    }

    fn properties_dialog(&mut self, ctx: &egui::Context, view: &mut PropertiesView) -> bool {
        let p = theme::palette_of(ctx);
        let item = view.item.clone();
        let mut rows: Vec<(String, String, bool)> = vec![
            ("Name".into(), item.name.clone(), false),
            ("Bucket".into(), view.bucket.clone(), false),
            ("Key".into(), item.key.clone(), true),
        ];
        let mut note = None;
        if item.kind == ItemKind::Folder {
            rows.push(("Type".into(), "Folder".into(), false));
        } else {
            match &view.info {
                None => note = Some("Loading details...".to_string()),
                Some(Err(error)) => note = Some(error.clone()),
                Some(Ok(info)) => {
                    rows.push((
                        "Size".into(),
                        format!(
                            "{} ({} bytes)",
                            format::size(info.size),
                            format::thousands(info.size)
                        ),
                        false,
                    ));
                    rows.push(("Content type".into(), info.content_type.clone(), false));
                    if let Some(modified) = format::parse_http_date(&info.modified) {
                        rows.push(("Modified".into(), format::date(modified), false));
                    }
                    rows.push(("ETag".into(), info.etag.clone(), true));
                    if !info.cache_control.is_empty() {
                        rows.push(("Cache control".into(), info.cache_control.clone(), false));
                    }
                    for (name, value) in &info.metadata {
                        rows.push((format!("Metadata: {name}"), value.clone(), false));
                    }
                    if let Some(base) = self.domain_for(&view.bucket) {
                        rows.push((
                            "Public URL".into(),
                            format::public_url(&base, &item.key),
                            true,
                        ));
                    }
                }
            }
        }
        let mut copied = None;
        let footer = dialog(
            ctx,
            "Properties",
            480.0,
            false,
            &[("Close", ButtonKind::Primary)],
            |ui| {
                // Fixed label and button columns, so the values always get the rest of
                // the width. A grid sizes its columns from the previous frame, which
                // left wrapping values one character wide.
                const LABEL: f32 = 104.0;
                const BUTTON: f32 = 24.0;
                const GAP: f32 = 12.0;
                let value_width = (ui.available_width() - LABEL - BUTTON - 2.0 * GAP).max(120.0);
                for (label, value, copyable) in &rows {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        ui.allocate_ui_with_layout(
                            vec2(LABEL, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_width(LABEL);
                                ui.add(
                                    egui::Label::new(RichText::new(label).color(p.muted)).wrap(),
                                );
                            },
                        );
                        ui.allocate_ui_with_layout(
                            vec2(value_width, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_width(value_width);
                                ui.add(egui::Label::new(value).wrap().selectable(true));
                            },
                        );
                        if *copyable
                            && icons::button(
                                ui,
                                Icon::Copy,
                                &format!("Copy {}", label.to_lowercase()),
                                true,
                            )
                            .clicked()
                        {
                            copied = Some(value.clone());
                        }
                    });
                    ui.add_space(4.0);
                }
                if let Some(note) = &note {
                    ui.label(RichText::new(note).color(p.muted));
                }
            },
        );
        if let Some(value) = copied {
            ctx.copy_text(value);
            self.toast("Copied", ToastKind::Success);
        }
        !(footer.cancel || footer.submit)
    }

    pub(super) fn dialog_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Tested(result) => {
                if let Some(Modal::Connection(form)) = self.modal.as_mut() {
                    form.busy = false;
                    match result {
                        Ok(count) => {
                            form.status = Some(format!(
                                "Connected. {} available.",
                                format::plural(count, "bucket")
                            ));
                            form.error = None;
                        }
                        Err(error) => {
                            form.status = None;
                            form.error = Some(error);
                        }
                    }
                }
            }
            Reply::Saved(result) => match result {
                Ok(connection) => {
                    self.modal = None;
                    self.load_connections();
                    self.switch_connection(&connection.id);
                }
                Err(error) => {
                    if let Some(Modal::Connection(form)) = self.modal.as_mut() {
                        form.busy = false;
                        form.status = None;
                        form.error = Some(error);
                    }
                }
            },
            Reply::PromptFinished { result } => match result {
                Ok(key) => {
                    self.modal = None;
                    if let Some(bucket) = self.location.bucket.clone() {
                        self.invalidate_tree(&bucket, Some(&self.location.prefix.clone()));
                    }
                    self.load_listing(true, Some(key));
                }
                Err(error) => {
                    if let Some(Modal::Prompt(form)) = self.modal.as_mut() {
                        form.busy = false;
                        form.error = Some(error);
                    }
                }
            },
            Reply::Properties { key, result } => {
                if let Some(Modal::Properties(view)) = self.modal.as_mut() {
                    if view.item.key == key {
                        view.info = Some(result);
                    }
                }
            }
            Reply::TextSaved { key, text, result } => self.text_saved(key, text, result),
        }
    }
}
