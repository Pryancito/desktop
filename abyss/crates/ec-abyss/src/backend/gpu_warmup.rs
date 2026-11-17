// SPDX-License-Identifier: AGPL-3.0-only
//! GPU warm-up before the first DRM start of a boot (`misc.gpu-warmup`).
//!
//! Some drivers come up broken for the first process of a boot that renders
//! and scans out on the GPU, and work for every process after it. The case
//! that motivated this is Eclipse OS on NVIDIA through zink + NVK on the
//! nouveau uAPI: the first start creates its EGL context and its scanout, the
//! panel shows noise, and a second start of the same binary draws correctly.
//!
//! So, once per boot, the compositor first runs a short-lived copy of itself
//! (`--gpu-warmup`). The copy takes the seat, opens the GPU, creates its
//! renderer and scanout, waits for its first page flip, and exits. The driver's
//! first-use state then belongs to a process that is gone, and the real
//! compositor starts after it as the second GPU process of the boot.
//!
//! The copy runs the normal DRM bring-up ([`super::drm::run`]) with every
//! client-facing part off: no IPC socket, no trusted socket, no Xwayland, no
//! add-ons, no session import, no kiosk client. It does listen on a Wayland
//! socket, which no one is told about, and drops it on exit.
//!
//! The once-per-boot marker lives in `$XDG_RUNTIME_DIR` (or `/run`), both of
//! which are emptied at boot. It is written before the copy starts, so a copy
//! that hangs or crashes is never started again in the same boot.

use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use smithay::reexports::calloop::{
    timer::{TimeoutAction, Timer},
    LoopHandle,
};

use crate::state::AbyssState;

/// The flag that makes this process the warm-up copy.
pub const FLAG: &str = "--gpu-warmup";

/// The copy exits this long after its first page flip, so the frame is on the
/// panel and the driver has finished with it.
const AFTER_FLIP: Duration = Duration::from_secs(1);
/// The copy gives up on its own after this long without a flip.
const CHILD_LIMIT: Duration = Duration::from_secs(15);
/// The parent kills the copy after this long, whatever it is doing.
const PARENT_LIMIT: Duration = Duration::from_secs(25);
const POLL: Duration = Duration::from_millis(100);

static CHILD: AtomicBool = AtomicBool::new(false);
static FLIPPED: AtomicBool = AtomicBool::new(false);

/// Mark this process as the warm-up copy. Called once from argument parsing.
pub fn set_child() {
    CHILD.store(true, Ordering::Relaxed);
}

/// Whether this process is the warm-up copy.
pub fn is_child() -> bool {
    CHILD.load(Ordering::Relaxed)
}

/// The once-per-boot marker.
fn marker() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| PathBuf::from("/run"), PathBuf::from);
    dir.join("ec-abyss-gpu-warm")
}

/// The copy's argv: this process's own flags up to any kiosk client, without
/// the ones that would make it talk to the session (`--session`, `--kiosk`).
fn child_args(own: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = own
        .into_iter()
        .take_while(|a| a != "--")
        .filter(|a| a != "--session" && a != "--kiosk" && a != "--stats")
        .collect();
    out.push(FLAG.to_owned());
    out
}

/// Run the warm-up copy once per boot, before this process takes the seat.
/// Any failure is logged and the caller starts normally.
pub fn prime() {
    if is_child() {
        return;
    }
    let marker = marker();
    if marker.exists() {
        tracing::debug!(marker = %marker.display(), "GPU already warmed up this boot");
        return;
    }
    // Written first: a copy that hangs or crashes is not retried this boot.
    if let Err(err) = std::fs::write(&marker, b"") {
        tracing::warn!(%err, marker = %marker.display(), "GPU warm-up skipped: cannot write its marker");
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            tracing::warn!(%err, "GPU warm-up skipped: cannot resolve the compositor binary");
            return;
        }
    };
    let args = child_args(std::env::args().skip(1));
    let started = Instant::now();
    let mut child = match Command::new(&exe).args(&args).spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(%err, "GPU warm-up skipped: spawning the warm-up copy failed");
            return;
        }
    };
    tracing::info!(
        pid = child.id(),
        "GPU warm-up: first GPU start of this boot runs in a short-lived copy"
    );
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::info!(%status, ms = started.elapsed().as_millis() as u64, "GPU warm-up finished");
                break;
            }
            Ok(None) if started.elapsed() >= PARENT_LIMIT => {
                // By pid, the copy's own: never by name (root CLAUDE.md).
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    secs = PARENT_LIMIT.as_secs(),
                    "GPU warm-up copy did not finish; killed it"
                );
                break;
            }
            Ok(None) => std::thread::sleep(POLL),
            // `SA_NOCLDWAIT` (main.rs) reaps the copy itself, so once it has
            // exited there is nothing left to wait for and this is ECHILD.
            Err(_) => {
                tracing::info!(ms = started.elapsed().as_millis() as u64, "GPU warm-up finished");
                break;
            }
        }
    }
}

/// In the copy: give up after [`CHILD_LIMIT`] if no frame ever flips.
pub fn arm_limit(handle: &LoopHandle<'static, AbyssState>) {
    if !is_child() {
        return;
    }
    let armed = handle.insert_source(Timer::from_duration(CHILD_LIMIT), |_, _, state| {
        tracing::warn!(
            secs = CHILD_LIMIT.as_secs(),
            "GPU warm-up copy saw no page flip; exiting"
        );
        state.loop_signal.stop();
        TimeoutAction::Drop
    });
    if armed.is_err() {
        tracing::warn!("GPU warm-up copy could not arm its time limit");
    }
}

/// In the copy: the first page flip landed. Exit [`AFTER_FLIP`] later.
pub fn flipped(state: &mut AbyssState) {
    if !is_child() || FLIPPED.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::info!("GPU warm-up copy: first page flip");
    let armed = state
        .loop_handle
        .insert_source(Timer::from_duration(AFTER_FLIP), |_, _, state| {
            state.loop_signal.stop();
            TimeoutAction::Drop
        });
    if armed.is_err() {
        state.loop_signal.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn the_copy_keeps_config_flags_and_drops_session_ones() {
        let got = child_args(s(&[
            "--backend",
            "drm",
            "--session",
            "--config",
            "/etc/x.kdl",
            "--stats",
        ]));
        assert_eq!(got, s(&["--backend", "drm", "--config", "/etc/x.kdl", FLAG]));
    }

    #[test]
    fn the_copy_never_starts_a_kiosk_client() {
        let got = child_args(s(&["--backend", "drm", "--kiosk", "--", "greeter", "--session"]));
        assert_eq!(got, s(&["--backend", "drm", FLAG]));
    }

    #[test]
    fn the_marker_follows_the_runtime_dir() {
        assert!(marker().ends_with("ec-abyss-gpu-warm"));
    }
}
