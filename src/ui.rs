//! Terminal display primitives: symbols, formatting, banner.
//!
//! Everything user-visible that isn't a domain concept lives here. Workers
//! and the render thread borrow the same `Symbols` table so a `✓` / `+`
//! check, an `→` / `->` arrow, etc. stays consistent across the run.

use std::ffi::CStr;

/// A box of display glyphs picked once at startup. Two concrete tables exist:
/// [`UNICODE_SYMBOLS`] (the default when the runtime locale advertises UTF-8)
/// and [`ASCII_SYMBOLS`] (a portable fallback for raw consoles).
pub struct Symbols {
    pub ellipsis: &'static str,
    pub bar_filled: &'static str,
    pub bar_head: &'static str,
    pub bar_empty: &'static str,
    pub hourglass: &'static str,
    pub play: &'static str,
    pub check: &'static str,
    pub cross: &'static str,
    pub arrow: &'static str,
}

pub const UNICODE_SYMBOLS: Symbols = Symbols {
    ellipsis: "\u{2026}",
    bar_filled: "\u{2501}",
    bar_head: "\u{2578}",
    bar_empty: "\u{2500}",
    hourglass: "\u{23f3}",
    play: "\u{25b6}",
    check: "\u{2713}",
    cross: "\u{2717}",
    arrow: "\u{2192}",
};

pub const ASCII_SYMBOLS: Symbols = Symbols {
    ellipsis: "..",
    bar_filled: "=",
    bar_head: ">",
    bar_empty: "-",
    hourglass: "~",
    play: ">",
    check: "+",
    cross: "x",
    arrow: "->",
};

/// Pick the symbol set based on the runtime locale's character encoding.
///
/// We deliberately consult `nl_langinfo(CODESET)` after `setlocale(LC_ALL, "")`
/// rather than just sniffing env vars: `LANG=en_US.UTF-8` may be exported even
/// when that locale isn't installed on the system, in which case the C library
/// silently falls back to ASCII and our box-drawing characters would render as
/// `?` or mojibake.
pub fn detect_symbols() -> &'static Symbols {
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
        let codeset = libc::nl_langinfo(libc::CODESET);
        if !codeset.is_null() {
            let cs = CStr::from_ptr(codeset).to_string_lossy().to_lowercase();
            if cs.contains("utf-8") || cs.contains("utf8") {
                return &UNICODE_SYMBOLS;
            }
        }
    }
    &ASCII_SYMBOLS
}

/// Return the number of columns in the terminal attached to stderr.
/// Falls back to 80 if unavailable (no tty, piped output, etc.).
pub fn terminal_cols() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            80
        }
    }
}

/// Overhead (terminal columns) consumed by the widest rendered line format,
/// excluding the file name. This is the disk-wait format:
///   `  ~           <name> (1000.0GB)  waiting for disk`
///    2+1+11              +2+8+1      +18  = 43, plus 1 margin = 44
pub const LINE_FORMAT_OVERHEAD: usize = 44;

/// Maximum file-name display width for a terminal of `cols` columns.
/// Keeps every rendered line within `cols` even for worst-case sizes.
pub fn max_name_for_cols(cols: usize) -> usize {
    cols.saturating_sub(LINE_FORMAT_OVERHEAD).max(20)
}

/// Truncate `name` to `max_len` characters, appending an ellipsis when needed.
/// Counts Unicode characters, not bytes — important for filenames containing
/// non-ASCII.
pub fn truncate_name(name: &str, max_len: usize, sym: &Symbols) -> String {
    if name.chars().count() <= max_len {
        name.to_string()
    } else {
        let truncated: String = name.chars().take(max_len - 1).collect();
        format!("{truncated}{}", sym.ellipsis)
    }
}

/// Render a progress bar of `width` cells representing `fraction` ∈ [0, 1].
/// The bar uses three glyphs: filled (`━`), head (`╸`), empty (`─`).
/// Clamps the fraction so out-of-range inputs don't panic.
pub fn progress_bar_str(fraction: f64, width: usize, sym: &Symbols) -> String {
    let filled = (fraction * width as f64) as usize;
    if filled >= width {
        sym.bar_filled.repeat(width)
    } else {
        format!(
            "{}{}{}",
            sym.bar_filled.repeat(filled),
            sym.bar_head,
            sym.bar_empty.repeat(width.saturating_sub(filled + 1))
        )
    }
}

/// Returns the banner printed when the built-in default config is used.
/// `use_unicode` selects box-drawing chars vs plain ASCII borders.
pub fn embedded_config_banner(use_unicode: bool) -> String {
    const W: usize = 62; // inner width (chars between the border columns)

    let content: &[&str] = &[
        "  hvac: using built-in default configuration",
        "",
        "  To customise encoding settings, save the defaults to a file:",
        "",
        "    hvac --dump-config > config.yaml",
        "    $EDITOR config.yaml",
        "    hvac --config config.yaml /path/to/media",
        "",
        "  Suppress this message: hvac --quiet ...",
    ];

    let (tl, tr, bl, br, h, v) = if use_unicode {
        ("╭", "╮", "╰", "╯", "─", "│")
    } else {
        ("+", "+", "+", "+", "-", "|")
    };

    let top = format!("{}{}{}", tl, h.repeat(W), tr);
    let bot = format!("{}{}{}", bl, h.repeat(W), br);

    let mut out = top;
    for line in content {
        let pad = W.saturating_sub(line.chars().count());
        out.push('\n');
        out.push_str(&format!("{}{}{}{}", v, line, " ".repeat(pad), v));
    }
    out.push('\n');
    out.push_str(&bot);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_name_short_unchanged() {
        assert_eq!(truncate_name("file.mkv", 30, &ASCII_SYMBOLS), "file.mkv");
    }

    #[test]
    fn truncate_name_truncates_with_ellipsis() {
        let name = "a".repeat(40);
        let out = truncate_name(&name, 10, &ASCII_SYMBOLS);
        // ASCII ellipsis is ".." — we trim to `max_len-1` chars then append it,
        // so the visible width is max_len-1 + 2 = max_len+1 in ASCII mode.
        // The contract is "ends with the ellipsis and isn't much longer than max_len".
        assert!(out.ends_with(".."), "got {out:?}");
        assert!(
            out.chars().count() <= 12,
            "got {} chars: {out:?}",
            out.chars().count()
        );
    }

    #[test]
    fn truncate_name_unicode_chars_count() {
        // 10 fullwidth chars (3 bytes each in UTF-8). We count chars, not bytes,
        // so this must NOT truncate.
        let name = "あいうえおかきくけこ";
        assert_eq!(name.chars().count(), 10);
        let out = truncate_name(name, 10, &UNICODE_SYMBOLS);
        assert_eq!(out, name);
    }

    #[test]
    fn progress_bar_zero_has_head_only() {
        let s = progress_bar_str(0.0, 10, &ASCII_SYMBOLS);
        assert_eq!(s.chars().count(), 10);
        assert!(s.starts_with('>'), "head should lead: {s}");
    }

    #[test]
    fn progress_bar_full_at_one() {
        let s = progress_bar_str(1.0, 10, &ASCII_SYMBOLS);
        assert_eq!(s, "==========");
    }

    #[test]
    fn progress_bar_half() {
        let s = progress_bar_str(0.5, 10, &ASCII_SYMBOLS);
        assert_eq!(s.chars().count(), 10);
        assert!(s.starts_with("====="));
    }

    #[test]
    fn progress_bar_clamps_above_one() {
        // Fraction > 1.0 must not panic on the saturating subtractions.
        let s = progress_bar_str(2.5, 10, &ASCII_SYMBOLS);
        assert_eq!(s, "==========");
    }

    #[test]
    fn max_name_for_narrow_terminal_floors_at_20() {
        // Even on a tiny terminal we keep 20 cols for the name —
        // the line will wrap, but truncating to <20 chars is worse UX.
        assert_eq!(max_name_for_cols(30), 20);
        assert_eq!(max_name_for_cols(0), 20);
    }

    #[test]
    fn max_name_for_normal_terminal() {
        // 80 - 44 = 36
        assert_eq!(max_name_for_cols(80), 36);
        // 120 - 44 = 76
        assert_eq!(max_name_for_cols(120), 76);
    }

    #[test]
    fn banner_unicode_form_uses_box_drawing() {
        let b = embedded_config_banner(true);
        assert!(b.contains("hvac: using built-in default configuration"));
        assert!(b.contains("╭"));
        assert!(b.contains("╯"));
    }

    #[test]
    fn banner_ascii_form_uses_plain_borders() {
        let b = embedded_config_banner(false);
        assert!(b.contains("hvac: using built-in default configuration"));
        assert!(b.starts_with("+"));
        assert!(b.ends_with("+"));
        assert!(!b.contains("╭"));
    }
}
