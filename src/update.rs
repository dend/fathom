//! Updates from GitHub releases: finding a newer version, downloading it,
//! checking it against the release's checksums and putting it in place of
//! this copy. The app and `fth` share all of it, and the record of the last
//! check, so that between them they check once a day.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where releases are published.
pub const REPOSITORY: &str = "dend/fathom";
const API: &str = "https://api.github.com";
/// The release file listing the SHA-256 of every other file.
const CHECKSUMS: &str = "SHA256SUMS.txt";
/// How long a check stands before another one runs on its own.
const RECHECK_AFTER: u64 = 24 * 60 * 60;
/// How long to wait after a check that failed, such as while offline.
const RETRY_AFTER: u64 = 60 * 60;

/// This copy's version.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Whether this copy was built by the release workflow. GitHub Actions names
/// the commit it builds. A copy built from source would always look out of
/// date, so only release builds check on their own and update themselves.
pub fn is_release_build() -> bool {
    option_env!("GITHUB_SHA").is_some()
}

/// Whether to check for updates without being asked: in release builds,
/// unless `FATHOM_NO_UPDATE_CHECK` is set.
pub fn checks_on_its_own() -> bool {
    is_release_build() && std::env::var_os("FATHOM_NO_UPDATE_CHECK").is_none()
}

/// Whether `version` comes after `than` by semantic versioning: `0.10.0` is
/// newer than `0.9.0`, and `1.0.0` than `1.0.0-beta.2`. Anything that isn't a
/// version is never newer.
pub fn is_newer(version: &str, than: &str) -> bool {
    match (Version::parse(version), Version::parse(than)) {
        (Some(version), Some(than)) => version > than,
        _ => false,
    }
}

#[derive(PartialEq, Eq)]
struct Version {
    numbers: [u64; 3],
    /// Pre-release identifiers, such as `beta` and `2` in `1.0.0-beta.2`.
    pre: Vec<Identifier>,
}

/// Numbers sort before words, as semantic versioning says.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Number(u64),
    Word(String),
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        // Build metadata (`+...`) doesn't count.
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut numbers = [0; 3];
        for number in &mut numbers {
            *number = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        let pre = pre
            .map(|pre| {
                pre.split('.')
                    .map(|id| match id.parse() {
                        Ok(number) => Identifier::Number(number),
                        Err(_) => Identifier::Word(id.to_string()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self { numbers, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers.cmp(&other.numbers).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ----- releases -------------------------------------------------------------------

/// A published release.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    /// The release's page on GitHub, with what changed.
    pub page: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    #[serde(rename = "browser_download_url")]
    url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

impl Release {
    /// Whether this release is newer than the running copy.
    pub fn is_newer(&self) -> bool {
        is_newer(&self.version, current())
    }

    /// The size of one of the release's files.
    pub fn size_of(&self, name: &str) -> Option<u64> {
        self.asset(name).map(|asset| asset.size)
    }

    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// Fathom's releases on GitHub.
pub struct GitHub {
    agent: ureq::Agent,
    base: String,
}

impl GitHub {
    /// `base` stands in for the GitHub API, for tests.
    pub fn new(base: Option<&str>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("fathom/", env!("CARGO_PKG_VERSION")))
            .timeout_resolve(Some(Duration::from_secs(15)))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .build()
            .into();
        Self {
            agent,
            base: base.unwrap_or(API).trim_end_matches('/').to_string(),
        }
    }

    /// The newest release. Drafts and pre-releases don't count.
    pub fn latest(&self) -> Result<Release, String> {
        let url = format!("{}/repos/{REPOSITORY}/releases/latest", self.base);
        let mut response = self
            .agent
            .get(&url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .call()
            .map_err(|e| format!("Could not reach GitHub: {e}"))?;
        let status = response.status().as_u16();
        let limited = response
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0");
        match status {
            404 => return Err("Fathom has no releases on GitHub yet".into()),
            403 | 429 if limited => {
                return Err("GitHub is limiting how often this computer can check for \
                            updates. Try again in an hour."
                    .into())
            }
            300.. => return Err(format!("GitHub returned HTTP {status}")),
            _ => {}
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| format!("Could not read GitHub's answer: {e}"))?;
        let latest: Latest = serde_json::from_str(&body)
            .map_err(|_| "GitHub's answer wasn't a release".to_string())?;
        Ok(Release {
            version: latest.tag_name.trim_start_matches('v').to_string(),
            page: latest.html_url,
            assets: latest.assets,
        })
    }

    /// Downloads one of the release's files into `dir` and checks it against
    /// the release's checksums. `progress` gets the bytes so far.
    pub fn download(
        &self,
        release: &Release,
        name: &str,
        dir: &Path,
        progress: &(dyn Fn(u64) + Sync),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, String> {
        let asset = release
            .asset(name)
            .ok_or_else(|| format!("Fathom {} has no {name} to download", release.version))?;
        let sums = release
            .asset(CHECKSUMS)
            .ok_or("The release has no checksums to check the download against")?;
        let sums = self.get(&sums.url)?.read_to_string().map_err(read_error)?;
        let expected = checksum_for(&sums, name)
            .ok_or_else(|| format!("The release's checksums don't include {name}"))?;

        fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        let path = dir.join(name);
        let partial = dir.join(format!("{name}.part"));
        let result = self.save(&asset.url, &partial, progress, cancel);
        let finished = match result {
            Ok(digest) if digest == expected => {
                fs::rename(&partial, &path).map_err(|e| e.to_string())
            }
            Ok(_) => Err(
                "The download doesn't match the release's checksum, so it wasn't installed."
                    .to_string(),
            ),
            Err(error) => Err(error),
        };
        if finished.is_err() {
            let _ = fs::remove_file(&partial);
        }
        finished.map(|()| path)
    }

    fn get(&self, url: &str) -> Result<ureq::Body, String> {
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| format!("Could not download the update: {e}"))?;
        match response.status().as_u16() {
            status @ 300.. => Err(format!("GitHub returned HTTP {status} for the download")),
            _ => Ok(response.into_body()),
        }
    }

    /// Streams `url` into `path`, returning the SHA-256 of what arrived.
    fn save(
        &self,
        url: &str,
        path: &Path,
        progress: &(dyn Fn(u64) + Sync),
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let mut reader = self.get(url)?.into_reader();
        let mut file = File::create(path).map_err(|e| format!("Could not save the update: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut written = 0u64;
        loop {
            if cancel.load(AtomicOrdering::Relaxed) {
                return Err("Cancelled".into());
            }
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(read_error(err)),
            };
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(|e| format!("Could not save the update: {e}"))?;
            written += read as u64;
            progress(written);
        }
        file.flush().map_err(|e| e.to_string())?;
        Ok(hex(&hasher.finalize()))
    }
}

fn read_error(error: impl std::fmt::Display) -> String {
    format!("The download stopped: {error}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The checksum of `name` in a `sha256sum` listing.
fn checksum_for(listing: &str, name: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        // `*` marks a file read in binary mode.
        let file = file.trim_start().trim_start_matches('*');
        (file == name).then(|| hash.to_ascii_lowercase())
    })
}

/// Where updates are downloaded to.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("fathom-update")
}

// ----- remembering checks ---------------------------------------------------------------

/// A release newer than this copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub version: String,
    pub page: String,
}

impl From<&Release> for Notice {
    fn from(release: &Release) -> Self {
        Self {
            version: release.version.clone(),
            page: release.page.clone(),
        }
    }
}

/// The last check, kept in the settings folder.
pub struct Checks {
    file: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Record {
    /// When the next check is due, in seconds since 1970.
    due: u64,
    version: String,
    page: String,
}

impl Checks {
    pub fn new(settings: &Path) -> Self {
        Self {
            file: settings.join("update.json"),
        }
    }

    fn read(&self) -> Record {
        fs::read_to_string(&self.file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write(&self, record: &Record) {
        if let (Some(dir), Ok(json)) = (self.file.parent(), serde_json::to_string(record)) {
            let _ = fs::create_dir_all(dir);
            let _ = fs::write(&self.file, json);
        }
    }

    /// The newer release the last check found, if this copy is still older.
    pub fn known(&self) -> Option<Notice> {
        let record = self.read();
        is_newer(&record.version, current()).then_some(Notice {
            version: record.version,
            page: record.page,
        })
    }

    /// Whether it's time to check again.
    pub fn due(&self) -> bool {
        let due = self.read().due;
        let now = unix_now();
        // A due date far ahead means the clock was turned back.
        now >= due || due > now + RECHECK_AFTER
    }

    /// Puts the next check off for an hour, keeping what is known. Done as a
    /// check starts, so one that fails or never finishes, such as while
    /// offline, isn't tried again straight away.
    pub fn postpone(&self) {
        let mut record = self.read();
        record.due = unix_now() + RETRY_AFTER;
        self.write(&record);
    }

    /// Remembers the latest release, and checks again in a day.
    pub fn record(&self, release: &Release) {
        self.write(&Record {
            due: unix_now() + RECHECK_AFTER,
            version: release.version.clone(),
            page: release.page.clone(),
        });
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ----- installing -----------------------------------------------------------------------

/// How this copy was installed, which decides how it updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// Windows, set up by the installer: the new installer updates it.
    Installer,
    /// Windows, unzipped from the portable zip into this folder.
    Portable(PathBuf),
    /// macOS, the app bundle at this path.
    App(PathBuf),
    /// Linux, the AppImage at this path.
    AppImage(PathBuf),
    /// Linux, installed from the .deb package.
    Package,
}

impl Method {
    /// How this copy was installed, or why it can't update itself.
    pub fn detect() -> Result<Self, String> {
        if !is_release_build() {
            return Err(
                "This copy of Fathom was built from source, so it doesn't update \
                        itself."
                    .into(),
            );
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // On macOS, fth may be run through a link in /usr/local/bin.
        let exe = if cfg!(target_os = "macos") {
            exe.canonicalize().unwrap_or(exe)
        } else {
            exe
        };
        detect_at(&exe, std::env::var_os("APPIMAGE").map(PathBuf::from))
    }

    /// The release file that updates this copy.
    pub fn asset(&self, version: &str) -> Result<String, String> {
        let x64 = cfg!(target_arch = "x86_64");
        let mac = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x64"
        };
        Ok(match self {
            Method::Installer if x64 => format!("fathom-{version}-windows-x64-setup.exe"),
            Method::Portable(_) if x64 => format!("fathom-{version}-windows-x64-portable.zip"),
            Method::App(_) => format!("fathom-{version}-macos-{mac}.dmg"),
            Method::AppImage(_) if x64 => format!("fathom-{version}-linux-x64.AppImage"),
            Method::Package if x64 => format!("fathom-{version}-linux-x64.deb"),
            _ => return Err("There's no download of Fathom for this kind of computer yet.".into()),
        })
    }

    /// What updating the app does, said before it starts.
    pub fn plan(&self) -> &'static str {
        match self {
            Method::Package => {
                "Fathom asks for your password to install the update, then opens again."
            }
            _ => "Fathom closes, installs the update and opens again.",
        }
    }
}

fn detect_at(exe: &Path, appimage: Option<PathBuf>) -> Result<Method, String> {
    let dir = exe.parent().unwrap_or(Path::new(""));
    if cfg!(windows) {
        // Inno Setup leaves its uninstaller next to what it installed.
        if dir.join("unins000.exe").is_file() {
            return Ok(Method::Installer);
        }
        if ["fathom.exe", "fth.exe", "LICENSE.txt"]
            .iter()
            .all(|name| dir.join(name).is_file())
        {
            return Ok(Method::Portable(dir.to_path_buf()));
        }
    } else if cfg!(target_os = "macos") {
        if let Some(bundle) = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
        {
            return Ok(Method::App(bundle.to_path_buf()));
        }
    } else if let Some(path) = appimage {
        return Ok(Method::AppImage(path));
    } else if dir == Path::new("/usr/bin") {
        return Ok(Method::Package);
    }
    Err("This copy of Fathom wasn't installed from a release, so it can't update itself.".into())
}

/// Puts a downloaded release in place. The app passes `reopen` and closes
/// once this returns, to open again updated. The command line tool doesn't,
/// and this returns once the update is in place.
pub fn install(method: &Method, file: &Path, reopen: bool) -> Result<(), String> {
    match method {
        Method::Installer => run_installer(file, reopen),
        Method::Portable(dir) => {
            swap_portable(file, dir)?;
            if reopen {
                spawn(&mut Command::new(dir.join("fathom.exe")))?;
            }
            Ok(())
        }
        Method::App(bundle) => {
            replace_app(file, bundle)?;
            if reopen {
                reopen_after_exit(OsStr::new("open"), &[bundle.as_os_str()])?;
            }
            Ok(())
        }
        Method::AppImage(path) => {
            replace_file(file, path)?;
            if reopen {
                reopen_after_exit(path.as_os_str(), &[])?;
            }
            Ok(())
        }
        Method::Package => {
            // The app has no terminal to ask for a password in, so polkit asks.
            let elevate = if reopen { "pkexec" } else { "sudo" };
            run(Command::new(elevate).arg("dpkg").arg("-i").arg(file)).map_err(|e| {
                format!(
                    "{e}. To install it yourself, run: sudo dpkg -i {}",
                    file.display()
                )
            })?;
            if reopen {
                reopen_after_exit(OsStr::new("/usr/bin/fathom"), &[])?;
            }
            Ok(())
        }
    }
}

/// Removes programs an earlier update moved aside because they were running.
pub fn tidy() {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return;
    };
    for name in ["fathom.exe", "fth.exe", "LICENSE.txt"] {
        let _ = fs::remove_file(dir.join(format!("{name}.old")));
    }
}

fn run(command: &mut Command) -> Result<(), String> {
    let program = Path::new(command.get_program());
    let program = program.file_name().unwrap_or(program.as_os_str());
    let program = program.to_string_lossy().into_owned();
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(match status.code() {
            Some(code) => format!("{program} stopped with exit code {code}"),
            None => format!("{program} stopped"),
        }),
        Err(error) => Err(format!("Could not run {program}: {error}")),
    }
}

fn spawn(command: &mut Command) -> Result<(), String> {
    let program = Path::new(command.get_program());
    let name = program.file_name().unwrap_or(program.as_os_str());
    let name = name.to_string_lossy().into_owned();
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not start {name}: {e}"))
}

/// Opens `program` once this process has exited.
fn reopen_after_exit(program: &OsStr, args: &[&OsStr]) -> Result<(), String> {
    spawn(
        Command::new("sh")
            .arg("-c")
            .arg(r#"while kill -0 "$0" 2>/dev/null; do sleep 0.2; done; exec "$@""#)
            .arg(std::process::id().to_string())
            .arg(program)
            .args(args),
    )
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn run_installer(setup: &Path, reopen: bool) -> Result<(), String> {
    let mut command = Command::new(setup);
    command.args(["/SUPPRESSMSGBOXES", "/NORESTART"]);
    if reopen {
        // The app closes now. Setup shows its progress, then opens the new version.
        return spawn(command.args(["/SILENT", "/relaunch=yes"]));
    }
    // Setup replaces fth.exe, which is running this. A running program can be
    // renamed but not replaced, so it moves aside for the new one.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let aside = with_suffix(&exe, ".old");
    let _ = fs::remove_file(&aside);
    let moved = fs::rename(&exe, &aside).is_ok();
    let result = run(command.arg("/VERYSILENT"));
    if result.is_err() && moved && !exe.exists() {
        let _ = fs::rename(&aside, &exe);
    }
    result.map_err(|e| format!("The update didn't install: {e}"))
}

fn swap_portable(zip: &Path, dir: &Path) -> Result<(), String> {
    let unpacked = zip.with_extension("");
    let _ = fs::remove_dir_all(&unpacked);
    fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    // Windows comes with a tar that reads zip files. Another tar may come
    // first on the PATH, so it is named in full.
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let tar = Path::new(&system).join("System32").join("tar.exe");
    run(Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(&unpacked))
    .map_err(|e| format!("Could not unpack the update: {e}"))?;
    for name in ["fathom.exe", "fth.exe", "LICENSE.txt"] {
        let new = unpacked.join(name);
        if !new.is_file() {
            return Err(format!("The update has no {name} in it"));
        }
        // Running programs can't be replaced, but they can move aside.
        let target = dir.join(name);
        let aside = dir.join(format!("{name}.old"));
        let _ = fs::remove_file(&aside);
        if target.exists() {
            fs::rename(&target, &aside)
                .map_err(|e| format!("Could not replace {}: {e}", target.display()))?;
        }
        if let Err(error) = fs::copy(&new, &target) {
            let _ = fs::rename(&aside, &target);
            return Err(format!("Could not replace {}: {error}", target.display()));
        }
    }
    let _ = fs::remove_dir_all(&unpacked);
    Ok(())
}

fn replace_app(image: &Path, bundle: &Path) -> Result<(), String> {
    let (Some(parent), Some(name)) = (bundle.parent(), bundle.file_name()) else {
        return Err(format!("{} isn't an app", bundle.display()));
    };
    let name = name.to_string_lossy();
    let mount = image.with_extension("mount");
    fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
    run(Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-quiet",
            "-mountpoint",
        ])
        .arg(&mount)
        .arg(image))
    .map_err(|e| format!("Could not open the disk image: {e}"))?;
    let staged = parent.join(format!(".{name}.new"));
    let copied = (|| {
        let app = fs::read_dir(&mount)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|e| e == "app"))
            .ok_or("The disk image has no app in it")?;
        let _ = fs::remove_dir_all(&staged);
        // ditto keeps the bundle's links, permissions and signature intact.
        run(Command::new("ditto").arg(app).arg(&staged))
    })();
    let _ = run(Command::new("hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount));
    copied.map_err(|e| format!("Could not copy the new version: {e}"))?;

    let aside = parent.join(format!(".{name}.old"));
    let _ = fs::remove_dir_all(&aside);
    fs::rename(bundle, &aside).map_err(|e| {
        let _ = fs::remove_dir_all(&staged);
        format!("Could not replace {}: {e}", bundle.display())
    })?;
    if let Err(error) = fs::rename(&staged, bundle) {
        let _ = fs::rename(&aside, bundle);
        return Err(format!("Could not replace {}: {error}", bundle.display()));
    }
    let _ = fs::remove_dir_all(&aside);
    Ok(())
}

fn replace_file(new: &Path, target: &Path) -> Result<(), String> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staged = target.with_file_name(format!(".{name}.new"));
    fs::copy(new, &staged)
        .and_then(|_| make_executable(&staged))
        .and_then(|()| fs::rename(&staged, target))
        .map_err(|e| {
            let _ = fs::remove_file(&staged);
            format!("Could not replace {}: {e}", target.display())
        })
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_by_semantic_versioning() {
        assert!(is_newer("0.0.4", "0.0.3"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("v1.2.3", "1.2.2"));
        assert!(!is_newer("0.0.3", "0.0.3"));
        assert!(!is_newer("0.0.2", "0.0.3"));
        // A release comes after its pre-releases, which sort by their parts.
        assert!(is_newer("1.0.0", "1.0.0-beta.2"));
        assert!(is_newer("1.0.0-beta.10", "1.0.0-beta.2"));
        assert!(is_newer("1.0.0-beta", "1.0.0-alpha.9"));
        assert!(is_newer("1.0.0-beta.1", "1.0.0-beta"));
        assert!(is_newer("0.0.4", "0.0.4-dev.17"));
        assert!(!is_newer("1.0.0-rc.1", "1.0.0"));
        // Build metadata doesn't count, and nonsense is never newer.
        assert!(!is_newer("1.0.0+build.5", "1.0.0"));
        assert!(!is_newer("latest", "0.0.1"));
        assert!(!is_newer("1.2", "1.1.0"));
    }

    #[test]
    fn finds_checksums_in_a_sha256sum_listing() {
        let listing = "\
            9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  fathom-0.0.4-windows-x64-setup.exe\n\
            60303AE22B998861BCE3B28F33EEC1BE758A213C86C93C076DBE9F558C11C752 *fathom-0.0.4-linux-x64.deb\n";
        assert_eq!(
            checksum_for(listing, "fathom-0.0.4-windows-x64-setup.exe").as_deref(),
            Some("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
        );
        assert_eq!(
            checksum_for(listing, "fathom-0.0.4-linux-x64.deb").as_deref(),
            Some("60303ae22b998861bce3b28f33eec1be758a213c86c93c076dbe9f558c11c752")
        );
        assert_eq!(checksum_for(listing, "fathom-0.0.4-linux-x64"), None);
    }

    #[test]
    fn remembers_checks_between_runs() {
        let dir = std::env::temp_dir().join(format!("fathom-checks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let checks = Checks::new(&dir);
        assert!(checks.due());
        assert_eq!(checks.known(), None);

        let release = Release {
            version: "999.0.0".into(),
            page: "https://github.com/dend/fathom/releases/tag/999.0.0".into(),
            assets: Vec::new(),
        };
        checks.record(&release);
        assert!(!checks.due());
        assert_eq!(checks.known().unwrap().version, "999.0.0");

        // A check that is put off keeps what was found before.
        fs::write(dir.join("update.json"), r#"{"due":0,"version":"999.0.0"}"#).unwrap();
        assert!(checks.due());
        checks.postpone();
        assert!(!checks.due());
        assert_eq!(checks.known().unwrap().version, "999.0.0");

        // A release this copy already has isn't news.
        let old = Release {
            version: "0.0.0".into(),
            ..release
        };
        checks.record(&old);
        assert_eq!(checks.known(), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[cfg(windows)]
    fn tells_installed_copies_from_portable_ones() {
        let dir = std::env::temp_dir().join(format!("fathom-method-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fth.exe");
        fs::write(&exe, b"").unwrap();
        assert!(detect_at(&exe, None).is_err());

        fs::write(dir.join("fathom.exe"), b"").unwrap();
        fs::write(dir.join("LICENSE.txt"), b"").unwrap();
        assert_eq!(detect_at(&exe, None), Ok(Method::Portable(dir.clone())));
        fs::write(dir.join("unins000.exe"), b"").unwrap();
        assert_eq!(detect_at(&exe, None), Ok(Method::Installer));

        assert_eq!(
            Method::Installer.asset("0.0.4").unwrap(),
            "fathom-0.0.4-windows-x64-setup.exe"
        );
        assert_eq!(
            Method::Portable(dir.clone()).asset("0.0.4").unwrap(),
            "fathom-0.0.4-windows-x64-portable.zip"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
