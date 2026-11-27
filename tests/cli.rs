//! End-to-end tests of the command line tool against the in-memory server in
//! `common`: picking objects with paths, globs and regexes, moving files,
//! deleting, and purging the cache.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::{zone_id, MockS3, ACCESS_KEY, API_TOKEN, SECRET_KEY, TOKEN_ID};
use fathom::cli::Context;
use fathom::config::{Connection, Credentials, Store, Vault};

#[derive(Default)]
struct MemoryVault(Arc<Mutex<HashMap<String, String>>>);

impl Vault for MemoryVault {
    fn get(&self, id: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().get(id).cloned())
    }
    fn set(&self, id: &str, value: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(id.into(), value.into());
        Ok(())
    }
    fn delete(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn lines(&self) -> Vec<&str> {
        self.out.lines().collect()
    }
}

/// A mock server, settings with a connection to it, and a folder to work in.
struct Setup {
    server: MockS3,
    store: Store,
    dir: PathBuf,
    env: HashMap<String, String>,
    interactive: bool,
    answer: String,
    progress: bool,
    /// Whether commands mention newer releases, as in a terminal.
    updates: bool,
}

impl Setup {
    fn new(name: &str) -> Self {
        let server = MockS3::start();
        let dir = std::env::temp_dir().join(format!("fth-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("work")).unwrap();
        let store = Store::load_with(dir.join("settings"), Box::new(MemoryVault::default()));
        let env = HashMap::from([(
            "FATHOM_CLOUDFLARE_API".to_string(),
            format!("{}/client/v4", server.endpoint),
        )]);
        let setup = Self {
            server,
            store,
            dir,
            env,
            interactive: false,
            answer: String::new(),
            progress: false,
            updates: false,
        };
        setup.save("main", "Main", true, &[]);
        setup
    }

    /// Saves a connection to the server, set up with the API token or with S3 keys.
    fn save(&self, id: &str, name: &str, token: bool, domains: &[(&str, &str)]) {
        let mut credentials = Credentials {
            account_id: "test-account".into(),
            endpoint: self.server.endpoint.clone(),
            access_key_id: ACCESS_KEY.into(),
            secret_access_key: SECRET_KEY.into(),
            ..Default::default()
        };
        if token {
            credentials.api_token = API_TOKEN.into();
            credentials.access_key_id = TOKEN_ID.into();
            credentials.secret_access_key = fathom::sigv4::sha256_hex(API_TOKEN.as_bytes());
        }
        self.store.save_credentials(id, &credentials).unwrap();
        let mut connection = Connection::default();
        connection.id = id.into();
        connection.name = name.into();
        connection.custom_endpoint = true;
        connection.uses_token = token;
        for (bucket, domain) in domains {
            connection
                .domains
                .insert(bucket.to_string(), domain.to_string());
        }
        self.store
            .update(|config| {
                config.connections.retain(|c| c.id != id);
                config.connections.push(connection);
                config.active.get_or_insert_with(|| id.to_string());
            })
            .unwrap();
    }

    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    fn write(&self, path: &str, data: &[u8]) {
        let path = self.work().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn run(&self, arguments: &[&str]) -> Run {
        let arguments: Vec<String> = arguments.iter().map(|a| a.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = self.answer.as_bytes();
        let env = |name: &str| self.env.get(name).cloned();
        let mut cx = Context {
            store: &self.store,
            env: &env,
            cwd: self.work(),
            out: &mut out,
            err: &mut err,
            input: &mut input,
            interactive: self.interactive,
            progress: self.progress,
            updates: self.updates.then(|| self.dir.join("settings")),
        };
        let code = fathom::cli::run(&arguments, &mut cx);
        // Local paths are shown with the system's separator.
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap().replace('\\', "/");
        Run {
            code,
            out: text(out),
            err: text(err),
        }
    }

    /// The (zone ID, URLs, prefixes) of each purge since the last call.
    fn purges(&self, seen: &mut usize) -> Vec<(String, Vec<String>, Vec<String>)> {
        let all = self.server.purges();
        let new = all[*seen..]
            .iter()
            .map(|(zone, body)| {
                let list = |field: &str| -> Vec<String> {
                    body.get(field)
                        .and_then(|v| v.as_array())
                        .map(|items| {
                            items
                                .iter()
                                .map(|i| i.as_str().unwrap().to_string())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                (zone.clone(), list("files"), list("prefixes"))
            })
            .collect();
        *seen = all.len();
        new
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn seed(server: &MockS3) {
    for key in [
        "readme.txt",
        "photos/cover.png",
        "photos/2023/old.png",
        "photos/2024/reef.png",
        "photos/2024/kelp.jpg",
        "photos/2024-extra/x.png",
        "logs/2023-12/c.log",
        "logs/2024-01/a.log",
        "logs/2024-02/b.log",
        "notes/what?.txt",
    ] {
        server.put("media", key, key.as_bytes(), "text/plain");
    }
    server.create_bucket("backups");
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                stack.push(entry.path());
            } else {
                let relative = entry.path().strip_prefix(dir).unwrap().to_path_buf();
                found.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn lists_buckets_folders_and_what_patterns_pick() {
    let setup = Setup::new("ls");
    seed(&setup.server);

    assert_eq!(setup.run(&["ls"]).lines(), ["backups", "media"]);
    assert_eq!(
        setup.run(&["ls", "media"]).lines(),
        ["logs/", "notes/", "photos/", "readme.txt"]
    );
    assert_eq!(
        setup.run(&["ls", "media", "photos/*.png"]).lines(),
        ["photos/cover.png"]
    );
    assert_eq!(
        setup.run(&["ls", "media", "photos/**/*.png"]).lines(),
        [
            "photos/2023/old.png",
            "photos/2024-extra/x.png",
            "photos/2024/reef.png",
            "photos/cover.png"
        ]
    );
    // A folder named in full, without the folder that only starts the same.
    assert_eq!(
        setup.run(&["ls", "media", "photos/2024"]).lines(),
        ["photos/2024/kelp.jpg", "photos/2024/reef.png"]
    );
    // A partial path.
    assert_eq!(
        setup.run(&["ls", "media", "logs/2024-**"]).lines(),
        ["logs/2024-01/a.log", "logs/2024-02/b.log"]
    );
    assert_eq!(
        setup.run(&["ls", "media", "**/*.{jpg,txt}"]).lines(),
        ["notes/what?.txt", "photos/2024/kelp.jpg", "readme.txt"]
    );
    assert_eq!(
        setup.run(&["ls", "media", "notes/what\\?.txt"]).lines(),
        ["notes/what?.txt"]
    );

    // An anchored regex lists only from its fixed start.
    let before = setup.server.request_log().len();
    let regex = setup.run(&["ls", "media", "--regex", "^logs/2024-0[12]/"]);
    assert_eq!(regex.lines(), ["logs/2024-01/a.log", "logs/2024-02/b.log"]);
    let requests = setup.server.request_log()[before..].to_vec();
    assert!(
        requests.iter().all(|r| r.contains("prefix=logs%2F2024-0")),
        "{requests:?}"
    );
    assert_eq!(
        setup.run(&["ls", "media", "-E", r"\.log$"]).lines(),
        [
            "logs/2023-12/c.log",
            "logs/2024-01/a.log",
            "logs/2024-02/b.log"
        ]
    );

    let long = setup.run(&["ls", "media", "readme.txt", "--long"]);
    assert!(long.out.contains("10 B"), "{}", long.out);
    assert!(
        long.out.trim_end().ends_with("  readme.txt"),
        "{}",
        long.out
    );

    // Each pattern that picks nothing is reported, and the exit code says so.
    let partly = setup.run(&["ls", "media", "photos/*.gif", "readme.txt"]);
    assert_eq!(partly.code, 1);
    assert_eq!(partly.lines(), ["readme.txt"]);
    assert!(
        partly.err.contains("Nothing in media matches photos/*.gif"),
        "{}",
        partly.err
    );
}

#[test]
fn uploads_files_folders_and_patterns() {
    let setup = Setup::new("upload");
    for bucket in ["media", "site", "styles", "pages", "dry"] {
        setup.server.create_bucket(bucket);
    }
    setup.write("photo.jpg", b"jpeg");
    setup.write("dist/index.html", b"<h1>hi</h1>");
    setup.write("dist/css/site.css", b"body{}");
    setup.write("dist/css/print.css", b"@media print{}");
    setup.write("other/site.css", b"p{}");

    let one = setup.run(&["upload", "media", "photo.jpg", "--to", "photos/2024"]);
    assert_eq!(one.code, 0, "{}", one.err);
    assert_eq!(
        one.lines(),
        [
            "photo.jpg -> media/photos/2024/photo.jpg",
            "Uploaded 1 file (4 B) to media."
        ]
    );
    let object = setup.server.get("media", "photos/2024/photo.jpg").unwrap();
    assert_eq!(object.data, b"jpeg");
    assert_eq!(object.content_type, "image/jpeg");

    // A folder keeps its name; a pattern keeps what follows its fixed folders.
    assert_eq!(setup.run(&["upload", "media", "dist"]).code, 0);
    assert_eq!(
        setup.server.keys("media")[..3],
        ["dist/css/print.css", "dist/css/site.css", "dist/index.html"]
    );
    assert_eq!(setup.run(&["upload", "site", "dist/**"]).code, 0);
    assert_eq!(
        setup.server.keys("site"),
        ["css/print.css", "css/site.css", "index.html"]
    );
    assert_eq!(setup.run(&["upload", "styles", "dist/css/*.css"]).code, 0);
    assert_eq!(setup.server.keys("styles"), ["print.css", "site.css"]);
    let regex = setup.run(&["upload", "pages", "-E", r"^dist/.*\.html$", "--to", "www/"]);
    assert_eq!(regex.code, 0, "{}", regex.err);
    assert_eq!(setup.server.keys("pages"), ["www/index.html"]);

    let dry = setup.run(&["upload", "dry", "dist/**", "--dry-run"]);
    assert_eq!(dry.code, 0);
    assert!(
        dry.out.contains("dist/index.html -> dry/index.html"),
        "{}",
        dry.out
    );
    assert!(
        dry.out.contains("Would upload 3 files (31 B) to dry."),
        "{}",
        dry.out
    );
    assert!(setup.server.keys("dry").is_empty());

    // What can be found is uploaded; what can't is reported.
    let partly = setup.run(&["upload", "dry", "missing.txt", "photo.jpg", "none/*.x"]);
    assert_eq!(partly.code, 1);
    assert!(
        partly
            .err
            .contains("There's no file or folder at missing.txt"),
        "{}",
        partly.err
    );
    assert!(
        partly.err.contains("No files match none/*.x"),
        "{}",
        partly.err
    );
    assert_eq!(setup.server.keys("dry"), ["photo.jpg"]);

    // Two files for one key stop the upload before anything is sent.
    let clash = setup.run(&["upload", "dry", "dist/css/site.css", "other/site.css"]);
    assert_eq!(clash.code, 1);
    assert!(
        clash.err.contains("would both be uploaded as site.css"),
        "{}",
        clash.err
    );
    assert_eq!(setup.server.keys("dry"), ["photo.jpg"]);
}

#[test]
fn downloads_what_patterns_pick() {
    let setup = Setup::new("download");
    seed(&setup.server);
    setup
        .server
        .put("media", "photos/empty/", b"", "application/x-directory");
    setup.server.put("media", "a/same.txt", b"a", "text/plain");
    setup.server.put("media", "b/same.txt", b"b", "text/plain");
    let work = setup.work();

    let folder = setup.run(&["download", "media", "photos/2024", "--to", "out"]);
    assert_eq!(folder.code, 0, "{}", folder.err);
    assert_eq!(
        files_in(&work.join("out")),
        ["2024/kelp.jpg", "2024/reef.png"]
    );
    assert_eq!(
        fs::read(work.join("out/2024/reef.png")).unwrap(),
        b"photos/2024/reef.png"
    );
    assert!(
        folder
            .out
            .contains("media/photos/2024/reef.png -> out/2024/reef.png"),
        "{}",
        folder.out
    );
    assert!(
        folder.out.contains("Downloaded 2 files (40 B) to out."),
        "{}",
        folder.out
    );

    assert_eq!(
        setup
            .run(&["download", "media", "photos/**/*.png", "--to", "png"])
            .code,
        0
    );
    assert_eq!(
        files_in(&work.join("png")),
        [
            "2023/old.png",
            "2024-extra/x.png",
            "2024/reef.png",
            "cover.png"
        ]
    );
    assert_eq!(
        setup
            .run(&["download", "media", "-E", "^logs/2024-0", "--to", "logs"])
            .code,
        0
    );
    assert_eq!(
        files_in(&work.join("logs")),
        ["2024-01/a.log", "2024-02/b.log"]
    );

    // Files already there are replaced, and empty folders are kept.
    fs::write(work.join("readme.txt"), b"old").unwrap();
    assert_eq!(setup.run(&["download", "media", "readme.txt"]).code, 0);
    assert_eq!(fs::read(work.join("readme.txt")).unwrap(), b"readme.txt");
    assert_eq!(
        setup
            .run(&["download", "media", "photos/", "--to", "all"])
            .code,
        0
    );
    assert!(work.join("all/photos/empty").is_dir());
    assert_eq!(files_in(&work.join("all")).len(), 5);

    let dry = setup.run(&["download", "media", "logs/", "--to", "dry", "-n"]);
    assert!(
        dry.out.contains("Would download 3 files (54 B) to dry."),
        "{}",
        dry.out
    );
    assert!(!work.join("dry").exists());

    let clash = setup.run(&[
        "download",
        "media",
        "a/same.txt",
        "b/same.txt",
        "--to",
        "clash",
    ]);
    assert_eq!(clash.code, 1);
    assert!(
        clash
            .err
            .contains("a/same.txt and b/same.txt would both be saved"),
        "{}",
        clash.err
    );
    assert!(!work.join("clash").exists());

    let missing = setup.run(&["download", "media", "nope/"]);
    assert_eq!(missing.code, 1);
    assert!(
        missing.err.contains("Nothing in media matches nope/"),
        "{}",
        missing.err
    );
}

#[test]
fn progress_stays_out_of_the_results() {
    let mut setup = Setup::new("progress");
    setup.progress = true;
    setup.server.create_bucket("media");
    for i in 0..6 {
        setup.write(&format!("batch/{i}.bin"), &vec![7; 200_000]);
    }

    let upload = setup.run(&["upload", "media", "batch"]);
    assert_eq!(upload.code, 0, "{}", upload.err);
    // Results stay one per line, ready to pipe; the progress line, redrawn
    // in place, goes to the error stream and is cleared at the end.
    assert_eq!(upload.lines().len(), 7);
    assert!(!upload.out.contains('\r'), "{:?}", upload.out);
    assert!(upload.err.contains(" of 6 files, "), "{:?}", upload.err);
    assert!(upload.err.ends_with('\r'), "{:?}", upload.err);

    let delete = setup.run(&["delete", "media", "batch/", "--yes"]);
    assert_eq!(delete.lines(), ["Deleted 6 objects from media."]);
    assert!(setup.server.keys("media").is_empty());
}

#[test]
fn deletes_only_what_was_confirmed() {
    let mut setup = Setup::new("delete");
    seed(&setup.server);

    // Nobody to ask: nothing happens without --yes.
    let refused = setup.run(&["delete", "media", "logs/2024-**"]);
    assert_eq!(refused.code, 1);
    assert!(
        refused
            .err
            .contains("Not deleting 2 objects without asking"),
        "{}",
        refused.err
    );
    let dry = setup.run(&["delete", "media", "logs/2024-**", "--dry-run"]);
    assert_eq!(
        dry.lines(),
        [
            "logs/2024-01/a.log",
            "logs/2024-02/b.log",
            "Would delete 2 objects from media."
        ]
    );
    assert_eq!(setup.server.keys("media").len(), 10);

    setup.interactive = true;
    setup.answer = "n\n".into();
    let declined = setup.run(&["delete", "media", "logs/2024-**"]);
    assert_eq!(declined.code, 0);
    assert!(
        declined.out.contains("Nothing was deleted."),
        "{}",
        declined.out
    );
    assert!(
        declined.err.contains("Delete 2 objects from media? [y/N]"),
        "{}",
        declined.err
    );
    assert_eq!(setup.server.keys("media").len(), 10);

    setup.answer = "y\n".into();
    let confirmed = setup.run(&["delete", "media", "logs/2024-**"]);
    assert_eq!(confirmed.code, 0, "{}", confirmed.err);
    assert!(
        confirmed.out.contains("Deleted 2 objects from media."),
        "{}",
        confirmed.out
    );
    assert!(setup.server.get("media", "logs/2024-01/a.log").is_none());
    assert!(setup.server.get("media", "logs/2023-12/c.log").is_some());

    setup.interactive = false;
    assert_eq!(
        setup
            .run(&["delete", "media", "-E", r"\.png$", "--yes"])
            .code,
        0
    );
    assert_eq!(
        setup.server.keys("media"),
        [
            "logs/2023-12/c.log",
            "notes/what?.txt",
            "photos/2024/kelp.jpg",
            "readme.txt"
        ]
    );
}

#[test]
fn purges_urls_and_folders_from_the_cache() {
    let setup = Setup::new("purge");
    seed(&setup.server);
    setup
        .server
        .add_custom_domain("media", "cdn.example.com", true);
    setup
        .server
        .put("media", "notes/été & more.txt", b"x", "text/plain");
    for i in 0..150 {
        setup
            .server
            .put("media", &format!("many/{i:03}.txt"), b"x", "text/plain");
    }
    let zone = zone_id("example.com");
    let mut seen = 0;

    // A file named in full is purged even when it's gone; a folder as a whole.
    let named = setup.run(&[
        "purge",
        "media",
        "photos/2024/reef.png",
        "gone.png",
        "photos/2023/",
    ]);
    assert_eq!(named.code, 0, "{}", named.err);
    assert_eq!(
        setup.purges(&mut seen),
        [
            (
                zone.clone(),
                vec![
                    "https://cdn.example.com/gone.png".to_string(),
                    "https://cdn.example.com/photos/2024/reef.png".to_string()
                ],
                vec![]
            ),
            (
                zone.clone(),
                vec![],
                vec!["cdn.example.com/photos/2023/".to_string()]
            )
        ]
    );
    assert!(
        named
            .out
            .contains("Purged 2 URLs and 1 folder from the cache of cdn.example.com."),
        "{}",
        named.out
    );

    // A path that turns out to be a folder purges both.
    setup.run(&["purge", "media", "photos"]);
    assert_eq!(
        setup.purges(&mut seen),
        [
            (
                zone.clone(),
                vec!["https://cdn.example.com/photos".to_string()],
                vec![]
            ),
            (
                zone.clone(),
                vec![],
                vec!["cdn.example.com/photos/".to_string()]
            )
        ]
    );

    // Wildcards purge what they pick, as URLs written the way browsers send them.
    setup.run(&["purge", "media", "notes/*.txt", "photos/*.png"]);
    assert_eq!(
        setup.purges(&mut seen)[0].1,
        [
            "https://cdn.example.com/notes/what%3F.txt",
            "https://cdn.example.com/notes/%C3%A9t%C3%A9%20%26%20more.txt",
            "https://cdn.example.com/photos/cover.png"
        ]
    );

    // Long lists go in batches the API accepts.
    let many = setup.run(&["purge", "media", "many/*"]);
    assert_eq!(many.code, 0, "{}", many.err);
    let batches: Vec<usize> = setup
        .purges(&mut seen)
        .iter()
        .map(|(_, files, _)| files.len())
        .collect();
    assert_eq!(batches, [100, 50]);

    let dry = setup.run(&["purge", "media", "photos/", "readme.txt", "--dry-run"]);
    assert_eq!(
        dry.lines(),
        [
            "https://cdn.example.com/readme.txt",
            "https://cdn.example.com/photos/",
            "Would purge 1 URL and 1 folder from the cache of cdn.example.com."
        ]
    );
    assert!(setup.purges(&mut seen).is_empty());

    let unmatched = setup.run(&["purge", "media", "photos/*.gif"]);
    assert_eq!(unmatched.code, 1);
    assert!(
        unmatched
            .err
            .contains("Nothing in media matches photos/*.gif"),
        "{}",
        unmatched.err
    );
}

#[test]
fn purges_every_domain_that_serves_the_bucket() {
    let setup = Setup::new("purge-domains");
    setup.server.put("media", "a.png", b"a", "image/png");
    setup
        .server
        .add_custom_domain("media", "cdn.example.com", true);
    setup
        .server
        .add_custom_domain("media", "img.other.net", true);
    setup
        .server
        .add_custom_domain("media", "pending.example.com", false);
    let strings = |items: &[&str]| -> Vec<String> { items.iter().map(|s| s.to_string()).collect() };
    let mut seen = 0;

    let run = setup.run(&["purge", "media", "a.png", "photos/"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(
        setup.purges(&mut seen),
        [
            (
                zone_id("example.com"),
                strings(&["https://cdn.example.com/a.png"]),
                vec![]
            ),
            (
                zone_id("example.com"),
                vec![],
                strings(&["cdn.example.com/photos/"])
            ),
            (
                zone_id("other.net"),
                strings(&["https://img.other.net/a.png"]),
                vec![]
            ),
            (
                zone_id("other.net"),
                vec![],
                strings(&["img.other.net/photos/"])
            ),
        ]
    );
    assert!(
        run.out.ends_with(
            "Purged 1 URL and 1 folder from the cache of cdn.example.com and img.other.net.\n"
        ),
        "{}",
        run.out
    );

    // A folder named in full needs no listing.
    let before = setup.server.request_log().len();
    assert_eq!(setup.run(&["purge", "media", "photos/"]).code, 0);
    let requests = setup.server.request_log()[before..].to_vec();
    assert!(
        !requests.iter().any(|r| r.starts_with("GET /media")),
        "{requests:?}"
    );
}

#[test]
fn purging_explains_what_it_needs() {
    let setup = Setup::new("purge-needs");
    for bucket in ["private", "dev", "denied", "site"] {
        setup.server.put(bucket, "index.html", b"<p>", "text/html");
    }
    setup.server.set_r2_dev("dev", "pub-123.r2.dev", true);
    setup
        .server
        .add_custom_domain("denied", "cdn.example.net", true);

    let private = setup.run(&["purge", "private", "index.html"]);
    assert_eq!(private.code, 1);
    assert!(
        private
            .err
            .contains("private isn't public on a custom domain"),
        "{}",
        private.err
    );
    let dev = setup.run(&["purge", "dev", "index.html"]);
    assert!(
        dev.err
            .contains("Only the cache of a custom domain can be purged"),
        "{}",
        dev.err
    );

    // A domain set by hand is found among the account's zones.
    setup.save(
        "main",
        "Main",
        true,
        &[("site", "https://www.example.org/assets")],
    );
    let org = setup.server.add_zone("example.org");
    let by_hand = setup.run(&["purge", "site", "index.html"]);
    assert_eq!(by_hand.code, 0, "{}", by_hand.err);
    assert_eq!(
        setup.server.purges(),
        [(
            org,
            serde_json::json!({"files": ["https://www.example.org/assets/index.html"]})
        )]
    );

    setup.server.deny_purge();
    let denied = setup.run(&["purge", "denied", "index.html"]);
    assert_eq!(denied.code, 1);
    assert!(
        denied
            .err
            .contains("Purging needs an API token with the Zone > Cache Purge permission"),
        "{}",
        denied.err
    );

    // S3 keys alone can't reach the Cloudflare API.
    setup.save("keys", "Keys only", false, &[]);
    let keys = setup.run(&["purge", "site", "index.html", "-c", "Keys only"]);
    assert!(
        keys.err
            .contains("needs a connection set up with an API token"),
        "{}",
        keys.err
    );
}

#[test]
fn upload_and_delete_can_purge_what_they_change() {
    let setup = Setup::new("then-purge");
    seed(&setup.server);
    setup.server.create_bucket("plain");
    setup
        .server
        .add_custom_domain("media", "cdn.example.com", true);
    setup.write("photo.jpg", b"jpeg");
    let mut seen = 0;

    let upload = setup.run(&["upload", "media", "photo.jpg", "--to", "photos", "--purge"]);
    assert_eq!(upload.code, 0, "{}", upload.err);
    assert_eq!(
        setup.purges(&mut seen)[0].1,
        ["https://cdn.example.com/photos/photo.jpg"]
    );

    let delete = setup.run(&["delete", "media", "photos/2024/", "--yes", "--purge"]);
    assert_eq!(delete.code, 0, "{}", delete.err);
    assert_eq!(
        setup.purges(&mut seen)[0].1,
        [
            "https://cdn.example.com/photos/2024/kelp.jpg",
            "https://cdn.example.com/photos/2024/reef.png"
        ]
    );

    // Without a cache to purge, nothing is changed.
    setup.server.put("plain", "a.txt", b"a", "text/plain");
    let refused = setup.run(&["delete", "plain", "a.txt", "--yes", "--purge"]);
    assert_eq!(refused.code, 1);
    assert!(setup.server.get("plain", "a.txt").is_some());
    let refused = setup.run(&["upload", "plain", "photo.jpg", "--purge"]);
    assert_eq!(refused.code, 1);
    assert!(setup.server.get("plain", "photo.jpg").is_none());
}

#[test]
fn picks_a_connection_by_name_or_id() {
    let setup = Setup::new("connections");
    setup.server.create_bucket("media");
    let other = MockS3::start();
    other.create_bucket("archive");
    let credentials = Credentials {
        account_id: "other-account".into(),
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SECRET_KEY.into(),
        endpoint: other.endpoint.clone(),
        ..Default::default()
    };
    setup
        .store
        .save_credentials("second", &credentials)
        .unwrap();
    setup
        .store
        .update(|config| {
            let mut connection = Connection::default();
            connection.id = "second".into();
            connection.name = "Backup".into();
            config.connections.push(connection);
        })
        .unwrap();

    assert_eq!(setup.run(&["ls"]).lines(), ["media"]);
    assert_eq!(
        setup.run(&["ls", "--connection", "backup"]).lines(),
        ["archive"]
    );
    assert_eq!(setup.run(&["-c", "second", "ls"]).lines(), ["archive"]);
    let unknown = setup.run(&["ls", "-c", "nope"]);
    assert_eq!(unknown.code, 1);
    assert!(
        unknown.err.contains("No connection is named nope"),
        "{}",
        unknown.err
    );

    assert_eq!(
        setup.run(&["connections"]).lines(),
        ["Main    main  (default)", "Backup  second"]
    );
}

#[test]
fn connects_with_environment_variables() {
    let mut setup = Setup::new("env");
    setup.server.create_bucket("media");
    setup
        .store
        .update(|config| config.connections.clear())
        .unwrap();

    let none = setup.run(&["ls"]);
    assert_eq!(none.code, 1);
    assert!(
        none.err
            .contains("set CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN"),
        "{}",
        none.err
    );

    setup
        .env
        .insert("CLOUDFLARE_ACCOUNT_ID".into(), "test-account".into());
    setup
        .env
        .insert("CLOUDFLARE_API_TOKEN".into(), API_TOKEN.into());
    setup
        .env
        .insert("FATHOM_ENDPOINT".into(), setup.server.endpoint.clone());
    let listed = setup.run(&["ls"]);
    assert_eq!(listed.code, 0, "{}", listed.err);
    assert_eq!(listed.lines(), ["media"]);

    setup
        .env
        .insert("CLOUDFLARE_API_TOKEN".into(), "not-the-token".into());
    let rejected = setup.run(&["ls"]);
    assert_eq!(rejected.code, 1);
    assert!(
        rejected.err.contains("didn't accept this API token"),
        "{}",
        rejected.err
    );
    assert!(!rejected.err.contains("not-the-token"), "{}", rejected.err);
}

#[test]
fn tells_about_newer_releases() {
    let mut setup = Setup::new("updates");
    setup.server.create_bucket("media");
    let endpoint = setup.server.endpoint.clone();
    setup.env.insert("FATHOM_UPDATE_API".into(), endpoint);
    let version = env!("CARGO_PKG_VERSION");

    setup.server.publish("0.0.0", &[]);
    assert_eq!(
        setup.run(&["update", "--check"]).lines(),
        [format!("Fathom {version} is the latest version.")]
    );

    setup.server.publish("999.1.0", &[]);
    let check = setup.run(&["update", "--check"]);
    assert_eq!(
        check.lines(),
        [
            format!("Fathom 999.1.0 is available. You have {version}."),
            "Run fth update to install it.".to_string()
        ]
    );
    // A copy that wasn't installed from a release points to the download.
    let install = setup.run(&["update"]);
    assert_eq!(install.code, 1);
    assert!(
        install.err.contains(
            "Download the new version from https://github.com/dend/fathom/releases/tag/999.1.0"
        ),
        "{}",
        install.err
    );

    // In a terminal, other commands mention it after their own output, and
    // remember the check for a day.
    setup.updates = true;
    let ls = setup.run(&["ls"]);
    assert_eq!(ls.lines(), ["media"]);
    assert!(
        ls.err.ends_with(&format!(
            "Fathom 999.1.0 is available. You have {version}. Run fth update to install it.
"
        )),
        "{:?}",
        ls.err
    );
    setup.server.publish("999.2.0", &[]);
    assert!(setup
        .run(&["ls"])
        .err
        .contains("Fathom 999.1.0 is available"));
    // Asking checks again, and the reminder follows.
    assert_eq!(setup.run(&["update", "--check"]).code, 0);
    assert!(setup
        .run(&["ls"])
        .err
        .contains("Fathom 999.2.0 is available"));
}

#[test]
fn explains_command_line_mistakes() {
    let setup = Setup::new("mistakes");
    setup.server.create_bucket("media");

    for (arguments, message) in [
        (
            &["upload", "media"][..],
            "Name the files or folders to upload after the bucket",
        ),
        (&["download"][..], "Name a bucket, then what to download"),
        (
            &["ls", "media/photos/"][..],
            "Name the bucket, then what's in it, as in: media photos/",
        ),
        (
            &["download", "media", "x", "--purge"][..],
            "download doesn't take --purge",
        ),
        (
            &["ls", "media", "-E", "(open"][..],
            "(open is not a valid pattern",
        ),
        (&["list"][..], "There's no command list"),
    ] {
        let run = setup.run(arguments);
        assert_eq!(run.code, 2, "{arguments:?}");
        assert!(run.err.contains(message), "{arguments:?}: {}", run.err);
        assert!(run.err.contains("Run fth help"), "{}", run.err);
    }

    let help = setup.run(&["help"]);
    assert_eq!(help.code, 0);
    assert!(help.out.contains("Usage: fth <command>"), "{}", help.out);
    let upload = setup.run(&["upload", "--help"]);
    assert!(
        upload.out.starts_with("Usage: fth upload"),
        "{}",
        upload.out
    );
    let version = setup.run(&["--version"]);
    assert_eq!(
        version.out.trim(),
        format!("fth {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn the_program_runs_from_a_shell() {
    let server = MockS3::start();
    server.put("media", "photos/reef.png", b"png", "image/png");
    let settings = std::env::temp_dir().join(format!("fth-binary-{}", std::process::id()));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fth"))
        .args(["ls", "media", "photos/*"])
        .env("FATHOM_CONFIG_DIR", &settings)
        .env(
            "FATHOM_CLOUDFLARE_API",
            format!("{}/client/v4", server.endpoint),
        )
        .env("FATHOM_ENDPOINT", &server.endpoint)
        .env("CLOUDFLARE_ACCOUNT_ID", "test-account")
        .env("CLOUDFLARE_API_TOKEN", API_TOKEN)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(settings);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "photos/reef.png"
    );

    let mistake = std::process::Command::new(env!("CARGO_BIN_EXE_fth"))
        .arg("frobnicate")
        .output()
        .unwrap();
    assert_eq!(mistake.status.code(), Some(2));
}
