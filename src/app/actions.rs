//! What the user can do to buckets, folders and files, and the context menus
//! that expose it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::Ui;

use super::dialogs::{ConfirmAction, Modal, PromptPurpose};
use super::icons::Icon;
use super::tree::{menu_item, menu_item_styled};
use super::{Clip, ClipMode, Event, FathomApp, Item, ItemKind, Location, RunningOp, ToastKind};
use crate::format;
use crate::local;
use crate::ops::{self, Move};
use crate::transfers::{Kind, Request};

const PRESIGN_OPTIONS: [(&str, u64); 3] =
    [("1 hour", 3600), ("24 hours", 86_400), ("7 days", 604_800)];

fn shortcut(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("Cmd+{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

fn describe(items: &[Item]) -> String {
    match items {
        [item] => format!("\"{}\"", item.name),
        _ => format::plural(items.len(), "item"),
    }
}

/// `name copy.ext`, `name copy 2.ext`, ... so a duplicate does not overwrite its source.
fn copy_name(name: &str, taken: &HashSet<String>) -> String {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| {
            if n == 1 {
                format!("{stem} copy{extension}")
            } else {
                format!("{stem} copy {n}{extension}")
            }
        })
        .find(|candidate| !taken.contains(candidate))
        .unwrap()
}

impl FathomApp {
    pub(super) fn open_item(&mut self, item: &Item) {
        match item.kind {
            ItemKind::Bucket => self.navigate(
                Location {
                    bucket: Some(item.name.clone()),
                    prefix: String::new(),
                },
                None,
            ),
            ItemKind::Folder => {
                let bucket = self.location.bucket.clone();
                self.navigate(
                    Location {
                        bucket,
                        prefix: item.key.clone(),
                    },
                    None,
                );
            }
            ItemKind::File if format::kind_of(&item.name).is_media() => self.play(item),
            ItemKind::File => self.open_viewer(item.clone(), false),
        }
    }

    fn names_of(&self, kind: ItemKind) -> HashSet<String> {
        self.listing
            .items
            .iter()
            .filter(|i| i.kind == kind)
            .map(|i| i.name.clone())
            .collect()
    }

    pub(super) fn new_folder(&mut self) {
        if self.location.bucket.is_none() {
            return;
        }
        let taken = self.names_of(ItemKind::Folder);
        self.modal = Some(Modal::prompt(
            "New folder",
            "Folder name",
            "New folder",
            "Create",
            PromptPurpose::NewFolder,
            taken,
            None,
        ));
    }

    pub(super) fn new_file(&mut self) {
        if self.location.bucket.is_none() {
            return;
        }
        let taken = self.names_of(ItemKind::File);
        self.modal = Some(Modal::prompt(
            "New text file",
            "File name",
            "untitled.txt",
            "Create",
            PromptPurpose::NewFile,
            taken,
            None,
        ));
    }

    pub(super) fn rename(&mut self, item: &Item) {
        if self.location.bucket.is_none() || item.kind == ItemKind::Bucket {
            return;
        }
        let folder = item.kind == ItemKind::Folder;
        let mut taken = self.names_of(item.kind);
        taken.remove(&item.name);
        let hint = folder.then(|| {
            "R2 has no real folders, so every object inside is copied to the new name and the originals are removed.".to_string()
        });
        self.modal = Some(Modal::prompt(
            if folder {
                "Rename folder"
            } else {
                "Rename file"
            },
            "New name",
            &item.name,
            "Rename",
            PromptPurpose::Rename(item.clone()),
            taken,
            hint,
        ));
    }

    pub(super) fn confirm_delete(&mut self, items: Vec<Item>) {
        let items: Vec<Item> = items
            .into_iter()
            .filter(|i| i.kind != ItemKind::Bucket)
            .collect();
        let Some(bucket) = self.location.bucket.clone() else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let folders = items.iter().any(|i| i.kind == ItemKind::Folder);
        self.modal = Some(Modal::confirm(
            format!("Delete {}?", describe(&items)),
            format!(
                "This permanently deletes {} from \"{bucket}\".",
                describe(&items)
            ),
            format!(
                "{}This cannot be undone.",
                if folders {
                    "Folders are deleted together with everything inside them. "
                } else {
                    ""
                }
            ),
            "Delete",
            true,
            ConfirmAction::Delete(items),
        ));
    }

    /// Runs a cancellable bulk operation on a worker thread behind a progress toast.
    fn run_op(
        &mut self,
        label: String,
        changed: Vec<(String, Option<String>)>,
        select: Option<String>,
        work: impl FnOnce(&AtomicBool, ops::Progress<'_>) -> Result<String, String> + Send + 'static,
    ) {
        let id = self.next_id();
        let cancel = Arc::new(AtomicBool::new(false));
        self.ops.push(RunningOp {
            id,
            label,
            done: 0,
            total: 0,
            cancel: cancel.clone(),
        });
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let last = AtomicU64::new(0);
            let progress = |done: usize, total: usize| {
                let now = started.elapsed().as_millis() as u64;
                if done == 0
                    || done == total
                    || now.saturating_sub(last.load(Ordering::Relaxed)) >= 60
                {
                    last.store(now, Ordering::Relaxed);
                    let _ = tx.send(Event::OpProgress { id, done, total });
                    ctx.request_repaint();
                }
            };
            let result = work(&cancel, &progress);
            let _ = tx.send(Event::OpFinished {
                id,
                result,
                changed,
                select,
            });
            ctx.request_repaint();
        });
    }

    pub(super) fn run_delete(&mut self, items: Vec<Item>) {
        let (Some(bucket), Ok(client)) = (self.location.bucket.clone(), self.client()) else {
            return;
        };
        let keys: Vec<String> = items
            .iter()
            .filter(|i| i.is_file())
            .map(|i| i.key.clone())
            .collect();
        let prefixes: Vec<String> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Folder)
            .map(|i| i.key.clone())
            .collect();
        let label = format!("Deleting {}...", describe(&items));
        let changed = vec![(bucket.clone(), Some(self.location.prefix.clone()))];
        self.run_op(label, changed, None, move |cancel, progress| {
            let count = ops::delete(&client, &bucket, &keys, &prefixes, cancel, progress)
                .map_err(|e| e.to_string())?;
            Ok(format!("Deleted {}", format::plural(count, "object")))
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_transfer(
        &mut self,
        label: String,
        src_bucket: String,
        dst_bucket: String,
        moves: Vec<Move>,
        delete_source: bool,
        select: Option<String>,
        target_prefix: String,
    ) {
        let Ok(client) = self.client() else {
            return;
        };
        let mut changed = vec![(dst_bucket.clone(), Some(target_prefix))];
        if delete_source {
            changed.push((src_bucket.clone(), None));
        }
        self.run_op(label, changed, select, move |cancel, progress| {
            ops::transfer(
                &client,
                &src_bucket,
                &dst_bucket,
                &moves,
                delete_source,
                cancel,
                progress,
            )
            .map(|_| String::new())
            .map_err(|e| e.to_string())
        });
    }

    pub(super) fn copy_or_cut(&mut self, items: &[Item], mode: ClipMode) {
        let items: Vec<Item> = items
            .iter()
            .filter(|i| i.kind != ItemKind::Bucket)
            .cloned()
            .collect();
        let (Some(bucket), Some(conn)) = (self.location.bucket.clone(), self.active.clone()) else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let verb = if mode == ClipMode::Cut {
            "Cut"
        } else {
            "Copied"
        };
        self.toast(
            format!("{verb} {}. Paste it in another folder.", describe(&items)),
            ToastKind::Info,
        );
        self.clipboard = Some(Clip {
            mode,
            conn,
            bucket,
            items,
        });
    }

    fn can_paste(&self) -> bool {
        self.clipboard
            .as_ref()
            .is_some_and(|clip| Some(&clip.conn) == self.active.as_ref())
            && self.location.bucket.is_some()
    }

    pub(super) fn paste(&mut self, target: &str) {
        if !self.can_paste() {
            return;
        }
        let Some(bucket) = self.location.bucket.clone() else {
            return;
        };
        let clip = self.clipboard.as_ref().unwrap();
        let into_current = target == self.location.prefix;
        let mut taken: HashSet<String> = if into_current {
            self.listing.items.iter().map(|i| i.name.clone()).collect()
        } else {
            HashSet::new()
        };
        let mut moves = Vec::new();
        let mut replaced = 0;
        for item in &clip.items {
            let folder = item.kind == ItemKind::Folder;
            let suffix = if folder { "/" } else { "" };
            let mut name = item.name.clone();
            let same_spot = clip.bucket == bucket && format!("{target}{name}{suffix}") == item.key;
            if same_spot {
                if clip.mode == ClipMode::Cut {
                    continue;
                }
                name = copy_name(&name, &taken);
            } else if taken.contains(&name) {
                replaced += 1;
            }
            taken.insert(name.clone());
            let to = format!("{target}{name}{suffix}");
            if folder && clip.bucket == bucket && to.starts_with(&item.key) {
                let message = format!("\"{}\" cannot be placed inside itself", item.name);
                self.toast(message, ToastKind::Error);
                return;
            }
            moves.push(Move {
                from: item.key.clone(),
                to,
                folder,
                size: item.size,
            });
        }
        if moves.is_empty() {
            return;
        }
        let moving = clip.mode == ClipMode::Cut;
        let label = format!(
            "{} {}...",
            if moving { "Moving" } else { "Copying" },
            describe(&clip.items)
        );
        let src_bucket = clip.bucket.clone();
        if replaced > 0 {
            let verb = if replaced == 1 { "exists" } else { "exist" };
            self.modal = Some(Modal::confirm(
                "Replace existing items?".into(),
                format!(
                    "{} with the same name already {verb} here.",
                    format::plural(replaced, "item")
                ),
                "Files with matching names will be overwritten.".into(),
                "Replace",
                true,
                ConfirmAction::Paste {
                    src_bucket,
                    moves,
                    delete_source: moving,
                    label,
                    target: target.to_string(),
                },
            ));
            return;
        }
        self.run_transfer(
            label,
            src_bucket,
            bucket,
            moves,
            moving,
            None,
            target.to_string(),
        );
        if moving {
            self.clipboard = None;
        }
    }

    // ----- uploads and downloads ---------------------------------------------------------

    pub(super) fn upload_pick(&mut self, folders: bool) {
        let Some(bucket) = self.location.bucket.clone() else {
            self.toast("Open a bucket to upload files", ToastKind::Info);
            return;
        };
        let dialog = rfd::FileDialog::new();
        let picked = if folders {
            dialog.set_title("Upload folders").pick_folders()
        } else {
            dialog.set_title("Upload files").pick_files()
        };
        if let Some(paths) = picked {
            let prefix = self.location.prefix.clone();
            self.upload_paths(paths, bucket, prefix);
        }
    }

    pub(super) fn upload_paths(&mut self, paths: Vec<PathBuf>, bucket: String, prefix: String) {
        let (Ok(client), Some(conn)) = (self.client(), self.active.clone()) else {
            return;
        };
        let transfers = self.transfers.clone();
        self.spawn(move || {
            // Walking folders can take a while, so it happens off the UI thread.
            let requests: Vec<Request> = local::collect_uploads(&paths, &prefix)
                .into_iter()
                .map(|source| Request {
                    kind: Kind::Upload,
                    client: client.clone(),
                    conn_id: conn.clone(),
                    bucket: bucket.clone(),
                    key: source.key,
                    path: source.path,
                    size: source.size,
                })
                .collect();
            Event::Queued {
                count: transfers.enqueue(requests),
                verb: "Uploading",
            }
        });
    }

    pub(super) fn download(&mut self, items: Vec<Item>) {
        let (Some(bucket), Ok(client), Some(conn)) = (
            self.location.bucket.clone(),
            self.client(),
            self.active.clone(),
        ) else {
            return;
        };
        let files: Vec<Item> = items.iter().filter(|i| i.is_file()).cloned().collect();
        let prefixes: Vec<String> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Folder)
            .map(|i| i.key.clone())
            .collect();
        if files.is_empty() && prefixes.is_empty() {
            return;
        }
        let transfers = self.transfers.clone();
        let request = move |key: String, size: u64, path: PathBuf| Request {
            kind: Kind::Download,
            client: client.clone(),
            conn_id: conn.clone(),
            bucket: bucket.clone(),
            key,
            path,
            size,
        };

        if let ([file], true) = (files.as_slice(), prefixes.is_empty()) {
            let name = local::sanitize_component(format::base_name(&file.key));
            let Some(dest) = rfd::FileDialog::new()
                .set_title("Save as")
                .set_file_name(name)
                .save_file()
            else {
                return;
            };
            let count = transfers.enqueue(vec![request(file.key.clone(), file.size, dest)]);
            self.handle(Event::Queued {
                count,
                verb: "Downloading",
            });
            return;
        }
        let Some(folder) = rfd::FileDialog::new()
            .set_title("Download to")
            .pick_folder()
        else {
            return;
        };
        let client = self.client().ok();
        let bucket = self.location.bucket.clone().unwrap_or_default();
        self.spawn(move || {
            let mut requests = Vec::new();
            for file in &files {
                let dest =
                    local::unique_path(local::path_for_key(&folder, format::base_name(&file.key)));
                requests.push(request(file.key.clone(), file.size, dest));
            }
            let never = AtomicBool::new(false);
            for prefix in &prefixes {
                let root =
                    local::unique_path(local::path_for_key(&folder, format::base_name(prefix)));
                if let Err(error) = std::fs::create_dir_all(&root) {
                    return Event::Toast {
                        text: error.to_string(),
                        error: true,
                    };
                }
                let Some(client) = client.as_ref() else {
                    continue;
                };
                let objects = match client.list_all(&bucket, prefix, &never) {
                    Ok(objects) => objects,
                    Err(error) => {
                        return Event::Toast {
                            text: error.to_string(),
                            error: true,
                        }
                    }
                };
                for object in objects {
                    let dest = local::path_for_key(&root, &object.key[prefix.len()..]);
                    if object.key.ends_with('/') {
                        // Folder marker: nothing to download, but keep empty folders.
                        let _ = std::fs::create_dir_all(&dest);
                    } else {
                        requests.push(request(object.key, object.size, dest));
                    }
                }
            }
            Event::Queued {
                count: transfers.enqueue(requests),
                verb: "Downloading",
            }
        });
    }

    // ----- links and properties ------------------------------------------------------------

    /// Copies public URLs, asking for the bucket's domain first if it has none.
    pub(super) fn copy_public_urls(&mut self, items: &[Item]) {
        let Some(bucket) = self.location.bucket.clone() else {
            return;
        };
        let files: Vec<Item> = items.iter().filter(|i| i.is_file()).cloned().collect();
        if files.is_empty() {
            return;
        }
        let Some(base) = self.domain_for(&bucket) else {
            self.modal = Some(Modal::domain(&bucket, None, files));
            return;
        };
        let urls: Vec<String> = files
            .iter()
            .map(|i| format::public_url(&base, &i.key))
            .collect();
        let message = if urls.len() == 1 {
            "Public URL copied".to_string()
        } else {
            format!("{} public URLs copied", urls.len())
        };
        self.ctx.copy_text(urls.join("\n"));
        self.toast(message, ToastKind::Success);
    }

    fn copy_presigned(&mut self, item: &Item, seconds: u64, label: &str) {
        let (Some(bucket), Ok(client)) = (self.location.bucket.clone(), self.client()) else {
            return;
        };
        self.ctx
            .copy_text(client.presign_get(&bucket, &item.key, seconds));
        self.toast(
            format!("Link copied. It expires in {label} and includes your account ID and access key ID."),
            ToastKind::Success,
        );
    }

    pub(super) fn show_properties(&mut self, item: &Item) {
        let Some(bucket) = self.location.bucket.clone() else {
            return;
        };
        self.modal = Some(Modal::properties(item.clone(), bucket.clone()));
        if !item.is_file() {
            return;
        }
        let Ok(client) = self.client() else {
            return;
        };
        let key = item.key.clone();
        self.spawn(move || {
            let result = client.head(&bucket, &key).map_err(|e| e.to_string());
            Event::Dialog(super::dialogs::Reply::Properties { key, result })
        });
    }

    // ----- menus --------------------------------------------------------------------------------

    pub(super) fn item_menu(&mut self, ui: &mut Ui, items: Vec<Item>) {
        if items.is_empty() {
            return self.background_menu(ui);
        }
        let single = (items.len() == 1).then(|| items[0].clone());
        if let Some(item) = single.as_ref().filter(|i| i.kind == ItemKind::Bucket) {
            return self.bucket_menu(ui, &item.name);
        }
        let files: Vec<&Item> = items.iter().filter(|i| i.is_file()).collect();
        let has_domain = self
            .location
            .bucket
            .as_ref()
            .and_then(|b| self.domain_for(b))
            .is_some();

        if let Some(item) = &single {
            let (icon, label) = match item.kind {
                ItemKind::Folder => (Icon::Folder, "Open"),
                _ if format::kind_of(&item.name).is_media() => (Icon::Play, "Play"),
                _ => (Icon::Eye, "Preview"),
            };
            if menu_item(ui, Some(icon), label, "Enter").clicked() {
                self.open_item(item);
            }
        }
        let download = if single.is_some() {
            "Download".to_string()
        } else {
            format!("Download {}", format::plural(items.len(), "item"))
        };
        if menu_item(ui, Some(Icon::Download), &download, "").clicked() {
            self.download(items.clone());
        }
        ui.separator();
        if !files.is_empty() {
            let label = if files.len() == 1 {
                "Copy public URL".to_string()
            } else {
                format!("Copy {} public URLs", files.len())
            };
            let keys = if has_domain {
                shortcut("L")
            } else {
                String::new()
            };
            if menu_item(ui, Some(Icon::Link), &label, &keys).clicked() {
                self.copy_public_urls(&items);
            }
        }
        if let Some(item) = single.as_ref().filter(|i| i.is_file()) {
            for (label, seconds) in PRESIGN_OPTIONS {
                if menu_item(
                    ui,
                    Some(Icon::Key),
                    &format!("Copy presigned URL ({label})"),
                    "",
                )
                .clicked()
                {
                    self.copy_presigned(item, seconds, label);
                }
            }
        }
        let key_label = match &single {
            Some(item) if item.kind == ItemKind::Folder => "Copy folder path",
            Some(_) => "Copy object key",
            None => "Copy object keys",
        };
        if menu_item(ui, Some(Icon::Copy), key_label, "").clicked() {
            let keys: Vec<String> = items.iter().map(|i| i.key.clone()).collect();
            ui.ctx().copy_text(keys.join("\n"));
            self.toast("Copied", ToastKind::Success);
        }
        ui.separator();
        if menu_item(ui, Some(Icon::Cut), "Cut", &shortcut("X")).clicked() {
            self.copy_or_cut(&items, ClipMode::Cut);
        }
        if menu_item(ui, Some(Icon::Copy), "Copy", &shortcut("C")).clicked() {
            self.copy_or_cut(&items, ClipMode::Copy);
        }
        if let Some(item) = single
            .as_ref()
            .filter(|i| i.kind == ItemKind::Folder && self.can_paste())
        {
            if menu_item(ui, Some(Icon::Paste), "Paste into folder", "").clicked() {
                let target = item.key.clone();
                self.paste(&target);
            }
        }
        if let Some(item) = &single {
            if menu_item(ui, Some(Icon::Pencil), "Rename...", "F2").clicked() {
                self.rename(item);
            }
        }
        let delete_key = if cfg!(target_os = "macos") {
            "Cmd+Backspace"
        } else {
            "Del"
        };
        if menu_item_styled(ui, Some(Icon::Trash), "Delete", delete_key, true, true).clicked() {
            self.confirm_delete(items.clone());
        }
        if let Some(item) = &single {
            ui.separator();
            if menu_item(ui, Some(Icon::Info), "Properties", "").clicked() {
                self.show_properties(item);
            }
        }
    }

    pub(super) fn background_menu(&mut self, ui: &mut Ui) {
        let Some(bucket) = self.location.bucket.clone() else {
            if menu_item(ui, Some(Icon::Refresh), "Refresh", "F5").clicked() {
                self.load_buckets();
            }
            ui.separator();
            if menu_item(ui, Some(Icon::Plus), "Add connection...", "").clicked() {
                self.modal = Some(Modal::connection(None));
            }
            return;
        };
        if menu_item(ui, Some(Icon::Upload), "Upload files...", &shortcut("U")).clicked() {
            self.upload_pick(false);
        }
        if menu_item(ui, Some(Icon::Folder), "Upload folder...", "").clicked() {
            self.upload_pick(true);
        }
        ui.separator();
        if menu_item(
            ui,
            Some(Icon::FolderPlus),
            "New folder...",
            &shortcut("Shift+N"),
        )
        .clicked()
        {
            self.new_folder();
        }
        if menu_item(ui, Some(Icon::FilePlus), "New text file...", "").clicked() {
            self.new_file();
        }
        ui.separator();
        let can_paste = self.can_paste();
        if menu_item_styled(
            ui,
            Some(Icon::Paste),
            "Paste",
            &shortcut("V"),
            can_paste,
            false,
        )
        .clicked()
        {
            let prefix = self.location.prefix.clone();
            self.paste(&prefix);
        }
        ui.separator();
        if menu_item(ui, Some(Icon::Refresh), "Refresh", "F5").clicked() {
            self.refresh();
        }
        if menu_item(ui, Some(Icon::Globe), "Public domain...", "").clicked() {
            let current = self.domain_for(&bucket);
            self.modal = Some(Modal::domain(&bucket, current, Vec::new()));
        }
    }

    pub(super) fn bucket_menu(&mut self, ui: &mut Ui, bucket: &str) {
        if menu_item(ui, Some(Icon::Bucket), "Open", "").clicked() {
            self.navigate(
                Location {
                    bucket: Some(bucket.to_string()),
                    prefix: String::new(),
                },
                None,
            );
        }
        ui.separator();
        if menu_item(ui, Some(Icon::Globe), "Public domain...", "").clicked() {
            let current = self.domain_for(bucket);
            self.modal = Some(Modal::domain(bucket, current, Vec::new()));
        }
        if menu_item(ui, Some(Icon::Copy), "Copy bucket name", "").clicked() {
            ui.ctx().copy_text(bucket.to_string());
            self.toast("Copied", ToastKind::Success);
        }
    }
}
