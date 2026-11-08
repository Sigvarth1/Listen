//! Per-OS helpers for the crates that start helper programs (mpv, yt-dlp, a JS runtime for yt-dlp).
//! Linux: from `PATH`. Windows: next to `listen.exe`, on `PATH` or in `%LOCALAPPDATA%\Listen\bin`.

use std::path::PathBuf;

/// `CREATE_NO_WINDOW`: a helper started from the GUI must not flash a console window.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keeps `cmd` from opening a console window (Windows; no-op elsewhere).
/// For a tokio command pass `cmd.as_std_mut()`.
pub fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// The directory of the running executable.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(PathBuf::from)
}

/// The user's home directory (`HOME`, `USERPROFILE` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Where the Windows installer / a manual setup may keep helper programs:
/// `%LOCALAPPDATA%\Listen\bin`.
pub fn local_bin_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("Listen").join("bin"))
}

/// File name of the helper program `name` on this OS (`mpv` -> `mpv.exe` on Windows).
pub fn exe_name(name: &str) -> String {
    if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The helper program `name` to run. Windows: next to `listen.exe`, then `PATH`, then
/// `%LOCALAPPDATA%\Listen\bin`, else the bare name (so the error names it). Elsewhere the bare name.
pub fn tool(name: &str) -> PathBuf {
    if !cfg!(windows) {
        return PathBuf::from(name);
    }
    find_tool(name, exe_dir(), std::env::var_os("PATH"), extra_tool_dirs())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Whether [`tool`] finds `name` (Windows; macOS: on `PATH`, which includes Homebrew's folder;
/// elsewhere the helpers come from the package manager and are looked up on `PATH` when started).
pub fn tool_installed(name: &str) -> bool {
    if cfg!(target_os = "macos") {
        return find_tool(name, None, std::env::var_os("PATH"), Vec::new()).is_some();
    }
    !cfg!(windows) || find_tool(name, exe_dir(), std::env::var_os("PATH"), extra_tool_dirs()).is_some()
}

/// Where Windows helpers end up besides `PATH`: Listen's own `bin`, winget's
/// links for portable packages (on `PATH` only for programs started after the
/// install) and the folder of winget's mpv package.
pub fn extra_tool_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = local_bin_dir().into_iter().collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(local).join("Microsoft").join("WinGet").join("Links"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(pf).join("MPV Player"));
    }
    dirs
}

/// Where macOS package managers put programs. An app started from Finder or the Dock gets only
/// `/usr/bin:/bin:/usr/sbin:/sbin` as `PATH`, so mpv and yt-dlp from Homebrew would not be found.
#[cfg(target_os = "macos")]
pub fn macos_tool_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].into_iter().map(PathBuf::from).collect();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".deno").join("bin"));
        dirs.push(home.join(".local").join("bin"));
    }
    dirs
}

/// macOS: appends the package-manager folders that exist to `PATH`, so every helper program
/// (mpv, yt-dlp, Deno, ffmpeg) is found however Listen was started.
///
/// # Safety
/// Changes the environment: call it before any other thread starts.
#[cfg(target_os = "macos")]
pub unsafe fn macos_extend_path() {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&current).collect();
    for dir in macos_tool_dirs() {
        if dir.is_dir() && !paths.contains(&dir) {
            paths.push(dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        // SAFETY: the caller guarantees no other thread reads the environment yet.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

/// Helper processes (mpv) that must not outlive Listen. On macOS "Quit" (Cmd+Q, the Dock, logging
/// out) ends the process with `exit()` right after the event loop's last event, and a signal ends
/// it without unwinding, so the handles that would kill them on drop never run;
/// [`install_exit_cleanup`] ends the ones still listed. Lock-free slots, so a signal handler can
/// read them.
#[cfg(unix)]
static CHILDREN: [std::sync::atomic::AtomicU32; 16] = [const { std::sync::atomic::AtomicU32::new(0) }; 16];

/// Lists a helper process to end when Listen exits.
pub fn track_child(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.filter(|p| *p != 0) {
        use std::sync::atomic::Ordering;
        let free = CHILDREN
            .iter()
            .find(|slot| slot.compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst).is_ok());
        if free.is_none() {
            tracing::debug!("helper process {pid} not tracked: all slots in use");
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// The helper processes Listen runs now (e.g. for the macOS visualizer's audio tap).
pub fn tracked_children() -> Vec<u32> {
    #[cfg(unix)]
    {
        CHILDREN
            .iter()
            .map(|slot| slot.load(std::sync::atomic::Ordering::SeqCst))
            .filter(|p| *p != 0)
            .collect()
    }
    #[cfg(not(unix))]
    Vec::new()
}

/// Takes a helper process off the list (it was ended or waited for).
pub fn untrack_child(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.filter(|p| *p != 0) {
        use std::sync::atomic::Ordering;
        for slot in &CHILDREN {
            let _ = slot.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
mod exit_cleanup {
    use super::CHILDREN;
    use std::sync::atomic::Ordering;

    unsafe extern "C" {
        fn atexit(callback: extern "C" fn()) -> i32;
        fn kill(pid: i32, signal: i32) -> i32;
        fn signal(signum: i32, handler: usize) -> usize;
        fn raise(signum: i32) -> i32;
    }
    const SIGHUP: i32 = 1;
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    const SIG_DFL: usize = 0;

    /// Only atomics and `kill`: safe in a signal handler.
    fn end_children() {
        for slot in &CHILDREN {
            let pid = slot.swap(0, Ordering::SeqCst);
            if pid != 0 {
                // SAFETY: a plain signal to a process Listen started.
                unsafe { kill(pid as i32, SIGTERM) };
            }
        }
    }

    extern "C" fn at_exit() {
        end_children();
    }

    /// Ends the children, then dies of the same signal as before.
    extern "C" fn on_signal(signum: i32) {
        end_children();
        // SAFETY: async-signal-safe calls restoring and re-raising the default action.
        unsafe {
            signal(signum, SIG_DFL);
            raise(signum);
        }
    }

    pub fn install() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        // SAFETY: the handlers only touch atomics and send signals.
        ONCE.call_once(|| unsafe {
            atexit(at_exit);
            for signum in [SIGHUP, SIGINT, SIGTERM] {
                signal(signum, on_signal as extern "C" fn(i32) as usize);
            }
        });
    }
}

/// Ends the listed helper processes however Listen exits: `exit()` (macOS "Quit") or
/// SIGTERM / SIGHUP / SIGINT. Call once at start-up.
pub fn install_exit_cleanup() {
    #[cfg(unix)]
    exit_cleanup::install();
}

/// [`tool`]'s search with the locations passed in (testable).
pub fn find_tool(
    name: &str,
    exe_dir: Option<PathBuf>,
    path_var: Option<std::ffi::OsString>,
    extra: Vec<PathBuf>,
) -> Option<PathBuf> {
    let file = exe_name(name);
    let from_path = path_var.into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>());
    exe_dir.into_iter().chain(from_path).chain(extra).map(|d| d.join(&file)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_dir_wins_over_path_and_local_bin() {
        let root = std::env::temp_dir().join(format!("listen-platform-{}", std::process::id()));
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        let file = exe_name("tool-x");
        std::fs::write(b.join(&file), "").unwrap();
        std::fs::write(c.join(&file), "").unwrap();
        let path = std::env::join_paths([&b]).unwrap();
        assert_eq!(
            find_tool("tool-x", Some(a.clone()), Some(path.clone()), vec![c.clone()]),
            Some(b.join(&file))
        );
        std::fs::write(a.join(&file), "").unwrap();
        assert_eq!(find_tool("tool-x", Some(a.clone()), Some(path), vec![c.clone()]), Some(a.join(&file)));
        assert_eq!(find_tool("tool-x", None, None, vec![c.clone()]), Some(c.join(&file)));
        assert_eq!(find_tool("missing-tool", Some(a), None, vec![]), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exe_suffix_only_on_windows() {
        assert_eq!(exe_name("mpv"), if cfg!(windows) { "mpv.exe" } else { "mpv" });
        assert_eq!(exe_name("a.exe"), "a.exe");
    }
}
