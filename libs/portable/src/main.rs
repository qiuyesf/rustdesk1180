#![windows_subsystem = "windows"]

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use bin_reader::{normalize_path, BinaryReader};

pub mod bin_reader;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
const APP_METADATA: &[u8] = include_bytes!("../app_metadata.toml");
#[cfg(not(windows))]
const APP_METADATA: &[u8] = &[];
const APP_METADATA_CONFIG: &str = "meta.toml";
const META_LINE_PREFIX_TIMESTAMP: &str = "timestamp = ";
const META_LINE_PREFIX_FILE: &str = "file = ";
const APP_PREFIX: &str = "rustdesk";
#[cfg(windows)]
const VERIFIED_UPDATE_FILE_PREFIX: &str = "rustdesk-verified-";
const APPNAME_RUNTIME_ENV_KEY: &str = "RUSTDESK_APPNAME";
// The launcher's directory: the green config root stays there while the
// extracted runtime directory is disposable.
const APPDIR_RUNTIME_ENV_KEY: &str = "RUSTDESK_APPDIR";
#[cfg(windows)]
const SET_FOREGROUND_WINDOW_ENV_KEY: &str = "SET_FOREGROUND_WINDOW";

// The extraction directory follows whatever executable the payload asks for, so a
// custom client gets its own directory instead of sharing RustDesk's. Falls back to
// APP_PREFIX when no package is injected, which keeps stock builds unchanged.
fn app_dir_name(exe: &str) -> String {
    Path::new(&exe.replace('\\', "/"))
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.trim().to_lowercase())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| APP_PREFIX.to_owned())
}

// Extract beside the launcher, with one runtime directory per launcher name so
// differently named clients in the same folder never share extracted files.
// Probing write access keeps read-only locations (program directories, locked
// drives) working by falling back to the stock %LOCALAPPDATA% location.
fn green_extract_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let parent = exe.parent()?;
    let stem = exe.file_stem()?.to_string_lossy();
    let prefix = format!(".rustdesk-{}-", stem);
    let current = format!("{}{}", prefix, std::process::id());
    // Best-effort cleanup of leftovers from earlier runs. A directory still in
    // use cannot be removed on Windows, which protects running instances.
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.as_ref() != current.as_str() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let dir = parent.join(&current);
    std::fs::create_dir_all(&dir).ok()?;
    let probe = dir.join(".writable");
    std::fs::write(&probe, b"").ok()?;
    let _ = std::fs::remove_file(&probe);
    Some(dir)
}

fn is_timestamp_matches(dir: &Path, ts: &mut u64) -> bool {
    let Ok(app_metadata) = std::str::from_utf8(APP_METADATA) else {
        return true;
    };
    for line in app_metadata.lines() {
        if line.starts_with(META_LINE_PREFIX_TIMESTAMP) {
            if let Ok(stored_ts) = line.replace(META_LINE_PREFIX_TIMESTAMP, "").parse::<u64>() {
                *ts = stored_ts;
                break;
            }
        }
    }
    if *ts == 0 {
        return true;
    }

    if let Ok(content) = std::fs::read_to_string(dir.join(APP_METADATA_CONFIG)) {
        for line in content.lines() {
            if line.starts_with(META_LINE_PREFIX_TIMESTAMP) {
                if let Ok(stored_ts) = line.replace(META_LINE_PREFIX_TIMESTAMP, "").parse::<u64>() {
                    return *ts == stored_ts;
                }
            }
        }
    }
    false
}

fn write_meta(dir: &Path, ts: u64, package_paths: &[String]) {
    let meta_file = dir.join(APP_METADATA_CONFIG);
    let mut content = format!("{}{}\n", META_LINE_PREFIX_TIMESTAMP, ts);
    for path in package_paths {
        content.push_str(&format!("{}{}\n", META_LINE_PREFIX_FILE, path));
    }
    // Ignore is ok here
    let _ = std::fs::write(meta_file, content);
}

fn previous_package_files(dir: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(dir.join(APP_METADATA_CONFIG)) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| line.strip_prefix(META_LINE_PREFIX_FILE))
        .map(|path| path.trim().to_owned())
        .collect()
}

// meta.toml is plain text in a user-writable directory, and it now drives deletion,
// so the path is rebuilt from plain components rather than joined as written. A
// prefix, root or parent component would otherwise escape the extraction directory:
// Path::join replaces the base entirely when given an absolute path.
fn resolve_within(dir: &Path, relative: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut path = dir.to_path_buf();
    let mut any = false;
    for component in Path::new(&relative.replace('\\', "/")).components() {
        match component {
            Component::Normal(part) => {
                // A drive-relative name like "C:x" parses as Normal, and only a
                // Windows host would classify "C:/..." as a Prefix, so the colon is
                // rejected outright rather than relying on the host's parser.
                if part.to_string_lossy().contains(':') {
                    return None;
                }
                path.push(part);
                any = true;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if any {
        Some(path)
    } else {
        None
    }
}

// A customer who drops a branding asset gets a package without it, and the file
// would otherwise linger in an existing extraction and keep being used. The wipe
// cannot cover this: it is keyed on the packer's build timestamp, which is now the
// same for every customer of a release.
fn remove_dropped_package_files_with<F>(
    dir: &Path,
    current: &[String],
    mut remove_file: F,
) -> Vec<String>
where
    F: FnMut(&Path) -> std::io::Result<()>,
{
    let keep: std::collections::HashSet<String> =
        current.iter().map(|p| normalize_path(p)).collect();
    let mut failed = Vec::new();
    for previous in previous_package_files(dir) {
        if keep.contains(&normalize_path(&previous)) {
            continue;
        }
        let Some(path) = resolve_within(dir, &previous) else {
            continue;
        };
        if path.is_file() {
            println!("removing dropped {}", previous);
            if let Err(error) = remove_file(&path) {
                eprintln!("failed to remove dropped {}: {}", previous, error);
                failed.push(previous);
            }
        }
    }
    failed
}

fn remove_dropped_package_files(dir: &Path, current: &[String]) -> Vec<String> {
    remove_dropped_package_files_with(dir, current, |path| std::fs::remove_file(path))
}

fn setup(
    reader: BinaryReader,
    dir: Option<PathBuf>,
    clear: bool,
    _args: &Vec<String>,
    _ui: &mut bool,
) -> Option<(PathBuf, PathBuf, bool)> {
    // `disposable` marks a runtime directory created by this launcher: it is
    // rebuilt on every start and removed once the child process exits.
    let (dir, disposable) = if let Some(dir) = dir {
        (dir, false)
    } else if let Some(dir) = green_extract_dir() {
        (dir, true)
    } else if let Some(dir) = dirs::data_local_dir() {
        (dir.join(app_dir_name(&reader.exe)), false)
    } else {
        eprintln!("not found data local dir");
        return None;
    };

    let mut ts = 0;
    if clear || disposable || !is_timestamp_matches(&dir, &mut ts) {
        #[cfg(windows)]
        if _args.is_empty() {
            *_ui = true;
            // The splash window has no auto-close event. The green launcher now
            // waits for the child, so showing it would keep the splash on screen
            // for the whole session. Keep it for the legacy AppData mode only.
            if !disposable {
                ui::setup();
            }
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
    }
    let mut metadata_paths = reader.package_paths.clone();
    metadata_paths.extend(remove_dropped_package_files(&dir, &reader.package_paths));
    for file in reader.files.iter() {
        file.write_to_file(&dir);
    }
    write_meta(&dir, ts, &metadata_paths);
    #[cfg(windows)]
    win::copy_runtime_broker(&dir);
    #[cfg(linux)]
    reader.configure_permission(&dir);
    Some((dir.join(&reader.exe), dir, disposable))
}

fn use_null_stdio() -> bool {
    #[cfg(windows)]
    {
        // When running in CMD on Windows 7, using Stdio::inherit() with spawn returns an "invalid handle" error.
        // Since using Stdio::null() didn’t cause any issues, and determining whether the program is launched from CMD or by double-clicking would require calling more APIs during startup, we also use Stdio::null() when launched by double-clicking on Windows 7.
        let is_windows_7 = is_windows_7();
        println!("is windows7: {}", is_windows_7);
        return is_windows_7;
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn is_windows_7() -> bool {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    unsafe {
        let mut version_info = OSVERSIONINFOW::default();
        version_info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;

        if RtlGetVersion(&mut version_info).is_ok() {
            // Windows 7 is version 6.1
            println!(
                "Windows version: {}.{}",
                version_info.dwMajorVersion, version_info.dwMinorVersion
            );
            return version_info.dwMajorVersion == 6 && version_info.dwMinorVersion == 1;
        }
    }
    false
}

// Mirror of `core_main::init_green_app_name()`: the green config directory is
// <launcher dir>/<APP_NAME>, where APP_NAME comes from the launcher's own file
// name. Keep both sides in sync if the naming rules ever change.
#[cfg(windows)]
fn green_config_dir_name(exe_name: &std::ffi::OsStr) -> String {
    let stem = Path::new(exe_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.eq_ignore_ascii_case("rustdesk") {
        return "RustDesk".to_owned();
    }
    let safe: String = stem
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .take(64)
        .collect();
    if safe.is_empty() {
        "RustDesk".to_owned()
    } else {
        safe
    }
}

// Ask once after the child exits. "No" (keep config) is the default button, so
// an accidental Enter can never wipe the ID, password and settings. A failed
// MessageBox returns 0, which also keeps the config (fail-safe).
#[cfg(windows)]
fn confirm_delete_config(config_name: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winuser::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_TOPMOST, MB_YESNO,
    };
    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let text = wide(&format!(
        "是否删除本程序的配置文件？\n\n配置文件夹：{config_name}\n日志文件夹：log\n\n选择「是」删除上面两个文件夹（下次启动会重新生成 ID 和密码）；选择「否」保留配置。\n\n默认：否（保留）"
    ));
    let caption = wide("RustDesk 便携版");
    let ret = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_DEFBUTTON2 | MB_ICONQUESTION | MB_TOPMOST,
        )
    };
    ret == IDYES
}

fn execute(path: PathBuf, extract_dir: PathBuf, disposable: bool, args: Vec<String>, _ui: bool) {
    println!("executing {}", path.display());
    // setup env
    let exe = std::env::current_exe().unwrap_or_default();
    let exe_name = exe.file_name().unwrap_or_default();
    let exe_dir = exe.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    // A plain double-click (no CLI args) is the only case that should prompt on
    // exit; --install / --quick_support and other flag-driven runs must not.
    #[cfg(windows)]
    let plain_launch = args.is_empty();
    // run executable
    let mut cmd = Command::new(path);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
        if _ui {
            cmd.env(SET_FOREGROUND_WINDOW_ENV_KEY, "1");
        }
    }

    cmd.env(APPNAME_RUNTIME_ENV_KEY, exe_name);
    // Config must live beside the launcher, not inside the disposable runtime
    // directory that is removed after the process exits.
    cmd.env(APPDIR_RUNTIME_ENV_KEY, exe_dir.as_os_str());
    if use_null_stdio() {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    } else {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    }

    match cmd.spawn() {
        Ok(mut child) => {
            #[cfg(windows)]
            if _ui {
                unsafe {
                    winapi::um::winuser::AllowSetForegroundWindow(child.id() as u32);
                }
            }
            let _ = child.wait();
            // Ask about the persistent config only on a plain launch of the
            // portable build. "Yes" drops the config folder and the shared log
            // folder; "No" (the default) keeps both. The disposable extracted
            // files are removed either way below.
            #[cfg(windows)]
            if plain_launch {
                let config_name = green_config_dir_name(exe_name);
                if confirm_delete_config(&config_name) {
                    let _ = std::fs::remove_dir_all(exe_dir.join(&config_name));
                    let _ = std::fs::remove_dir_all(exe_dir.join("log"));
                }
            }
        }
        Err(e) => eprintln!("{:?}", e),
    }

    // Green mode: leave no extracted files behind once RustDesk has closed.
    // Files still in use make this fail; a later start rebuilds the directory.
    if disposable {
        let _ = std::fs::remove_dir_all(&extract_dir);
    }
}

fn main() -> Result<(), String> {
    let mut args = Vec::new();
    let mut arg_exe = Default::default();
    let mut i = 0;
    for arg in std::env::args() {
        if i == 0 {
            arg_exe = arg.clone();
        } else {
            args.push(arg);
        }
        i += 1;
    }
    let click_setup = args.is_empty() && arg_exe.to_lowercase().ends_with("install.exe");
    #[cfg(windows)]
    let quick_support = args.is_empty() && win::is_quick_support_exe(&arg_exe);
    #[cfg(not(windows))]
    let quick_support = false;

    #[cfg(windows)]
    if args.first().is_some_and(|arg| arg == "--update") {
        if let Err(err) = win::schedule_verified_update_cleanup() {
            eprintln!("Failed to schedule verified update file cleanup: {}", err);
        }
    }

    let mut ui = false;
    let reader = BinaryReader::new()?;
    if let Some((exe, extract_dir, disposable)) = setup(
        reader,
        None,
        click_setup || args.contains(&"--silent-install".to_owned()),
        &args,
        &mut ui,
    ) {
        if click_setup {
            args = vec!["--install".to_owned()];
        } else if quick_support {
            args = vec!["--quick_support".to_owned()];
        }
        execute(exe, extract_dir, disposable, args, ui);
    }
    Ok(())
}

#[cfg(windows)]
mod win {
    use std::{fs, os::windows::process::CommandExt, path::Path, process::Command};

    // Used for privacy mode(magnifier impl).
    pub const RUNTIME_BROKER_EXE: &'static str = "C:\\Windows\\System32\\RuntimeBroker.exe";
    pub const WIN_TOPMOST_INJECTED_PROCESS_EXE: &'static str = "RuntimeBroker_rustdesk.exe";

    pub(super) fn schedule_verified_update_cleanup() -> std::io::Result<()> {
        use windows::{
            core::{HSTRING, PCWSTR},
            Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT},
        };

        let current_exe = std::env::current_exe()?;
        let is_verified_update = current_exe
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(super::VERIFIED_UPDATE_FILE_PREFIX) && name.ends_with(".exe")
            });
        if !is_verified_update {
            return Ok(());
        }

        unsafe {
            MoveFileExW(
                &HSTRING::from(current_exe.as_path()),
                PCWSTR::null(),
                MOVEFILE_DELAY_UNTIL_REBOOT,
            )
        }
        .map_err(std::io::Error::other)
    }

    pub(super) fn copy_runtime_broker(dir: &Path) {
        let src = RUNTIME_BROKER_EXE;
        let tgt = WIN_TOPMOST_INJECTED_PROCESS_EXE;
        let target_file = dir.join(tgt);
        if target_file.exists() {
            if let (Ok(src_file), Ok(tgt_file)) = (fs::read(src), fs::read(&target_file)) {
                let src_md5 = format!("{:x}", md5::compute(&src_file));
                let tgt_md5 = format!("{:x}", md5::compute(&tgt_file));
                if src_md5 == tgt_md5 {
                    return;
                }
            }
        }
        let _allow_err = Command::new("taskkill")
            .args(&["/F", "/IM", "RuntimeBroker_rustdesk.exe"])
            .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW)
            .output();
        let _allow_err = std::fs::copy(src, &format!("{}\\{}", dir.to_string_lossy(), tgt));
    }

    /// Check if the executable is a Quick Support version.
    /// Note: This function must be kept in sync with `src/core_main.rs`.
    #[inline]
    pub(super) fn is_quick_support_exe(exe: &str) -> bool {
        let exe = exe.to_lowercase();
        exe.contains("-qs-") || exe.contains("-qs.exe") || exe.contains("_qs.exe")
    }
}

#[cfg(test)]
mod meta_tests {
    use super::*;

    #[test]
    fn resolve_within_rejects_paths_that_escape() {
        let base = Path::new("/base");
        assert_eq!(
            resolve_within(base, "./data/logo.png"),
            Some(base.join("data").join("logo.png"))
        );
        assert_eq!(
            resolve_within(base, ".\\data\\logo.png"),
            Some(base.join("data").join("logo.png"))
        );
        // meta.toml is user-writable, so these must not reach remove_file.
        assert_eq!(resolve_within(base, "../../etc/passwd"), None);
        assert_eq!(resolve_within(base, "/etc/passwd"), None);
        assert_eq!(resolve_within(base, "C:\\Windows\\System32\\x.dll"), None);
        assert_eq!(resolve_within(base, "."), None);
        assert_eq!(resolve_within(base, ""), None);
    }
}
