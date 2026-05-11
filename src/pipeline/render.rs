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

use crate::flags::Flags;
use crate::ui::{progress_bar_str, Symbols};

use super::worker::lower_max;
use super::{WorkerSlot, CANCELLED, LD_KILL};

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
    pub flags: Arc<Flags>,
}

/// Run the render loop until all files are accounted for (or the run was
/// cancelled). Designed to be spawned in a `std::thread::scope`.
pub fn run_render(ctx: RenderCtx) {
    let start = Instant::now();
    let mut prev_viewport = 0usize;
    let mut ramp_baseline_speed = 0u64;
    let mut last_ramp_time = start;
    let mut prev_flag_max = 0usize;

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

        // max-parallel-jobs flag: override max_encoders every tick when set.
        // When it clears back to 0, re-enable auto-ramp so the run can find
        // its own optimal concurrency again (unless frozen by session limits).
        let flag_max = ctx.flags.max_parallel_jobs();
        if flag_max > 0 {
            let max_slots = ctx.slots.len().max(1);
            let clamped = flag_max.clamp(1, max_slots) as u32;
            ctx.max_encoders.store(clamped, Ordering::SeqCst);
            ctx.ramping.store(false, Ordering::SeqCst);
        } else if prev_flag_max > 0 && !ctx.session_limit_frozen.load(Ordering::SeqCst) {
            ctx.ramping.store(true, Ordering::SeqCst);
        }
        prev_flag_max = flag_max;

        let completed = ctx.transcoded.load(Ordering::Relaxed);
        let errs = ctx.errors.load(Ordering::Relaxed);
        let ld_kill = LD_KILL.load(Ordering::Relaxed);
        let cancelled = CANCELLED.load(Ordering::Relaxed);
        if (completed + errs) as u64 >= ctx.file_count || cancelled || ld_kill {
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

/// Outcome of one auto-ramp evaluation. Pure value type so the decision
/// logic can be unit-tested without atomics or threads.
///
/// The render thread translates each variant into a concrete state mutation:
/// `Wait` does nothing, `RampUp` bumps `max_encoders` and updates the
/// baseline, `Stall` flips `ramping` off, `StallAndRevert` does the same
/// plus a `lower_max` call. Side-effect-free helpers ([`decide_ramp`] and
/// [`SIGNIFICANT_DROP_NUMERATOR`]/`DENOMINATOR`) live next to it.
#[derive(Debug, PartialEq, Eq)]
pub enum RampAction {
    /// Not enough information yet (slots haven't all reported a speed),
    /// or aggregate speed is still zero.
    Wait,
    /// Bump `max_encoders` by one. `new_baseline` is the speed reading
    /// that justified the bump and should replace the current baseline.
    RampUp { new_baseline: u64 },
    /// Throughput has stalled. Stop ramping for the rest of the run.
    Stall,
    /// Throughput dropped significantly (below `SIGNIFICANT_DROP_NUMERATOR
    /// / SIGNIFICANT_DROP_DENOMINATOR` of baseline). Stop ramping *and*
    /// revert the last `max_encoders++`.
    StallAndRevert,
}

/// "Significantly worse" threshold: 85 % of baseline. If speed drops
/// below this we treat the last ramp as actively harmful and revert it.
/// Anything between 85 % and 100 % is just a plateau — we stop ramping
/// but keep the current concurrency.
pub const SIGNIFICANT_DROP_NUMERATOR: u64 = 85;
pub const SIGNIFICANT_DROP_DENOMINATOR: u64 = 100;

/// Decide what (if anything) to do this ramp tick.
///
/// Inputs:
/// * `reporting` — how many active worker slots are currently reporting a
///   non-zero speed.
/// * `current_max` — the value of `max_encoders` at the start of this tick.
/// * `total_speed` — sum of reported speeds across active workers.
/// * `baseline` — the speed reading from the previous successful ramp, or 0
///   if we've never measured.
pub fn decide_ramp(
    reporting: u32,
    current_max: u32,
    total_speed: u64,
    baseline: u64,
) -> RampAction {
    // Wait until every active slot has reported a speed AND there's any
    // aggregate measurement to act on. Without both we don't know whether
    // the previous ramp helped.
    if reporting < current_max || total_speed == 0 {
        return RampAction::Wait;
    }
    // Two cases warrant another ramp: we've never measured (`baseline == 0`,
    // first observation), or total throughput went up since the last ramp.
    if baseline == 0 || total_speed > baseline {
        return RampAction::RampUp {
            new_baseline: total_speed,
        };
    }
    // Throughput stalled or dropped: stop ramping. Compute the threshold
    // in u128 so a large `baseline` (the helper is callable with any u64,
    // and we now unit-test it with arbitrary values) can't overflow during
    // the multiplication before the divide.
    let threshold = (baseline as u128) * (SIGNIFICANT_DROP_NUMERATOR as u128)
        / (SIGNIFICANT_DROP_DENOMINATOR as u128);
    if (total_speed as u128) < threshold {
        RampAction::StallAndRevert
    } else {
        RampAction::Stall
    }
}

fn try_ramp(ctx: &RenderCtx, baseline: &mut u64, last_ramp: &mut Instant) {
    let current_max = ctx.max_encoders.load(Ordering::SeqCst);

    // Snapshot reported speeds from each active (non-queued) worker slot.
    let speeds: Vec<u64> = ctx
        .slots
        .iter()
        .filter(|s| s.info.lock().unwrap().is_some() && !s.queued.load(Ordering::Relaxed))
        .map(|s| s.speed.load(Ordering::Relaxed))
        .collect();
    let reporting = speeds.iter().filter(|&&s| s > 0).count() as u32;
    let total_speed: u64 = speeds.iter().sum();

    match decide_ramp(reporting, current_max, total_speed, *baseline) {
        RampAction::Wait => {}
        RampAction::RampUp { new_baseline } => {
            *baseline = new_baseline;
            let new_max = current_max + 1;
            ctx.max_encoders.store(new_max, Ordering::SeqCst);
            ctx.flags
                .track_auto_ramp_increased(current_max, new_max, total_speed);
            *last_ramp = Instant::now();
        }
        RampAction::Stall => {
            ctx.ramping.store(false, Ordering::SeqCst);
            ctx.flags.track_auto_ramp_stopped(current_max, false);
        }
        RampAction::StallAndRevert => {
            ctx.ramping.store(false, Ordering::SeqCst);
            let reverted = current_max.saturating_sub(1).max(1);
            lower_max(&ctx.max_encoders, reverted);
            ctx.flags.track_auto_ramp_stopped(reverted, true);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_when_not_all_slots_reporting() {
        // 3 slots active, only 2 have a speed → wait for the third.
        assert_eq!(
            decide_ramp(2, 3, 100, 0),
            RampAction::Wait,
            "reporting < current_max must wait"
        );
    }

    #[test]
    fn wait_when_total_speed_zero() {
        // Reporting == max but everyone's at 0 (ffmpeg hasn't started
        // emitting progress yet). Wait, don't ramp.
        assert_eq!(decide_ramp(2, 2, 0, 0), RampAction::Wait);
    }

    #[test]
    fn first_observation_ramps_up() {
        // baseline == 0 sentinel means "we've never measured" → ramp on
        // first complete reading.
        assert_eq!(
            decide_ramp(2, 2, 100, 0),
            RampAction::RampUp { new_baseline: 100 }
        );
    }

    #[test]
    fn improving_throughput_ramps_up() {
        assert_eq!(
            decide_ramp(3, 3, 150, 100),
            RampAction::RampUp { new_baseline: 150 }
        );
    }

    #[test]
    fn equal_throughput_stalls_without_revert() {
        // baseline == total → no improvement → stop, but don't revert
        // (we're not actively worse, just plateaued).
        assert_eq!(decide_ramp(3, 3, 100, 100), RampAction::Stall);
    }

    #[test]
    fn slight_drop_stalls_without_revert() {
        // 90% of baseline: above the 85% threshold; plateau, don't revert.
        assert_eq!(decide_ramp(3, 3, 90, 100), RampAction::Stall);
    }

    #[test]
    fn threshold_exactly_stalls_without_revert() {
        // 85% is the boundary; the predicate is strict less-than, so 85
        // exactly is NOT a "significant drop".
        assert_eq!(decide_ramp(3, 3, 85, 100), RampAction::Stall);
    }

    #[test]
    fn significant_drop_reverts() {
        // 80% of baseline → below threshold → revert the last ramp.
        assert_eq!(decide_ramp(3, 3, 80, 100), RampAction::StallAndRevert);
    }

    #[test]
    fn far_below_baseline_reverts() {
        // GPU thrashing: speed collapsed. Definitely revert.
        assert_eq!(decide_ramp(3, 3, 10, 1000), RampAction::StallAndRevert);
    }

    #[test]
    fn waiting_takes_priority_over_baseline_compare() {
        // Even if we have a baseline and the totals look bad, if a slot
        // hasn't reported yet we still wait — we don't have full data.
        assert_eq!(decide_ramp(2, 3, 50, 100), RampAction::Wait);
    }

    #[test]
    fn current_max_zero_corner_case() {
        // Pathological but defensive: max=0 means "no active encoders",
        // so reporting==0 vacuously satisfies reporting >= current_max
        // but total_speed is also 0 → still Wait.
        assert_eq!(decide_ramp(0, 0, 0, 0), RampAction::Wait);
    }

    #[test]
    fn ramp_threshold_constants_are_sensible() {
        // Guards against accidental edits that would change the 85% rule.
        assert_eq!(SIGNIFICANT_DROP_NUMERATOR, 85);
        assert_eq!(SIGNIFICANT_DROP_DENOMINATOR, 100);
    }

    #[test]
    fn ramp_does_not_overflow_on_large_baselines() {
        // Real worker speed × 100 fits in u64 a million times over,
        // but the helper is now a pure function and easy to call with
        // anything. The intermediate `baseline * SIGNIFICANT_DROP_NUMERATOR`
        // must not overflow u64; doing the math in u128 covers it.
        // With a baseline of u64::MAX and a non-zero-but-tiny total_speed,
        // the previous implementation would panic in debug builds on the
        // multiplication. Now it computes the threshold cleanly and
        // returns StallAndRevert.
        let result = decide_ramp(1, 1, 1, u64::MAX);
        assert!(
            matches!(result, RampAction::StallAndRevert),
            "huge baseline with tiny current speed: {:?}",
            result
        );
    }

    #[test]
    fn ramp_handles_u64_max_total_speed() {
        // Symmetric: huge `total_speed` against a small baseline. Must
        // pick RampUp (improved) without panicking on the comparison.
        let result = decide_ramp(1, 1, u64::MAX, 100);
        assert!(
            matches!(result, RampAction::RampUp { .. }),
            "huge speed vs small baseline: {:?}",
            result
        );
    }
}
