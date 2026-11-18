//! End-to-end tests of the S3 client, bulk operations and transfer queue
//! against the in-memory server in `common`.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use common::{MockS3, ACCESS_KEY, API_TOKEN, SECRET_KEY, TOKEN_ID};
use fathom::cloudflare::Api;
use fathom::ops::{self, Move};
use fathom::s3::{Error, S3Client};
use fathom::transfers::{Kind, Request, Status, Transfers};

fn client(server: &MockS3) -> S3Client {
    S3Client::new(&server.endpoint, ACCESS_KEY, SECRET_KEY).unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fathom-it-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

static NO_CANCEL: AtomicBool = AtomicBool::new(false);

fn api(server: &MockS3, token: &str) -> Api {
    Api::with_base(
        &format!("{}/client/v4", server.endpoint),
        "test-account",
        token,
    )
}

#[test]
fn api_token_gives_working_s3_keys() {
    let server = MockS3::start();
    server.create_bucket("media");

    let (access_key, secret) = api(&server, API_TOKEN).s3_keys().unwrap();
    assert_eq!(access_key, TOKEN_ID);
    assert_eq!(secret, fathom::sigv4::sha256_hex(API_TOKEN.as_bytes()));

    let buckets = S3Client::new(&server.endpoint, &access_key, &secret)
        .unwrap()
        .list_buckets()
        .unwrap();
    assert_eq!(buckets.len(), 1);
}

#[test]
fn rejected_api_token_is_reported() {
    let server = MockS3::start();
    let error = api(&server, "not-the-token").s3_keys().unwrap_err();
    assert!(error.contains("didn't accept"), "{error}");
    assert!(!error.contains("not-the-token"), "{error}");
}

#[test]
fn public_domain_comes_from_the_bucket_settings() {
    let server = MockS3::start();
    server.add_custom_domain("media", "cdn.example.com", true);
    server.add_custom_domain("website", "www.example.com", false);
    server.set_r2_dev("website", "pub-1234.r2.dev", true);
    server.set_r2_dev("backups", "pub-5678.r2.dev", false);
    let api = api(&server, API_TOKEN);

    // An active custom domain wins; a pending one falls back to the public r2.dev
    // address; a bucket with neither has no public URL.
    assert_eq!(
        api.public_domain("media", "").unwrap().as_deref(),
        Some("https://cdn.example.com")
    );
    assert_eq!(
        api.public_domain("website", "").unwrap().as_deref(),
        Some("https://pub-1234.r2.dev")
    );
    assert_eq!(api.public_domain("backups", "").unwrap(), None);
}

#[test]
fn lists_buckets() {
    let server = MockS3::start();
    server.create_bucket("media");
    server.create_bucket("backups");

    let names: Vec<String> = client(&server)
        .list_buckets()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["backups", "media"]);
}

#[test]
fn lists_folders_and_awkward_keys() {
    let server = MockS3::start();
    let keys = [
        "readme.txt",
        "photos/2024/a.png",
        "photos/2024/b.png",
        "photos/cover image (1).jpg",
        "photos/été + more & <tags>.txt",
        "photos/100% legit?.txt",
        "docs/",
    ];
    for key in keys {
        server.put("media", key, b"x", "text/plain");
    }
    let client = client(&server);

    let root = client.list("media", "", Some("/"), None).unwrap();
    assert_eq!(root.prefixes, ["docs/", "photos/"]);
    assert_eq!(root.objects.len(), 1);
    assert_eq!(root.objects[0].key, "readme.txt");
    assert_eq!(root.objects[0].size, 1);
    assert!(!root.objects[0].modified.is_empty());
    assert!(!root.objects[0].etag.contains('"'));

    let photos = client.list("media", "photos/", Some("/"), None).unwrap();
    assert_eq!(photos.prefixes, ["photos/2024/"]);
    let names: Vec<&str> = photos.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(
        names,
        [
            "photos/100% legit?.txt",
            "photos/cover image (1).jpg",
            "photos/été + more & <tags>.txt"
        ]
    );

    // Every awkward key must survive a round trip through signing.
    for key in keys {
        assert_eq!(client.head("media", key).unwrap().size, 1, "{key}");
    }
}

#[test]
fn follows_pagination() {
    let server = MockS3::start_on(0, 3);
    for i in 0..10 {
        server.put("media", &format!("logs/{i:02}.log"), b"line", "text/plain");
    }
    let client = client(&server);

    let first = client.list("media", "logs/", Some("/"), None).unwrap();
    assert_eq!(first.objects.len(), 3);
    let token = first.next_token.expect("a continuation token");
    let second = client
        .list("media", "logs/", Some("/"), Some(&token))
        .unwrap();
    assert_eq!(second.objects[0].key, "logs/03.log");

    let all = client.list_all("media", "logs/", &NO_CANCEL).unwrap();
    assert_eq!(all.len(), 10);
    assert_eq!(all.last().unwrap().key, "logs/09.log");
}

#[test]
fn reads_metadata_and_partial_content() {
    let server = MockS3::start();
    server.put(
        "media",
        "notes/hello.md",
        b"# Hello\nworld",
        "text/markdown",
    );
    server.put("media", "empty.txt", b"", "text/plain");
    let client = client(&server);

    let info = client.head("media", "notes/hello.md").unwrap();
    assert_eq!(info.size, 13);
    assert_eq!(info.content_type, "text/markdown");
    assert!(!info.etag.is_empty());

    assert_eq!(
        client.read_head("media", "notes/hello.md", 1024).unwrap(),
        (b"# Hello\nworld".to_vec(), 13)
    );
    assert_eq!(
        client.read_head("media", "notes/hello.md", 7).unwrap(),
        (b"# Hello".to_vec(), 13)
    );
    assert_eq!(
        client.read_head("media", "empty.txt", 1024).unwrap(),
        (Vec::new(), 0)
    );

    assert!(client
        .head("media", "missing.txt")
        .unwrap_err()
        .is_not_found());
}

#[test]
fn reports_service_errors() {
    let server = MockS3::start();
    server.create_bucket("media");

    let wrong_secret = S3Client::new(&server.endpoint, ACCESS_KEY, "not-the-secret").unwrap();
    match wrong_secret.list_buckets().unwrap_err() {
        Error::Api { status, code, .. } => {
            assert_eq!(status, 403);
            assert_eq!(code, "SignatureDoesNotMatch");
        }
        other => panic!("unexpected error: {other}"),
    }

    let err = client(&server)
        .list("nope", "", Some("/"), None)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "The specified bucket does not exist. (NoSuchBucket, HTTP 404)"
    );
}

#[test]
fn uploads_and_downloads_files() {
    let server = MockS3::start();
    server.create_bucket("media");
    let dir = temp_dir("roundtrip");
    let mut client = client(&server);
    client.tuning.multipart_threshold = 100_000;
    client.tuning.min_part_size = 40_000;

    for (name, len) in [
        ("empty.bin", 0),
        ("small.png", 5_000),
        ("large.mp4", 250_001),
    ] {
        let data = patterned(len);
        let source = dir.join(name);
        fs::write(&source, &data).unwrap();

        let reported = AtomicU64::new(0);
        let progress = |done: u64| reported.store(done, Ordering::Relaxed);
        let key = format!("uploads/{name}");
        client
            .upload_file("media", &key, &source, &progress, &NO_CANCEL)
            .unwrap();
        assert_eq!(reported.load(Ordering::Relaxed), len as u64);
        assert_eq!(server.get("media", &key).unwrap().data, data, "{name}");

        let dest = dir.join("down").join(name);
        reported.store(0, Ordering::Relaxed);
        client
            .download_file("media", &key, &dest, &progress, &NO_CANCEL)
            .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data, "{name}");
        assert_eq!(reported.load(Ordering::Relaxed), len as u64);
        assert!(!dir.join("down").join(format!("{name}.part")).exists());
    }

    assert_eq!(
        server
            .get("media", "uploads/small.png")
            .unwrap()
            .content_type,
        "image/png"
    );
    assert_eq!(
        server
            .get("media", "uploads/large.mp4")
            .unwrap()
            .content_type,
        "video/mp4"
    );
    // 250,001 bytes in 40,000-byte parts.
    let parts = server
        .request_log()
        .iter()
        .filter(|line| line.starts_with("PUT /media/uploads/large.mp4?partNumber="))
        .count();
    assert_eq!(parts, 7);
    assert_eq!(server.pending_uploads(), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cancelling_an_upload_aborts_the_multipart_upload() {
    let server = MockS3::start();
    server.create_bucket("media");
    let dir = temp_dir("cancel");
    let source = dir.join("big.bin");
    fs::write(&source, patterned(300_000)).unwrap();

    let mut client = client(&server);
    client.tuning.multipart_threshold = 100_000;
    client.tuning.min_part_size = 50_000;

    // Cancel as soon as the second part starts.
    let cancel = AtomicBool::new(false);
    let progress = |done: u64| {
        if done > 60_000 {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let result = client.upload_file("media", "big.bin", &source, &progress, &cancel);
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(server.get("media", "big.bin").is_none());
    assert_eq!(server.pending_uploads(), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn copies_objects_including_multipart_copies() {
    let server = MockS3::start();
    let data = patterned(100_000);
    server.put("media", "video/clip one.mp4", &data, "video/mp4");
    server.create_bucket("archive");

    let mut client = client(&server);
    client
        .copy(
            "media",
            "video/clip one.mp4",
            "archive",
            "copy.mp4",
            data.len() as u64,
            &NO_CANCEL,
        )
        .unwrap();
    let copy = server.get("archive", "copy.mp4").unwrap();
    assert_eq!(copy.data, data);
    assert_eq!(copy.content_type, "video/mp4");

    client.tuning.single_copy_limit = 30_000;
    client.tuning.copy_part_size = 30_000;
    client
        .copy(
            "media",
            "video/clip one.mp4",
            "media",
            "video/big copy.mp4",
            data.len() as u64,
            &NO_CANCEL,
        )
        .unwrap();
    let copy = server.get("media", "video/big copy.mp4").unwrap();
    assert_eq!(copy.data, data);
    assert_eq!(copy.content_type, "video/mp4");
    assert_eq!(server.pending_uploads(), 0);
}

#[test]
fn renames_folders_and_deletes_recursively() {
    let server = MockS3::start();
    for key in [
        "old/",
        "old/a.txt",
        "old/deep/b.txt",
        "old/deep/er/c.txt",
        "older.txt",
        "keep.txt",
    ] {
        server.put("media", key, key.as_bytes(), "text/plain");
    }
    let client = client(&server);

    let steps = AtomicUsize::new(0);
    let progress = |done: usize, total: usize| {
        assert!(done <= total);
        steps.store(total, Ordering::Relaxed);
    };

    // Rename the folder and one file in a single operation.
    let moves = [
        Move {
            from: "old/".into(),
            to: "new name/".into(),
            folder: true,
            size: 0,
        },
        Move {
            from: "older.txt".into(),
            to: "newer.txt".into(),
            folder: false,
            size: 9,
        },
    ];
    let moved = ops::transfer(
        &client, "media", "media", &moves, true, &NO_CANCEL, &progress,
    )
    .unwrap();
    assert_eq!(moved, 5);
    assert_eq!(steps.load(Ordering::Relaxed), 10);
    assert_eq!(
        server.keys("media"),
        [
            "keep.txt",
            "new name/",
            "new name/a.txt",
            "new name/deep/b.txt",
            "new name/deep/er/c.txt",
            "newer.txt"
        ]
    );
    assert_eq!(
        server.get("media", "new name/deep/b.txt").unwrap().data,
        b"old/deep/b.txt"
    );

    // Copying keeps the source.
    let copies = [Move {
        from: "new name/deep/".into(),
        to: "copy/".into(),
        folder: true,
        size: 0,
    }];
    ops::transfer(
        &client, "media", "media", &copies, false, &NO_CANCEL, &progress,
    )
    .unwrap();
    assert!(server.get("media", "copy/er/c.txt").is_some());
    assert!(server.get("media", "new name/deep/er/c.txt").is_some());

    // A folder cannot be moved into itself.
    let invalid = [Move {
        from: "copy/".into(),
        to: "copy/inner/".into(),
        folder: true,
        size: 0,
    }];
    assert!(
        ops::transfer(&client, "media", "media", &invalid, true, &NO_CANCEL, &progress).is_err()
    );

    let deleted = ops::delete(
        &client,
        "media",
        &["newer.txt".to_string()],
        &["new name/".to_string(), "copy/".to_string()],
        &NO_CANCEL,
        &progress,
    )
    .unwrap();
    assert_eq!(deleted, 7);
    assert_eq!(server.keys("media"), ["keep.txt"]);

    assert!(ops::delete(
        &client,
        "media",
        &[],
        &[String::new()],
        &NO_CANCEL,
        &progress
    )
    .is_err());
}

#[test]
fn presigned_urls_work_without_credentials() {
    let server = MockS3::start();
    server.put("media", "shared/photo 1.png", b"png-bytes", "image/png");

    let url = client(&server).presign_get("media", "shared/photo 1.png", 600);
    let mut response = ureq::get(&url).call().unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.body_mut().read_to_vec().unwrap(), b"png-bytes");

    // Tampering with the URL invalidates it.
    let tampered = url.replace("photo%201.png", "photo%202.png");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    assert_eq!(agent.get(&tampered).call().unwrap().status(), 403);
}

#[test]
fn transfer_queue_runs_uploads_and_downloads() {
    let server = MockS3::start();
    server.put(
        "media",
        "remote/report.pdf",
        &patterned(20_000),
        "application/pdf",
    );
    let dir = temp_dir("queue");
    fs::write(dir.join("local.txt"), patterned(10_000)).unwrap();
    let client = Arc::new(client(&server));

    let (sender, receiver) = mpsc::channel();
    let transfers = Transfers::start(move |changed| {
        let _ = sender.send(changed);
    });
    let request = |kind, key: &str, path: PathBuf, size| Request {
        kind,
        client: client.clone(),
        conn_id: "test".into(),
        bucket: "media".into(),
        key: key.into(),
        path,
        size,
    };
    let queued = transfers.enqueue(vec![
        request(
            Kind::Upload,
            "uploaded/local.txt",
            dir.join("local.txt"),
            10_000,
        ),
        request(
            Kind::Download,
            "remote/report.pdf",
            dir.join("out/report.pdf"),
            20_000,
        ),
        request(
            Kind::Download,
            "remote/missing.pdf",
            dir.join("out/missing.pdf"),
            1,
        ),
    ]);
    assert_eq!(queued, 3);

    while transfers.is_busy() {
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("transfers should keep reporting progress");
    }

    let views = transfers.list();
    assert_eq!(views[0].status, Status::Done);
    assert_eq!(views[0].done, 10_000);
    assert_eq!(views[1].status, Status::Done);
    assert_eq!(views[2].status, Status::Failed);
    assert!(views[2].error.contains("NoSuchKey"), "{}", views[2].error);

    assert_eq!(
        server.get("media", "uploaded/local.txt").unwrap().data,
        patterned(10_000)
    );
    assert_eq!(
        fs::read(dir.join("out/report.pdf")).unwrap(),
        patterned(20_000)
    );
    assert!(!dir.join("out/missing.pdf").exists());
    assert!(!dir.join("out/missing.pdf.part").exists());

    transfers.clear_finished();
    assert!(transfers.list().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn preview_links_serve_objects_without_exposing_the_account() {
    let server = MockS3::start();
    server.put(
        "media",
        "clips/intro video.mp4",
        &patterned(50_000),
        "video/mp4",
    );
    let previews = fathom::proxy::PreviewServer::start().unwrap();
    let link = previews.link(Arc::new(client(&server)), "media", "clips/intro video.mp4");

    assert!(link.starts_with("http://127.0.0.1:"));
    for secret in [ACCESS_KEY, SECRET_KEY, "X-Amz", "media", "intro"] {
        assert!(!link.contains(secret), "{link}");
    }

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut whole = agent.get(&link).call().unwrap();
    assert_eq!(whole.status(), 200);
    assert_eq!(whole.headers()["content-type"], "video/mp4");
    assert_eq!(whole.body_mut().read_to_vec().unwrap(), patterned(50_000));

    // Seeking in a video is a range request.
    let mut part = agent
        .get(&link)
        .header("range", "bytes=100-199")
        .call()
        .unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"], "bytes 100-199/50000");
    assert_eq!(
        part.body_mut().read_to_vec().unwrap(),
        patterned(50_000)[100..200]
    );

    let guess = format!("{}/{}", &link[..link.rfind('/').unwrap()], "0".repeat(32));
    assert_eq!(agent.get(&guess).call().unwrap().status(), 404);
    assert_eq!(agent.post(&link).send("x").unwrap().status(), 404);
}
