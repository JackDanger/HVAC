//! Single-threaded progress renderer for Phase 3.
//!
//! Owns the cursor: every 200ms it moves up over the previous viewport, clears
//! to end-of-screen, drains any completed-line strings from the worker queue
//! (those go *above* the viewport, into scrollback), then repaints one line
//! per active worker plus a final progress bar.
//!
//! Also drives auto-ramp: while [`RenderCtx::ramping`] is true and the slots
//! report measurable speeds, the renderer bumps `max_encoders` up by one and
//! watches whether total throughput keeps improving. Hitting a stall or the
//! frozen flag turns ramping off permanently for the rest of the run.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::ui::{progress_bar_str, Symbols};

use super::worker::lower_max;
use super::{WorkerSlot, CANCELLED};

/// Render-thread state. Held by the spawn callback so each Arc is cloned once.
pub struct RenderCtx {
    pub slots: Vec<Arc<WorkerSlot>>,
    pub completed_lines: Arc<Mutex<Vec<String>>>,
    pub completed_units: Arc<AtomicU64>,
    pub transcoded: Arc<AtomicU32>,
    pub errors: Arc<AtomicU32>,
    pub max_encoders: Arc<AtomicU32>,
    pub ramping: Arc<AtomicBool>,
    pub session_limit_frozen: Arc<AtomicBool>,
    pub total_units: u64,
    pub file_count: u64,
    pub sym: &'static Symbols,
}

/// Run the render loop until all files are accounted for (or the run was
/// cancelled). Designed to be spawned in a `std::thread::scope`.
pub fn run_render(ctx: RenderCtx) {
    let start = Instant::now();
    let mut prev_viewport = 0usize;
    let mut ramp_baseline_speed = 0u64;
    let mut last_ramp_time = start;

    loop {
        paint_viewport(&ctx, &mut prev_viewport, start);

        // Auto-ramp: add workers while total throughput improves.
        if ctx.session_limit_frozen.load(Ordering::SeqCst) {
            ctx.ramping.store(false, Ordering::SeqCst);
        }
        if ctx.ramping.load(Ordering::SeqCst)
            && !ctx.session_limit_frozen.load(Ordering::SeqCst)
            && last_ramp_time.elapsed().as_secs() >= 5
        {
            try_ramp(&ctx, &mut ramp_baseline_speed, &mut last_ramp_time);
        }

        let completed = ctx.transcoded.load(Ordering::Relaxed);
        let errs = ctx.errors.load(Ordering::Relaxed);
        let cancelled = CANCELLED.load(Ordering::Relaxed);
        if (completed + errs) as u64 >= ctx.file_count || cancelled {
            finish_screen(&ctx, prev_viewport);
            return;
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

fn paint_viewport(ctx: &RenderCtx, prev_viewport: &mut usize, start: Instant) {
    let mut stderr = std::io::stderr().lock();

    // Cursor is on the last viewport line (no trailing \n). \r goes to col 0;
    // move up (prev_viewport-1) to reach the first viewport line, then erase
    // to end of screen. Omitting the trailing newline on the progress bar
    // prevents the viewport from scrolling into the scrollback buffer —
    // which is what causes ghost progress lines in terminal history.
    if *prev_viewport > 0 {
        write!(stderr, "\r").ok();
        if *prev_viewport > 1 {
            write!(stderr, "\x1b[{}A", *prev_viewport - 1).ok();
        }
    }
    write!(stderr, "\x1b[J").ok();

    // Drain completed lines into scrollback (above the viewport).
    {
        let mut lines = ctx.completed_lines.lock().unwrap();
        for line in lines.drain(..) {
            writeln!(stderr, "{}", line).ok();
        }
    }

    let mut viewport = 0usize;
    let max = ctx.max_encoders.load(Ordering::Relaxed);

    for (slot_idx, slot) in ctx.slots.iter().enumerate() {
        let info = slot.info.lock().unwrap();
        if let Some((ref name, ref size)) = *info {
            let is_excess = (slot_idx + 1) as u32 > max;
            if slot.disk_wait.load(Ordering::Relaxed) {
                writeln!(
                    stderr,
                    "  {}           {} ({})  waiting for disk",
                    ctx.sym.hourglass, name, size,
                )
                .ok();
            } else if slot.queued.load(Ordering::Relaxed) && is_excess {
                writeln!(
                    stderr,
                    "  {}           {} ({})  queued {}/{}",
                    ctx.sym.hourglass,
                    name,
                    size,
                    slot_idx + 1,
                    max
                )
                .ok();
            } else {
                let pct = slot.progress.load(Ordering::Relaxed) / 10;
                let spd = slot.speed.load(Ordering::Relaxed);
                let speed_str = if spd > 0 {
                    format!("{}.{}x", spd / 100, (spd % 100) / 10)
                } else {
                    String::new()
                };
                writeln!(
                    stderr,
                    "  {} {:>2}% {:>4} {} ({})",
                    ctx.sym.play, pct, speed_str, name, size
                )
                .ok();
            }
            viewport += 1;
        }
    }

    // Overall progress bar.
    let done_units = ctx.completed_units.load(Ordering::Relaxed);
    let active_sum: u64 = ctx
        .slots
        .iter()
        .map(|s| s.progress.load(Ordering::Relaxed))
        .sum();
    let current = (done_units + active_sum).min(ctx.total_units);
    let frac = if ctx.total_units > 0 {
        current as f64 / ctx.total_units as f64
    } else {
        0.0
    };

    let completed = ctx.transcoded.load(Ordering::Relaxed);
    let errs = ctx.errors.load(Ordering::Relaxed);
    let finished = completed + errs;
    let elapsed = start.elapsed().as_secs();

    // No trailing newline — keeps the cursor ON the progress bar so the
    // viewport never scrolls into scrollback.
    write!(
        stderr,
        "  {} {}/{} done  [{:02}:{:02}:{:02}]",
        progress_bar_str(frac, 40, ctx.sym),
        finished,
        ctx.file_count,
        elapsed / 3600,
        (elapsed % 3600) / 60,
        elapsed % 60
    )
    .ok();
    viewport += 1;

    stderr.flush().ok();
    *prev_viewport = viewport;
}

fn try_ramp(ctx: &RenderCtx, baseline: &mut u64, last_ramp: &mut Instant) {
    let current_max = ctx.max_encoders.load(Ordering::SeqCst);

    // Collect speeds from active (non-queued) workers.
    let speeds: Vec<u64> = ctx
        .slots
        .iter()
        .filter(|s| s.info.lock().unwrap().is_some() && !s.queued.load(Ordering::Relaxed))
        .map(|s| s.speed.load(Ordering::Relaxed))
        .collect();
    let reporting = speeds.iter().filter(|&&s| s > 0).count() as u32;
    let total_speed: u64 = speeds.iter().sum();

    // Wait until all current slots are encoding and reporting a speed.
    if reporting < current_max || total_speed == 0 {
        return;
    }

    // Two cases warrant another ramp: we've never measured (`baseline == 0`,
    // first observation), or total throughput went up since the last ramp.
    // Both update the baseline and bump max by one.
    let still_improving = *baseline == 0 || total_speed > *baseline;
    if still_improving {
        *baseline = total_speed;
        ctx.max_encoders.store(current_max + 1, Ordering::SeqCst);
        *last_ramp = Instant::now();
        return;
    }

    // Throughput stalled or dropped: stop ramping permanently.
    ctx.ramping.store(false, Ordering::SeqCst);
    if total_speed < *baseline * 85 / 100 {
        // Significant drop: revert the last ramp.
        lower_max(&ctx.max_encoders, current_max.saturating_sub(1).max(1));
    }
}

fn finish_screen(ctx: &RenderCtx, prev_viewport: usize) {
    let mut stderr = std::io::stderr().lock();
    if prev_viewport > 0 {
        write!(stderr, "\r").ok();
        if prev_viewport > 1 {
            write!(stderr, "\x1b[{}A", prev_viewport - 1).ok();
        }
        write!(stderr, "\x1b[J").ok();
    }
    write!(stderr, "\x1b[0m\r\n").ok();
    stderr.flush().ok();
    let _ = ctx; // silence unused-field warning when adding later metrics
}
