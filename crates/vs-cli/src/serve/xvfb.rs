//! A private X display for page windows on Linux.
//!
//! On the user's own display, page windows showed on their screen and
//! trusted input moved their pointer. A display too small for several
//! page tiles (any ordinary monitor) stacked the windows at one spot,
//! where X's single pointer and stacking order let pages take each
//! other's clicks and hover. When `Xvfb` is installed, the daemon runs
//! its pages on a display of its own instead, sized for
//! [`DEFAULT_SCREEN`] worth of tiles, and XTest input goes there, so
//! pure Wayland sessions get trusted input without portal consent too.
//!
//! `VS_DISPLAY=host` keeps the old behaviour; `VS_XVFB_SCREEN=WxH`
//! changes the size. Xvfb allocates its whole framebuffer up front and
//! cannot grow it, so the size is chosen once: the default (5x5 tiles,
//! as CI runs) costs about 150 MB over a minimal display.

use std::io::{BufRead as _, BufReader};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};

/// Room for 5x5 page tiles of 1600x1000, plus a margin.
const DEFAULT_SCREEN: &str = "8016x5016";

/// How long Xvfb may take to report its display number.
const START_BUDGET: Duration = Duration::from_secs(10);

/// A running Xvfb, stopped when dropped. A daemon killed outright does
/// not leave it behind either: it gets SIGTERM when the daemon dies.
pub struct PrivateDisplay {
    child: Child,
    /// The display name, e.g. `:3`.
    pub name: String,
    pub screen: String,
}

impl Drop for PrivateDisplay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start the private display unless the user asked for the host one or
/// Xvfb is missing. Every way of not starting it is logged, because the
/// daemon then behaves as before.
pub fn choose() -> Option<PrivateDisplay> {
    if std::env::var("VS_DISPLAY").as_deref() == Ok("host") {
        tracing::info!("VS_DISPLAY=host: page windows on the host display");
        return None;
    }
    let screen = std::env::var("VS_XVFB_SCREEN").unwrap_or_else(|_| DEFAULT_SCREEN.to_string());
    match start(&screen) {
        Ok(d) => {
            tracing::info!(display = %d.name, screen = %d.screen, "page windows on a private Xvfb display");
            Some(d)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "no private display (install Xvfb for headless page windows); using the host display"
            );
            None
        }
    }
}

fn start(screen: &str) -> Result<PrivateDisplay> {
    if !valid_screen(screen) {
        bail!("VS_XVFB_SCREEN={screen:?} is not WxH");
    }
    // `-displayfd 1`: Xvfb picks a free display itself and writes its
    // number to stdout once it accepts connections, so there is no
    // race over display numbers with other X servers.
    let mut command = Command::new("Xvfb");
    command
        .args(["-displayfd", "1", "-screen", "0"])
        .arg(format!("{screen}x24"))
        .args(["-nolisten", "tcp", "-terminate"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: prctl is async-signal-safe; nothing else runs between
    // fork and exec.
    unsafe {
        command.pre_exec(|| {
            // The kernel stops Xvfb when the daemon dies, however it
            // dies. `-terminate` alone was not enough: a `dbus-launch`
            // GTK autostarts holds a connection to the display.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().context("spawn Xvfb")?;
    let stdout = child.stdout.take().context("Xvfb stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let number = match rx.recv_timeout(START_BUDGET) {
        Ok(line) if !line.trim().is_empty() => line.trim().to_string(),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Xvfb did not report a display within {START_BUDGET:?}");
        }
    };
    Ok(PrivateDisplay {
        child,
        name: format!(":{number}"),
        screen: screen.to_string(),
    })
}

fn valid_screen(screen: &str) -> bool {
    screen.split_once('x').is_some_and(|(w, h)| {
        w.parse::<u16>().is_ok_and(|w| w > 0) && h.parse::<u16>().is_ok_and(|h| h > 0)
    })
}

/// Point GTK and the input path at `display`: X11 only, so a Wayland
/// session's compositor is not used for page windows or input.
///
/// Must run before GTK initializes and before any other thread starts.
pub fn use_display(display: &PrivateDisplay) {
    std::env::set_var("DISPLAY", &display.name);
    std::env::set_var("GDK_BACKEND", "x11");
    std::env::remove_var("WAYLAND_DISPLAY");
}

#[cfg(test)]
mod tests {
    use super::valid_screen;

    #[test]
    fn screen_sizes_are_checked() {
        assert!(valid_screen("8016x5016"));
        assert!(valid_screen("1280x800"));
        for bad in ["", "8016", "x5016", "0x10", "8016x", "big", "99999x10"] {
            assert!(!valid_screen(bad), "{bad}");
        }
    }
}
