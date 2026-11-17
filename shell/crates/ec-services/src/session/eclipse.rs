// SPDX-License-Identifier: AGPL-3.0-only
//! Session control on Eclipse OS, which has no logind and no polkit.
//!
//! Eclipse OS runs `eclipse-init` as PID 1 and the Abyss session as its
//! `abyss.service` (`type = respawn`). Each action goes to the part of the
//! system that already owns it, and adds no authority: the kernel's `kill(2)`
//! permission check is the whole gate, exactly as it is for busybox `reboot`
//! typed in a terminal.
//!
//! - Reboot and power off: the signal `eclipse-init` answers, the same one
//!   busybox `reboot`/`poweroff` send (halt.c): `SIGTERM` reboots, `SIGUSR2`
//!   powers off. init then stops the services and calls `reboot(2)` itself.
//! - Log out: `SIGTERM` to the compositor this process is a client of, found
//!   through `SO_PEERCRED` on its Wayland socket. init starts a fresh session.
//! - Lock: Eclipse OS's own lock screen, `lunarlock`, when it is installed.
//! - Suspend and hibernate: refused. The kernel advertises `/sys/power/state`
//!   but does not sleep, so a button would do nothing.
//!
//! Each availability is asked of the same thing the action uses (`kill(pid, 0)`
//! for a signal, a lookup for a binary), so the menu offers only what will work.

use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Action, Availability};

/// `ID=` in `/etc/os-release` on Eclipse OS (xtask `os_release.rs`).
const OS_ID: &str = "eclipse";
const OS_RELEASE: [&str; 2] = ["/etc/os-release", "/usr/lib/os-release"];
/// Eclipse OS's lock screen (tools/lunarbar `lunarlock`).
const LOCKER: &str = "lunarlock";
/// Where the image installs its binaries, searched after `PATH`: the bar is a
/// service and may have a minimal `PATH`.
const BIN_DIRS: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];
const INIT_PID: libc::pid_t = 1;

/// Whether `/etc/os-release` (or its fallback) says this is Eclipse OS.
pub(super) fn is_eclipse_os() -> bool {
    OS_RELEASE
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|body| os_id(&body).as_deref() == Some(OS_ID))
}

/// The `ID` field of an os-release body, unquoted (os-release(5)).
fn os_id(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let value = line.trim().strip_prefix("ID=")?;
        Some(value.trim_matches(|c| c == '"' || c == '\'').to_owned())
    })
}

/// The signal `eclipse-init` turns into this action, for the two it does.
fn init_signal(action: Action) -> Option<libc::c_int> {
    match action {
        Action::Reboot => Some(libc::SIGTERM),
        Action::PowerOff => Some(libc::SIGUSR2),
        _ => None,
    }
}

/// Whether the kernel lets this process signal `pid` (`kill(pid, 0)`).
fn may_signal(pid: libc::pid_t) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

fn signal(pid: libc::pid_t, sig: libc::c_int) -> Result<(), String> {
    if pid <= 0 {
        return Err(format!("refusing to signal pid {pid}"));
    }
    if unsafe { libc::kill(pid, sig) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

/// The Wayland socket this process was started for.
fn wayland_socket(display: Option<OsString>, runtime: Option<OsString>) -> Option<PathBuf> {
    let display = PathBuf::from(display.filter(|d| !d.is_empty())?);
    if display.is_absolute() {
        return Some(display);
    }
    Some(PathBuf::from(runtime.filter(|r| !r.is_empty())?).join(display))
}

/// The pid of the process listening on `socket`, from `SO_PEERCRED`.
fn listener_pid(socket: &Path) -> Option<libc::pid_t> {
    use std::os::fd::AsRawFd;
    let stream = UnixStream::connect(socket).ok()?;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.pid)
}

/// The compositor to end for a log out. Never init, never this process.
fn compositor_pid() -> Option<libc::pid_t> {
    let socket = wayland_socket(
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let pid = listener_pid(&socket)?;
    let own = std::process::id() as libc::pid_t;
    (pid > INIT_PID && pid != own).then_some(pid)
}

/// `name` in `PATH`, then in the image's binary directories.
fn find_bin(name: &str, path: Option<OsString>) -> Option<PathBuf> {
    let from_path = path
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .into_iter()
        .chain(BIN_DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn locker() -> Option<PathBuf> {
    find_bin(LOCKER, std::env::var_os("PATH"))
}

fn yes_if(ok: bool) -> Availability {
    if ok {
        Availability::Yes
    } else {
        Availability::No
    }
}

/// Whether this action will work here.
pub(super) fn availability(action: Action) -> Availability {
    match action {
        Action::Lock => yes_if(locker().is_some()),
        Action::LogOut => yes_if(compositor_pid().is_some_and(may_signal)),
        Action::Suspend | Action::Hibernate => Availability::No,
        Action::Reboot | Action::PowerOff => yes_if(may_signal(INIT_PID)),
    }
}

/// Do it. The error is the system's own answer, worded for the menu.
pub(super) fn perform(action: Action) -> Result<(), String> {
    if let Some(sig) = init_signal(action) {
        return signal(INIT_PID, sig);
    }
    match action {
        Action::Lock => {
            let bin = locker().ok_or_else(|| format!("{LOCKER} is not installed"))?;
            let mut child = Command::new(bin)
                .stdin(Stdio::null())
                .spawn()
                .map_err(|e| format!("{LOCKER}: {e}"))?;
            // Reap it whenever it ends, so a lock never leaves a zombie.
            let reaper = std::thread::Builder::new()
                .name("eclipse-lock-reaper".into())
                .spawn(move || child.wait());
            reaper.map(drop).map_err(|e| e.to_string())
        }
        Action::LogOut => {
            let pid = compositor_pid().ok_or("cannot find this session's compositor")?;
            signal(pid, libc::SIGTERM)
        }
        Action::Suspend | Action::Hibernate => Err("Eclipse OS cannot suspend this machine yet".into()),
        Action::Reboot | Action::PowerOff => unreachable!("handled by init_signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_id_is_read_quoted_or_not() {
        assert_eq!(
            os_id("NAME=\"Eclipse OS\"\nID=\"eclipse\"\n").as_deref(),
            Some("eclipse")
        );
        assert_eq!(os_id("ID=eclipse\n").as_deref(), Some("eclipse"));
        assert_eq!(os_id("ID_LIKE=eclipse\nID=arch\n").as_deref(), Some("arch"));
        assert_eq!(os_id("NAME=x\n"), None);
    }

    /// The mapping is eclipse-init's SIGNAL_HANDLERS table: SIGTERM reboots,
    /// SIGUSR2 powers off. A swapped row powers the machine off on Reboot.
    #[test]
    fn init_gets_the_signal_busybox_would_send() {
        assert_eq!(init_signal(Action::Reboot), Some(libc::SIGTERM));
        assert_eq!(init_signal(Action::PowerOff), Some(libc::SIGUSR2));
        for other in [Action::Lock, Action::LogOut, Action::Suspend, Action::Hibernate] {
            assert_eq!(init_signal(other), None, "{other:?}");
        }
    }

    #[test]
    fn suspend_is_never_offered() {
        assert_eq!(availability(Action::Suspend), Availability::No);
        assert_eq!(availability(Action::Hibernate), Availability::No);
    }

    #[test]
    fn the_wayland_socket_follows_the_environment() {
        let s = |v: &str| Some(OsString::from(v));
        assert_eq!(
            wayland_socket(s("wayland-1"), s("/run/user/0")),
            Some(PathBuf::from("/run/user/0/wayland-1"))
        );
        assert_eq!(wayland_socket(s("/tmp/w"), None), Some(PathBuf::from("/tmp/w")));
        assert_eq!(wayland_socket(s("wayland-1"), None), None);
        assert_eq!(wayland_socket(s(""), s("/run")), None);
        assert_eq!(wayland_socket(None, s("/run")), None);
    }

    #[test]
    fn signalling_nothing_is_refused() {
        assert!(!may_signal(0));
        assert!(!may_signal(-1));
        assert!(signal(0, libc::SIGTERM).is_err());
        assert!(signal(-1, libc::SIGTERM).is_err());
    }

    #[test]
    fn the_listener_is_found_by_peer_credentials() {
        let dir = std::env::temp_dir().join(format!("ec-services-peer-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("wayland-test");
        let _ = std::fs::remove_file(&sock);
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert_eq!(listener_pid(&sock), Some(std::process::id() as libc::pid_t));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binary_is_found_on_path_first() {
        let dir = std::env::temp_dir().join(format!("ec-services-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("ec-test-locker");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_bin("ec-test-locker", Some(dir.clone().into())),
            Some(bin.clone())
        );
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(find_bin("ec-test-locker", Some(dir.clone().into())), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
