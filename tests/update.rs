//! Finding and downloading updates, against the stand-in for GitHub in `common`.

mod common;

use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::MockS3;
use fathom::update::GitHub;

const SETUP: &str = "fathom-999.1.0-windows-x64-setup.exe";

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fathom-update-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn finds_the_latest_release() {
    let server = MockS3::start();
    let github = GitHub::new(Some(&server.endpoint));
    let missing = github.latest().unwrap_err();
    assert!(missing.contains("no releases"), "{missing}");

    server.publish("999.1.0", &[(SETUP, b"setup program")]);
    let release = github.latest().unwrap();
    assert_eq!(release.version, "999.1.0");
    assert_eq!(
        release.page,
        "https://github.com/dend/fathom/releases/tag/999.1.0"
    );
    assert!(release.is_newer());
    assert_eq!(release.size_of(SETUP), Some(13));

    server.publish("0.0.0", &[]);
    assert!(!github.latest().unwrap().is_newer());
}

#[test]
fn downloads_only_what_matches_the_checksums() {
    let server = MockS3::start();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    server.publish("999.1.0", &[(SETUP, &data)]);
    let github = GitHub::new(Some(&server.endpoint));
    let release = github.latest().unwrap();
    let dir = temp_dir("download");
    let never = AtomicBool::new(false);
    let received = AtomicU64::new(0);
    let progress = |done: u64| received.store(done, Ordering::Relaxed);

    let file = github
        .download(&release, SETUP, &dir, &progress, &never)
        .unwrap();
    assert_eq!(file, dir.join(SETUP));
    assert_eq!(fs::read(&file).unwrap(), data);
    assert_eq!(received.load(Ordering::Relaxed), data.len() as u64);

    // A download that doesn't match is thrown away.
    fs::remove_file(&file).unwrap();
    server.corrupt(SETUP);
    let error = github
        .download(&release, SETUP, &dir, &progress, &never)
        .unwrap_err();
    assert!(
        error.contains("doesn't match the release's checksum"),
        "{error}"
    );
    assert!(!file.exists());
    assert!(!dir.join(format!("{SETUP}.part")).exists());

    let missing = github
        .download(
            &release,
            "fathom-999.1.0-linux-x64.deb",
            &dir,
            &progress,
            &never,
        )
        .unwrap_err();
    assert!(
        missing.contains("has no fathom-999.1.0-linux-x64.deb"),
        "{missing}"
    );

    let cancelled = AtomicBool::new(true);
    let error = github
        .download(&release, SETUP, &dir, &progress, &cancelled)
        .unwrap_err();
    assert_eq!(error, "Cancelled");
    let _ = fs::remove_dir_all(dir);
}
