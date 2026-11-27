//! The commands, and what they share: finding the connection, picking objects
//! with patterns, moving files, and purging the cache.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::cloudflare::{self, Api};
use crate::config::{Connection, Credentials, Store};
use crate::format::{self, plural, thousands};
use crate::local::{self, UploadSource};
use crate::ops;
use crate::pattern::{Pattern, Syntax};
use crate::s3::{Bucket, Object, S3Client};
use crate::update::{self, Checks, GitHub, Method};

use super::args::{Args, Command};
use super::{available, Cli, Error};

/// Files uploaded or downloaded at the same time.
const WORKERS: usize = 4;
/// Objects shown before asking whether to delete them.
const SHOWN_BEFORE_ASKING: usize = 10;
const ENV_HINT: &str = "Or set CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN to connect \
                        without a saved connection.";

pub(super) fn run(args: &Args, cli: &mut Cli) -> Result<(), Error> {
    match args.command {
        Command::Ls => ls(args, cli),
        Command::Upload => upload(args, cli),
        Command::Download => download(args, cli),
        Command::Delete => delete(args, cli),
        Command::Purge => purge(args, cli),
        Command::Connections => connections(cli),
        Command::Update => install_update(args, cli),
    }
}

// ----- commands ------------------------------------------------------------------

fn ls(args: &Args, cli: &mut Cli) -> Result<(), Error> {
    let Some((bucket, texts)) = args.positional.split_first() else {
        let target = connect(args, cli)?;
        let buckets = if target.buckets.is_empty() {
            target.client.list_buckets()?
        } else {
            target
                .buckets
                .iter()
                .map(|name| Bucket {
                    name: name.clone(),
                    created: String::new(),
                })
                .collect()
        };
        for bucket in buckets {
            if args.long {
                cli.say(format!("{:<16}  {}", date(&bucket.created), bucket.name));
            } else {
                cli.say(bucket.name);
            }
        }
        return Ok(());
    };
    check_bucket(bucket)?;
    let patterns = patterns(args, texts)?;
    let target = connect(args, cli)?;

    if patterns.is_empty() {
        // The top of the bucket, the way the app shows it: folders and files.
        let mut rows: Vec<(String, Option<u64>, String)> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = target
                .client
                .list(bucket, "", Some("/"), token.as_deref())?;
            rows.extend(page.prefixes.into_iter().map(|p| (p, None, String::new())));
            rows.extend(
                page.objects
                    .into_iter()
                    .map(|o| (o.key, Some(o.size), o.modified)),
            );
            token = page.next_token;
            if token.is_none() {
                break;
            }
        }
        rows.sort();
        for (key, size, modified) in rows {
            cli.say(row(args.long, &key, size, &modified));
        }
        return Ok(());
    }

    let selection = select(cli, &target.client, bucket, &patterns)?;
    for pick in &selection.picks {
        let object = &pick.object;
        cli.say(row(
            args.long,
            &object.key,
            Some(object.size),
            &object.modified,
        ));
    }
    finish(report_unmatched(cli, bucket, &patterns, &selection.matched))
}

fn upload(args: &Args, cli: &mut Cli) -> Result<(), Error> {
    let (bucket, texts) = bucket_and_rest(args, "the files or folders to upload")?;
    let folder = key_folder(args.to.as_deref());

    // Every file is found before any is sent, so a mistake stops the upload
    // before it starts.
    let mut missing = false;
    let mut sources: Vec<UploadSource> = Vec::new();
    for text in texts {
        match find_files(cli, text, syntax(args)) {
            Ok(found) => sources.extend(found),
            Err(Error::Failed(message)) => {
                cli.warn(message);
                missing = true;
            }
            Err(other) => return Err(other),
        }
    }
    let mut seen = HashSet::new();
    let mut owners: HashMap<String, PathBuf> = HashMap::new();
    let mut jobs = Vec::new();
    for source in sources {
        if !seen.insert(source.path.clone()) {
            continue;
        }
        let key = format!("{folder}{}", source.key);
        if let Some(other) = owners.insert(key.clone(), source.path.clone()) {
            return Err(Error::Failed(format!(
                "{} and {} would both be uploaded as {key}",
                cli.shown(&other),
                cli.shown(&source.path)
            )));
        }
        jobs.push(Job {
            label: format!("{} -> {bucket}/{key}", cli.shown(&source.path)),
            key,
            path: source.path,
            size: source.size,
        });
    }
    if jobs.is_empty() {
        return Err(Error::Reported);
    }

    let target = connect(args, cli)?;
    let cache = if args.purge {
        Some(cache(&target, bucket)?)
    } else {
        None
    };
    if args.dry_run {
        for job in &jobs {
            cli.say(&job.label);
        }
        cli.say(format!(
            "Would upload {} ({}) to {bucket}.",
            plural(jobs.len(), "file"),
            format::size(jobs.iter().map(|j| j.size).sum())
        ));
        if let Some(cache) = &cache {
            cli.say(format!(
                "Would then purge them from the cache of {}.",
                cache.hosts()
            ));
        }
        return finish(missing);
    }

    let worked = transfer(cli, &target.client, bucket, &jobs, Direction::Upload);
    let uploaded: Vec<&Job> = jobs
        .iter()
        .zip(&worked)
        .filter(|(_, ok)| **ok)
        .map(|(job, _)| job)
        .collect();
    cli.say(summary(
        "Uploaded",
        &uploaded,
        jobs.len(),
        &format!("to {bucket}"),
    ));
    if let Some(cache) = &cache {
        if !uploaded.is_empty() {
            let keys: Vec<String> = uploaded.iter().map(|job| job.key.clone()).collect();
            purge_keys(cli, cache, &keys, &[])?;
        }
    }
    finish(missing || uploaded.len() < jobs.len())
}

fn download(args: &Args, cli: &mut Cli) -> Result<(), Error> {
    let (bucket, texts) = bucket_and_rest(args, "what to download")?;
    let patterns = patterns(args, texts)?;
    let destination = match &args.to {
        Some(to) => cli.cx.cwd.join(to),
        None => cli.cx.cwd.clone(),
    };
    let target = connect(args, cli)?;
    let selection = select(cli, &target.client, bucket, &patterns)?;
    let unmatched = report_unmatched(cli, bucket, &patterns, &selection.matched);

    let mut folders = Vec::new();
    let mut jobs = Vec::new();
    // Paths as the file system compares them, and the key saved at each.
    let mut owners: HashMap<String, &str> = HashMap::new();
    for pick in &selection.picks {
        let key = pick.object.key.as_str();
        let path = local::path_for_key(&destination, patterns[pick.pattern].relative(key));
        // A folder marker: nothing to download, but empty folders are kept.
        if key.ends_with('/') {
            folders.push(path);
            continue;
        }
        let name = path.to_string_lossy();
        let name = if cfg!(any(windows, target_os = "macos")) {
            name.to_lowercase()
        } else {
            name.into_owned()
        };
        if let Some(other) = owners.insert(name, key) {
            return Err(Error::Failed(format!(
                "{other} and {key} would both be saved as {}. Download them one at a time.",
                cli.shown(&path)
            )));
        }
        jobs.push(Job {
            label: format!("{bucket}/{key} -> {}", cli.shown(&path)),
            key: key.to_string(),
            path,
            size: pick.object.size,
        });
    }
    if jobs.is_empty() && folders.is_empty() {
        return Err(Error::Reported);
    }
    let place = format!("to {}", cli.shown(&destination));
    if args.dry_run {
        for job in &jobs {
            cli.say(&job.label);
        }
        cli.say(format!(
            "Would download {} ({}) {place}.",
            plural(jobs.len(), "file"),
            format::size(jobs.iter().map(|j| j.size).sum())
        ));
        return finish(unmatched);
    }

    let mut failed = false;
    for folder in &folders {
        if let Err(error) = fs::create_dir_all(folder) {
            cli.warn(format!("Could not create {}: {error}", cli.shown(folder)));
            failed = true;
        }
    }
    let worked = transfer(cli, &target.client, bucket, &jobs, Direction::Download);
    let downloaded: Vec<&Job> = jobs
        .iter()
        .zip(&worked)
        .filter(|(_, ok)| **ok)
        .map(|(job, _)| job)
        .collect();
    cli.say(summary("Downloaded", &downloaded, jobs.len(), &place));
    finish(unmatched || failed || downloaded.len() < jobs.len())
}

fn delete(args: &Args, cli: &mut Cli) -> Result<(), Error> {
    let (bucket, texts) = bucket_and_rest(args, "what to delete")?;
    let patterns = patterns(args, texts)?;
    let target = connect(args, cli)?;
    let selection = select(cli, &target.client, bucket, &patterns)?;
    let unmatched = report_unmatched(cli, bucket, &patterns, &selection.matched);
    let keys: Vec<String> = selection.picks.into_iter().map(|p| p.object.key).collect();
    if keys.is_empty() {
        return Err(Error::Reported);
    }
    // Found before deleting, so a bucket without a cache to purge stops here.
    let cache = if args.purge {
        Some(cache(&target, bucket)?)
    } else {
        None
    };
    let count = plural(keys.len(), "object");

    if args.dry_run {
        for key in &keys {
            cli.say(key);
        }
        cli.say(format!("Would delete {count} from {bucket}."));
        if let Some(cache) = &cache {
            cli.say(format!(
                "Would then purge them from the cache of {}.",
                cache.hosts()
            ));
        }
        return finish(unmatched);
    }
    if !args.yes {
        if !cli.cx.interactive {
            return Err(Error::Failed(format!(
                "Not deleting {count} without asking. Pass --yes to delete them, or --dry-run \
                 to see what they are."
            )));
        }
        for key in keys.iter().take(SHOWN_BEFORE_ASKING) {
            cli.say(key);
        }
        if keys.len() > SHOWN_BEFORE_ASKING {
            cli.say(format!(
                "...and {} more",
                thousands((keys.len() - SHOWN_BEFORE_ASKING) as u64)
            ));
        }
        if !cli.confirm(&format!("Delete {count} from {bucket}?")) {
            cli.say("Nothing was deleted.");
            return Ok(());
        }
    }

    let never = AtomicBool::new(false);
    let done = AtomicUsize::new(0);
    let result = wait(
        cli,
        || {
            ops::delete(
                &target.client,
                bucket,
                &keys,
                &[],
                &never,
                &|finished, _| {
                    done.fetch_max(finished, Ordering::Relaxed);
                },
            )
        },
        || {
            let finished = done.load(Ordering::Relaxed) as u64;
            format!("Deleting {} of {count}", thousands(finished))
        },
    );
    let failed = match result {
        Ok(deleted) => {
            cli.say(format!(
                "Deleted {} from {bucket}.",
                plural(deleted, "object")
            ));
            false
        }
        Err(error) => {
            let deleted = done.load(Ordering::Relaxed);
            cli.warn(format!(
                "Deleted {} of {count} from {bucket}, then: {error}",
                thousands(deleted as u64)
            ));
            if deleted == 0 {
                return Err(Error::Reported);
            }
            true
        }
    };
    if let Some(cache) = &cache {
        // Some may not have been deleted after a failure, but purging them is harmless.
        let files: Vec<String> = keys.into_iter().filter(|k| !k.ends_with('/')).collect();
        purge_keys(cli, cache, &files, &[])?;
    }
    finish(unmatched || failed)
}

fn purge(args: &Args, cli: &mut Cli) -> Result<(), Error> {
    let (bucket, texts) = bucket_and_rest(args, "what to purge")?;
    let patterns = patterns(args, texts)?;
    let target = connect(args, cli)?;
    let cache = cache(&target, bucket)?;

    // Named in full, a file is purged even if it is gone from the bucket, and
    // a folder whatever is left in it, so neither needs a listing.
    let mut files = BTreeSet::new();
    let mut folders = BTreeSet::new();
    for pattern in patterns.iter().filter(|p| p.is_path()) {
        if pattern.text.ends_with('/') {
            folders.insert(pattern.text.clone());
            continue;
        }
        files.insert(pattern.text.clone());
        // It may also be the name of a folder.
        let folder = format!("{}/", pattern.text);
        if !target
            .client
            .list(bucket, &folder, None, None)?
            .objects
            .is_empty()
        {
            folders.insert(folder);
        }
    }
    let wildcards: Vec<Pattern> = patterns.iter().filter(|p| !p.is_path()).cloned().collect();
    let mut unmatched = false;
    if !wildcards.is_empty() {
        let selection = select(cli, &target.client, bucket, &wildcards)?;
        unmatched = report_unmatched(cli, bucket, &wildcards, &selection.matched);
        let picked = selection.picks.into_iter().map(|pick| pick.object.key);
        files.extend(picked.filter(|key| !key.ends_with('/')));
    }
    files.retain(|key: &String| {
        !folders
            .iter()
            .any(|folder| key.starts_with(folder.as_str()))
    });
    if files.is_empty() && folders.is_empty() {
        return Err(Error::Reported);
    }

    let files: Vec<String> = files.into_iter().collect();
    let folders: Vec<String> = folders.into_iter().collect();
    if args.dry_run {
        for url in purge_urls(&cache, &files, &folders) {
            cli.say(url);
        }
        cli.say(format!(
            "Would purge {} from the cache of {}.",
            purged(files.len(), folders.len()),
            cache.hosts()
        ));
        return finish(unmatched);
    }
    purge_keys(cli, &cache, &files, &folders)?;
    finish(unmatched)
}

fn connections(cli: &mut Cli) -> Result<(), Error> {
    let config = cli.cx.store.snapshot();
    let from_env =
        cli.env("CLOUDFLARE_ACCOUNT_ID").is_some() && cli.env("CLOUDFLARE_API_TOKEN").is_some();
    let default = if from_env {
        None
    } else {
        pick(cli.cx.store, None).ok().map(|c| c.id)
    };
    if config.connections.is_empty() {
        cli.say("No connections are saved in Fathom.");
    }
    let width = config
        .connections
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    for connection in &config.connections {
        let mark = if default.as_ref() == Some(&connection.id) {
            "  (default)"
        } else {
            ""
        };
        cli.say(format!(
            "{:<width$}  {}{mark}",
            connection.name, connection.id
        ));
    }
    if from_env {
        cli.say(
            "CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN are set, so commands use them unless \
             --connection names a saved connection.",
        );
    }
    Ok(())
}

fn install_update(args: &Args, cli: &mut Cli) -> Result<(), Error> {
    if let Some(extra) = args.positional.first() {
        return Err(Error::Usage(format!("update doesn't take {extra}")));
    }
    let github = GitHub::new(cli.env("FATHOM_UPDATE_API").as_deref());
    let release = wait(cli, || github.latest(), || "Checking for updates".into())?;
    if let Some(settings) = &cli.cx.updates {
        Checks::new(settings).record(&release);
    }
    if !release.is_newer() {
        cli.say(format!(
            "Fathom {} is the latest version.",
            update::current()
        ));
        return Ok(());
    }
    cli.say(available(&release.version));
    if args.check {
        cli.say("Run fth update to install it.");
        return Ok(());
    }

    let elsewhere = |error: String| {
        Error::Failed(format!(
            "{error} Download the new version from {}",
            release.page
        ))
    };
    let method = Method::detect().map_err(elsewhere)?;
    let name = method.asset(&release.version).map_err(elsewhere)?;
    let size = format::size(release.size_of(&name).unwrap_or(0));
    let never = AtomicBool::new(false);
    let received = AtomicU64::new(0);
    let file = wait(
        cli,
        || {
            let progress = |done: u64| received.store(done, Ordering::Relaxed);
            github.download(&release, &name, &update::download_dir(), &progress, &never)
        },
        || {
            let done = format::size(received.load(Ordering::Relaxed));
            format!("Downloading {name}: {done} of {size}")
        },
    )?;
    cli.say(format!(
        "Downloaded {name} ({size}). It matches the release's checksum."
    ));
    if method == Method::Installer {
        cli.say("Installing. If Fathom is open, it closes first.");
    } else {
        cli.say("Installing.");
    }
    let installed = update::install(&method, &file, false);
    let _ = fs::remove_file(&file);
    installed?;
    cli.say(format!("Updated Fathom and fth to {}.", release.version));
    Ok(())
}

// ----- connecting ------------------------------------------------------------------

/// A connection, ready to use.
struct Target {
    client: S3Client,
    jurisdiction: String,
    /// Public domains set by hand in the app, by bucket.
    domains: BTreeMap<String, String>,
    /// Buckets set by hand in the app, for tokens that can't list them.
    buckets: Vec<String>,
    /// For connections set up with an API token.
    api: Option<Api>,
}

fn connect(args: &Args, cli: &Cli) -> Result<Target, Error> {
    let api_base = cli
        .env("FATHOM_CLOUDFLARE_API")
        .unwrap_or_else(|| cloudflare::API.to_string());
    let from_env = (
        cli.env("CLOUDFLARE_ACCOUNT_ID"),
        cli.env("CLOUDFLARE_API_TOKEN"),
    );
    if let (None, (Some(account_id), Some(api_token))) = (&args.connection, from_env) {
        let mut credentials = Credentials {
            account_id,
            api_token,
            // Like a connection's custom endpoint, for S3-compatible test servers.
            endpoint: cli.env("FATHOM_ENDPOINT").unwrap_or_default(),
            ..Default::default()
        };
        credentials.normalize();
        let api = Api::with_base(&api_base, &credentials.account_id, &credentials.api_token);
        (credentials.access_key_id, credentials.secret_access_key) = api.s3_keys()?;
        return Ok(Target {
            client: client(&credentials, "")?,
            jurisdiction: String::new(),
            domains: BTreeMap::new(),
            buckets: Vec::new(),
            api: Some(api),
        });
    }

    let connection = pick(cli.cx.store, args.connection.as_deref())?;
    let credentials = cli
        .cx
        .store
        .credentials(&connection.id)
        .map_err(|e| format!("{e} {ENV_HINT}"))?;
    let api = (!credentials.api_token.is_empty())
        .then(|| Api::with_base(&api_base, &credentials.account_id, &credentials.api_token));
    Ok(Target {
        client: client(&credentials, &connection.jurisdiction)?,
        jurisdiction: connection.jurisdiction,
        domains: connection.domains,
        buckets: connection.buckets,
        api,
    })
}

fn client(credentials: &Credentials, jurisdiction: &str) -> Result<S3Client, Error> {
    Ok(S3Client::new(
        &credentials.endpoint_url(jurisdiction),
        &credentials.access_key_id,
        &credentials.secret_access_key,
    )?)
}

/// The saved connection with this ID or name, or else the one last open in
/// the app.
fn pick(store: &Store, name: Option<&str>) -> Result<Connection, String> {
    let config = store.snapshot();
    let all = &config.connections;
    let Some(name) = name else {
        let active = all.iter().find(|c| Some(&c.id) == config.active.as_ref());
        return active
            .or(all.first())
            .cloned()
            .ok_or_else(|| format!("There's no connection to use. Add one in Fathom. {ENV_HINT}"));
    };
    if let Some(connection) = all.iter().find(|c| c.id == name) {
        return Ok(connection.clone());
    }
    let named: Vec<&Connection> = all
        .iter()
        .filter(|c| c.name.eq_ignore_ascii_case(name))
        .collect();
    match named.as_slice() {
        [connection] => Ok((*connection).clone()),
        [] if all.is_empty() => Err("No connections are saved in Fathom".into()),
        [] => Err(format!(
            "No connection is named {name}. Run fth connections to see them."
        )),
        _ => Err(format!(
            "More than one connection is named {name}. Pick one by the ID that fth \
             connections shows."
        )),
    }
}

// ----- picking objects ---------------------------------------------------------------

fn syntax(args: &Args) -> Syntax {
    if args.regex {
        Syntax::Regex
    } else {
        Syntax::Glob
    }
}

fn patterns(args: &Args, texts: &[String]) -> Result<Vec<Pattern>, Error> {
    texts
        .iter()
        .map(|text| Pattern::new(text, syntax(args)).map_err(Error::Usage))
        .collect()
}

/// The bucket, named first, and the arguments after it, of which there must
/// be at least one.
fn bucket_and_rest<'a>(args: &'a Args, what: &str) -> Result<(&'a str, &'a [String]), Error> {
    let Some((bucket, rest)) = args.positional.split_first() else {
        return Err(Error::Usage(format!("Name a bucket, then {what}")));
    };
    check_bucket(bucket)?;
    if rest.is_empty() {
        return Err(Error::Usage(format!("Name {what} after the bucket")));
    }
    Ok((bucket, rest))
}

fn check_bucket(bucket: &str) -> Result<(), Error> {
    match bucket.split_once('/') {
        Some((name, rest)) => Err(Error::Usage(format!(
            "{bucket} isn't the name of a bucket. Name the bucket, then what's in it, as in: \
             {name} {}",
            if rest.is_empty() { "<pattern>" } else { rest }
        ))),
        None => Ok(()),
    }
}

/// An object, and the first of the patterns that picked it.
struct Pick {
    object: Object,
    pattern: usize,
}

struct Selection {
    /// In key order.
    picks: Vec<Pick>,
    /// Whether each pattern picked anything.
    matched: Vec<bool>,
}

fn select(
    cli: &mut Cli,
    client: &S3Client,
    bucket: &str,
    patterns: &[Pattern],
) -> Result<Selection, Error> {
    // Each listing starts at a prefix. One that starts with another is
    // covered by that one's listing.
    let mut prefixes: Vec<&str> = patterns.iter().map(|p| p.prefix.as_str()).collect();
    prefixes.sort_unstable();
    prefixes.dedup_by(|longer, shorter| longer.starts_with(*shorter));

    let mut selection = Selection {
        picks: Vec::new(),
        matched: vec![false; patterns.len()],
    };
    let mut listed = 0u64;
    for prefix in prefixes {
        let mut token: Option<String> = None;
        loop {
            let page = client.list(bucket, prefix, None, token.as_deref())?;
            listed += page.objects.len() as u64;
            for object in page.objects {
                let mut first = None;
                for (index, pattern) in patterns.iter().enumerate() {
                    if pattern.matches(&object.key) {
                        selection.matched[index] = true;
                        first.get_or_insert(index);
                    }
                }
                if let Some(pattern) = first {
                    selection.picks.push(Pick { object, pattern });
                }
            }
            cli.status(&format!(
                "Looking through {} objects in {bucket}",
                thousands(listed)
            ));
            token = page.next_token;
            if token.is_none() {
                break;
            }
        }
    }
    cli.clear_status();
    selection
        .picks
        .sort_by(|a, b| a.object.key.cmp(&b.object.key));
    Ok(selection)
}

/// Reports each pattern that picked nothing. Returns whether there were any.
fn report_unmatched(cli: &mut Cli, bucket: &str, patterns: &[Pattern], matched: &[bool]) -> bool {
    let mut any = false;
    for (pattern, matched) in patterns.iter().zip(matched) {
        if !matched {
            cli.warn(format!("Nothing in {bucket} matches {}", pattern.text));
            any = true;
        }
    }
    any
}

/// Done, unless some problem has been reported along the way.
fn finish(problems: bool) -> Result<(), Error> {
    if problems {
        Err(Error::Reported)
    } else {
        Ok(())
    }
}

/// One line of `ls`. With --long, the size and date come first.
fn row(long: bool, key: &str, size: Option<u64>, modified: &str) -> String {
    if !long {
        return key.to_string();
    }
    let size = size.map(format::size).unwrap_or_default();
    format!("{size:>10}  {:<16}  {key}", date(modified))
}

fn date(iso: &str) -> String {
    format::parse_iso8601(iso)
        .map(format::date)
        .unwrap_or_default()
}

// ----- local files ----------------------------------------------------------------------

/// The files a path or pattern names on this computer. A failure is a message
/// to report before going on with the other paths.
fn find_files(cli: &Cli, text: &str, syntax: Syntax) -> Result<Vec<UploadSource>, Error> {
    let cwd = &cli.cx.cwd;
    if syntax == Syntax::Glob {
        let path = cwd.join(text);
        if path.exists() {
            let found = local::collect_uploads(&[path], "");
            if found.is_empty() {
                return Err(Error::Failed(format!("{text} has no files in it")));
            }
            return Ok(found);
        }
    }
    let pattern = Pattern::local(text, syntax).map_err(Error::Usage)?;
    if pattern.is_path() {
        return Err(Error::Failed(format!(
            "There's no file or folder at {text}"
        )));
    }
    // Rebuilt from its parts, so paths found below it use the system's separator.
    let base: PathBuf = Path::new(&pattern.base).components().collect();
    let found = local::collect_matching(&cwd.join(base), &|relative| {
        pattern.matches(&format!("{}{relative}", pattern.base))
    });
    if found.is_empty() {
        return Err(Error::Failed(format!("No files match {text}")));
    }
    Ok(found)
}

/// `--to photos/2024` as the start of a key: `photos/2024/`.
fn key_folder(to: Option<&str>) -> String {
    let folder = to.unwrap_or_default().trim_start_matches('/');
    if folder.is_empty() || folder.ends_with('/') {
        folder.to_string()
    } else {
        format!("{folder}/")
    }
}

// ----- transfers ------------------------------------------------------------------------

struct Job {
    key: String,
    path: PathBuf,
    size: u64,
    /// The source and destination, as shown.
    label: String,
}

#[derive(Clone, Copy)]
enum Direction {
    Upload,
    Download,
}

/// Uploads or downloads a few files at a time, showing each as it finishes.
/// Returns which ones worked.
fn transfer(
    cli: &mut Cli,
    client: &S3Client,
    bucket: &str,
    jobs: &[Job],
    direction: Direction,
) -> Vec<bool> {
    let total = format::size(jobs.iter().map(|j| j.size).sum());
    let next = AtomicUsize::new(0);
    let moved: Vec<AtomicU64> = jobs.iter().map(|_| AtomicU64::new(0)).collect();
    let never = AtomicBool::new(false);
    let mut worked = vec![false; jobs.len()];
    let (sender, receiver) = mpsc::channel();

    thread::scope(|scope| {
        for _ in 0..WORKERS.min(jobs.len()) {
            let sender = sender.clone();
            let (next, moved, never) = (&next, &moved, &never);
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(index) else {
                    return;
                };
                let progress = |done: u64| moved[index].store(done, Ordering::Relaxed);
                let result = match direction {
                    Direction::Upload => {
                        client.upload_file(bucket, &job.key, &job.path, &progress, never)
                    }
                    Direction::Download => {
                        client.download_file(bucket, &job.key, &job.path, &progress, never)
                    }
                };
                if sender.send((index, result)).is_err() {
                    return;
                }
            });
        }
        drop(sender);

        let mut finished = 0u64;
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok((index, Ok(()))) => {
                    finished += 1;
                    worked[index] = true;
                    cli.say(&jobs[index].label);
                }
                Ok((index, Err(error))) => {
                    finished += 1;
                    cli.warn(format!("{}: {error}", jobs[index].label));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let bytes = moved.iter().map(|m| m.load(Ordering::Relaxed)).sum();
            cli.status(&format!(
                "{} of {}, {} of {total}",
                thousands(finished),
                plural(jobs.len(), "file"),
                format::size(bytes)
            ));
        }
        cli.clear_status();
    });
    worked
}

/// `Uploaded 3 files (1.2 MB) to media.`, or how many of them when some failed.
fn summary(verb: &str, done: &[&Job], total: usize, place: &str) -> String {
    if done.len() == total {
        let bytes = done.iter().map(|job| job.size).sum();
        format!(
            "{verb} {} ({}) {place}.",
            plural(total, "file"),
            format::size(bytes)
        )
    } else {
        format!(
            "{verb} {} of {} {place}.",
            thousands(done.len() as u64),
            plural(total, "file")
        )
    }
}

/// Runs `work` on its own thread, keeping `status` on screen until it's done.
fn wait<T: Send>(cli: &mut Cli, work: impl FnOnce() -> T + Send, status: impl Fn() -> String) -> T {
    if !cli.cx.progress {
        return work();
    }
    thread::scope(|scope| {
        let handle = scope.spawn(work);
        while !handle.is_finished() {
            cli.status(&status());
            thread::sleep(Duration::from_millis(100));
        }
        cli.clear_status();
        handle
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

// ----- the cache ----------------------------------------------------------------------------

/// Where a bucket's objects are cached: each custom domain that serves it.
struct Cache<'a> {
    api: &'a Api,
    sites: Vec<Site>,
}

struct Site {
    /// The public base URL, such as `https://cdn.example.com`.
    base: String,
    host: String,
    zone: String,
}

impl Site {
    fn url(&self, key: &str) -> String {
        format::public_url(&self.base, key)
    }

    /// A folder as the purge API takes it: `cdn.example.com/photos/`.
    fn prefix(&self, folder: &str) -> String {
        let url = self.url(folder);
        match url.split_once("://") {
            Some((_, rest)) => rest.to_string(),
            None => url,
        }
    }
}

impl Cache<'_> {
    /// `cdn.example.com and assets.example.com`
    fn hosts(&self) -> String {
        let hosts: Vec<&str> = self.sites.iter().map(|s| s.host.as_str()).collect();
        match hosts.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
            _ => hosts.concat(),
        }
    }
}

fn cache<'a>(target: &'a Target, bucket: &str) -> Result<Cache<'a>, Error> {
    let api = target.api.as_ref().ok_or_else(|| {
        "Purging the cache needs a connection set up with an API token. Edit the connection in \
         Fathom to use one, or set CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN."
            .to_string()
    })?;
    let attached = api.custom_domains(bucket, &target.jurisdiction);
    let sites = match (target.domains.get(bucket), &attached) {
        // A domain set by hand in the app is the one people use.
        (Some(base), _) => vec![site(
            api,
            bucket,
            base,
            attached.as_deref().unwrap_or_default(),
        )?],
        (None, Ok(attached)) => attached
            .iter()
            .map(|(base, _)| site(api, bucket, base, attached))
            .collect::<Result<_, _>>()?,
        (None, Err(error)) => {
            return Err(Error::Failed(format!(
                "Could not look up the custom domains of {bucket}: {error}"
            )))
        }
    };
    if sites.is_empty() {
        let message = match api.public_domain(bucket, &target.jurisdiction) {
            Ok(Some(base)) => r2_dev_only(bucket, host_of(&base)),
            _ => format!("{bucket} isn't public on a custom domain, so there's no cache to purge"),
        };
        return Err(Error::Failed(message));
    }
    Ok(Cache { api, sites })
}

/// One domain of a bucket, with its zone: the one Cloudflare gave for the
/// attached domain, or else the one found by name.
fn site(api: &Api, bucket: &str, base: &str, attached: &[(String, String)]) -> Result<Site, Error> {
    let host = host_of(base).to_string();
    if host.ends_with(".r2.dev") {
        return Err(Error::Failed(r2_dev_only(bucket, &host)));
    }
    let known = attached
        .iter()
        .find(|(other, zone)| host_of(other).eq_ignore_ascii_case(&host) && !zone.is_empty());
    let zone = match known {
        Some((_, zone)) => zone.clone(),
        None => api.zone_of(&host)?,
    };
    Ok(Site {
        base: base.to_string(),
        host,
        zone,
    })
}

fn r2_dev_only(bucket: &str, host: &str) -> String {
    format!("{bucket} is only public at {host}. Only the cache of a custom domain can be purged.")
}

/// `https://cdn.example.com/assets` -> `cdn.example.com`
fn host_of(base: &str) -> &str {
    let after_scheme = base.split_once("://").map_or(base, |(_, rest)| rest);
    after_scheme.split('/').next().unwrap_or_default()
}

/// `3 URLs and 1 folder`
fn purged(files: usize, folders: usize) -> String {
    match (files, folders) {
        (_, 0) => plural(files, "URL"),
        (0, _) => plural(folders, "folder"),
        _ => format!("{} and {}", plural(files, "URL"), plural(folders, "folder")),
    }
}

/// The URLs of objects, then of folders, on every domain of the cache.
fn purge_urls(cache: &Cache, files: &[String], folders: &[String]) -> Vec<String> {
    let mut urls = Vec::new();
    for site in &cache.sites {
        urls.extend(files.iter().chain(folders).map(|key| site.url(key)));
    }
    urls
}

/// Purges objects, and folders with everything in them, from the cache of
/// every domain of the bucket.
fn purge_keys(
    cli: &mut Cli,
    cache: &Cache,
    files: &[String],
    folders: &[String],
) -> Result<(), Error> {
    let per_site = files.len() + folders.len();
    let total = thousands((per_site * cache.sites.len()) as u64);
    for (index, site) in cache.sites.iter().enumerate() {
        let urls: Vec<String> = files.iter().map(|key| site.url(key)).collect();
        let prefixes: Vec<String> = folders.iter().map(|folder| site.prefix(folder)).collect();
        let done = AtomicUsize::new(0);
        wait(
            cli,
            || {
                cache.api.purge(&site.zone, &urls, &prefixes, &|finished| {
                    done.store(finished, Ordering::Relaxed)
                })
            },
            || {
                let finished = index * per_site + done.load(Ordering::Relaxed);
                format!("Purging {} of {total}", thousands(finished as u64))
            },
        )
        .map_err(|e| format!("Could not purge the cache of {}: {e}", site.host))?;
    }
    for url in purge_urls(cache, files, folders) {
        cli.say(url);
    }
    cli.say(format!(
        "Purged {} from the cache of {}.",
        purged(files.len(), folders.len()),
        cache.hosts()
    ));
    Ok(())
}
