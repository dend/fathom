//! The few things the app needs from the operating system: where to keep
//! settings, the local time zone, and handing files and URLs to other apps.

use std::path::{Path, PathBuf};
use std::process::Command;

const APP_ID: &str = "dev.den.fathom";

/// Where settings are kept. `FATHOM_CONFIG_DIR` overrides it, which keeps
/// development and test runs away from real settings.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FATHOM_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
    };
    base.join(APP_ID)
}

/// Seconds to add to a UTC timestamp to get local time at that moment.
#[cfg(windows)]
pub fn local_offset(unix: i64) -> i64 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTime;

    let (year, month, day, hour, minute, second) = crate::format::civil(unix);
    let utc = SYSTEMTIME {
        wYear: year as u16,
        wMonth: month as u16,
        wDayOfWeek: 0,
        wDay: day as u16,
        wHour: hour as u16,
        wMinute: minute as u16,
        wSecond: second as u16,
        wMilliseconds: 0,
    };
    let mut local = utc;
    // SAFETY: both pointers refer to valid SYSTEMTIME values for the duration of the call.
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return 0;
    }
    let local_unix = crate::format::unix_from_civil(
        i64::from(local.wYear),
        i64::from(local.wMonth),
        i64::from(local.wDay),
        i64::from(local.wHour),
        i64::from(local.wMinute),
        i64::from(local.wSecond),
    );
    local_unix - unix
}

/// Seconds to add to a UTC timestamp to get local time at that moment.
#[cfg(unix)]
pub fn local_offset(unix: i64) -> i64 {
    let time = unix as libc::time_t;
    // SAFETY: `tm` is plain data that localtime_r fills in; both pointers are valid.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens a file or URL with whatever the system uses for it.
pub fn open(target: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let operation = wide("open");
        let file = wide(target);
        // SAFETY: all strings are NUL-terminated UTF-16 buffers that outlive the call.
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecute reports success with a value greater than 32.
        if result as usize > 32 {
            Ok(())
        } else {
            Err("Windows could not open it".into())
        }
    }
    #[cfg(target_os = "macos")]
    {
        spawn(Command::new("open").arg(target))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        spawn(Command::new("xdg-open").arg(target))
    }
}

/// Shows a file in the system file manager.
pub fn reveal(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(path.as_os_str());
        spawn(Command::new("explorer").arg(select))
    }
    #[cfg(target_os = "macos")]
    {
        spawn(Command::new("open").arg("-R").arg(path))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let folder = path.parent().unwrap_or(path);
        spawn(Command::new("xdg-open").arg(folder))
    }
}

fn spawn(command: &mut Command) -> Result<(), String> {
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not start {:?}: {e}", command.get_program()))
}

/// Plays a video or audio stream (a local proxy URL) in the default player for
/// files with `extension`. Falls back to the system's URL handler, which is
/// usually a browser, when no player that accepts URLs can be found.
pub fn play(url: &str, extension: &str) -> Result<(), String> {
    if let Some(player) = default_player(extension) {
        if launch_player(&player, url).is_ok() {
            return Ok(());
        }
    }
    open(url)
}

#[cfg(windows)]
struct Player(PathBuf);

#[cfg(windows)]
fn default_player(extension: &str) -> Option<Player> {
    use windows_sys::Win32::UI::Shell::{AssocQueryStringW, ASSOCF_NONE, ASSOCSTR_EXECUTABLE};
    let association = wide(&format!(".{extension}"));
    let verb = wide("open");
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    // SAFETY: the input strings are NUL-terminated and `length` holds the buffer capacity.
    let result = unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_EXECUTABLE,
            association.as_ptr(),
            verb.as_ptr(),
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if result != 0 {
        return None;
    }
    let path = PathBuf::from(String::from_utf16_lossy(
        &buffer[..length.saturating_sub(1) as usize],
    ));
    // Store apps cannot be started by path and many do not accept URLs.
    let packaged = path
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("\\windowsapps\\");
    (path.is_file() && !packaged).then_some(Player(path))
}

#[cfg(windows)]
fn launch_player(player: &Player, url: &str) -> Result<(), String> {
    spawn(Command::new(&player.0).arg(url))
}

#[cfg(target_os = "macos")]
struct Player(&'static str);

#[cfg(target_os = "macos")]
fn default_player(extension: &str) -> Option<Player> {
    // QuickTime Player is the system player and streams these formats over HTTP.
    matches!(
        extension,
        "mp4" | "m4v" | "mov" | "mp3" | "m4a" | "aac" | "wav" | "aiff"
    )
    .then_some(Player("com.apple.QuickTimePlayerX"))
}

#[cfg(target_os = "macos")]
fn launch_player(player: &Player, url: &str) -> Result<(), String> {
    spawn(Command::new("open").arg("-b").arg(player.0).arg(url))
}

#[cfg(all(unix, not(target_os = "macos")))]
struct Player(Vec<String>);

/// Resolves the default application for the file's MIME type from its
/// `.desktop` entry, keeping only players that accept URLs.
#[cfg(all(unix, not(target_os = "macos")))]
fn default_player(extension: &str) -> Option<Player> {
    let mime = match extension {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "ogv" => "video/ogg",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "wav" => "audio/x-wav",
        "aac" => "audio/aac",
        _ => return None,
    };
    let output = Command::new("xdg-mime")
        .args(["query", "default", mime])
        .output()
        .ok()?;
    let desktop_id = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if desktop_id.is_empty() {
        return None;
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .into_iter()
        .collect();
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );

    let entry = dirs
        .iter()
        .find_map(|dir| std::fs::read_to_string(dir.join("applications").join(&desktop_id)).ok())?;
    let exec = entry
        .lines()
        .skip_while(|line| line.trim() != "[Desktop Entry]")
        .find_map(|line| line.strip_prefix("Exec="))?;
    // Only players whose command line takes URLs (%u / %U) can stream.
    if !exec.contains("%u") && !exec.contains("%U") {
        return None;
    }
    let args: Vec<String> = exec
        .split_whitespace()
        .filter(|arg| !(arg.len() == 2 && arg.starts_with('%') && !matches!(*arg, "%u" | "%U")))
        .map(|arg| arg.trim_matches('"').to_string())
        .collect();
    (!args.is_empty()).then_some(Player(args))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch_player(player: &Player, url: &str) -> Result<(), String> {
    let args: Vec<String> = player
        .0
        .iter()
        .map(|arg| {
            if arg == "%u" || arg == "%U" {
                url.to_string()
            } else {
                arg.clone()
            }
        })
        .collect();
    spawn(Command::new(&args[0]).args(&args[1..]))
}
