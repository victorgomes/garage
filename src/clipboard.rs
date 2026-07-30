//! Clipboard (TODO 5.2) and export (TODO 5.3).
//!
//! Copy strategy per PLAN §7.9: over SSH or inside tmux, the only thing that
//! can reach the *local* clipboard is OSC 52 — an escape sequence the
//! terminal emulator itself interprets — so that is tried there. Locally,
//! `arboard` talks to the OS clipboard directly. OSC 52 gives no success
//! signal, which is why the choice is made up front from the environment
//! instead of "try and fall back".

use std::io::Write;

use anyhow::{Context, Result};
use base64_engine as b64;

/// Hand-rolled standard base64: the payload of OSC 52. Ten lines beat a
/// dependency for exactly one call site.
mod base64_engine {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
            out.push(TABLE[(n >> 18 & 63) as usize] as char);
            out.push(TABLE[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }
}

/// Some terminals silently drop oversized OSC 52 payloads (tmux's default cap
/// is 100 KB *encoded*); past this, copying is refused with a message rather
/// than silently truncated.
pub const MAX_COPY: usize = 72 * 1024;

/// Is this session one where only OSC 52 can possibly reach the user's
/// clipboard? **SSH only.** tmux alone is not remote: a local tmux session
/// still has direct access to the OS clipboard, and OSC 52 there depends on
/// the *outer* terminal supporting it — Terminal.app, for one, does not, so
/// routing local-tmux copies through OSC 52 silently lost them (user
/// report).
fn remote() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

/// Copies text to the clipboard. Returns a short description of the route
/// taken, for the status line.
pub fn copy(text: &str) -> Result<&'static str> {
    if text.len() > MAX_COPY {
        anyhow::bail!(
            "selection is {} KB; the clipboard path tops out at {} KB — use export instead",
            text.len() / 1024,
            MAX_COPY / 1024
        );
    }

    if remote() {
        osc52(text)?;
        return Ok("copied (OSC 52)");
    }

    // Local (tmux included): the OS clipboard is authoritative; OSC 52 is
    // the fallback for environments arboard cannot reach.
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())) {
        Ok(()) => Ok("copied"),
        Err(e) => {
            tracing::debug!("arboard failed ({e}); falling back to OSC 52");
            osc52(text)?;
            Ok("copied (OSC 52)")
        }
    }
}

/// Emits the OSC 52 sequence on the terminal, written to the same screen
/// handle the TUI draws on.
///
/// Inside tmux the *plain* sequence is the right default: tmux's own
/// `set-clipboard external` (on by default) interprets it, sets the paste
/// buffer, and forwards to the outer terminal. The DCS passthrough wrapper is
/// only correct when `allow-passthrough` is enabled — and that option
/// *defaults to off* since tmux 3.3, where a wrapped sequence is silently
/// discarded. So passthrough is used only when tmux confirms it is on.
fn osc52(text: &str) -> Result<()> {
    let payload = b64::encode(text.as_bytes());
    let mut screen = crate::tty::Screen::open().context("no terminal for OSC 52")?;

    if std::env::var_os("TMUX").is_some() && tmux_allows_passthrough() {
        let inner = format!("\x1b]52;c;{payload}\x07");
        let wrapped = format!("\x1bPtmux;{}\x1b\\", inner.replace('\x1b', "\x1b\x1b"));
        screen.write_all(wrapped.as_bytes())?;
    } else {
        write!(screen, "\x1b]52;c;{payload}\x07")?;
    }
    screen.flush()?;
    Ok(())
}

fn tmux_allows_passthrough() -> bool {
    std::process::Command::new("tmux")
        .args(["show", "-Ap", "allow-passthrough"])
        .output()
        .map(|out| {
            let text = String::from_utf8_lossy(&out.stdout);
            text.contains(" on") || text.contains("all")
        })
        .unwrap_or(false)
}

/// Writes the current view as Markdown (TODO 5.3): a fenced block with a
/// one-line provenance header, ready to paste into a bug or Gerrit comment.
pub fn export(path: &std::path::Path, title: &str, lines: &[String]) -> Result<()> {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum::<usize>() + 128);
    out.push_str(&format!("### {title}\n\n```\n"));
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("```\n");
    std::fs::write(path, out).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc() {
        assert_eq!(b64::encode(b""), "");
        assert_eq!(b64::encode(b"f"), "Zg==");
        assert_eq!(b64::encode(b"fo"), "Zm8=");
        assert_eq!(b64::encode(b"foo"), "Zm9v");
        assert_eq!(b64::encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64::encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64::encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn export_writes_fenced_markdown() {
        let dir = std::env::temp_dir().join(format!("garage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("export.md");
        export(
            &path,
            "add · Maglev #1 · Register allocation",
            &["  9/9: CheckedSmiUntag".to_string()],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("### add · Maglev #1"));
        assert!(text.contains("```\n  9/9: CheckedSmiUntag\n```"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
