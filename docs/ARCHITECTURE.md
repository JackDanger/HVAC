# Architecture

A walkthrough of how a `hvac <directory>` invocation flows through the
codebase, in the order the code runs. Cross-reference the module names
against `src/` while you read.

## Runtime flow

```
                    ┌───────────────────────────────────────┐
                    │ main()                                │
                    │  • Parse Cli                          │
                    │  • Install SIGINT handler             │
                    │  • Detect symbols, terminal width     │
                    │  • Load config (YAML or embedded)     │
                    │  • Detect GPU                         │
                    │  • LdGuard { flags, telemetry }       │
                    └────────────────┬──────────────────────┘
                                     │
                                     ▼
       ┌──────────────────────────────────────────────────────────┐
       │ Phase 1 — scanner::scan + pipeline::scan::expand         │
       │  Vec<PathBuf>  →  Vec<ScanItem>                          │
       │                                                          │
       │  • Recursive directory walk (scanner.rs)                 │
       │  • Each ISO opens via isomage; expands into either       │
       │      - single ScanItem (single-feature),                 │
       │      - one ScanItem with inner_paths (multi-file),       │
       │      - N ScanItems with title_suffix (multi-title DVD).  │
       │  • AACS / BD+ encrypted discs: skipped here.             │
       └──────────────────────────────┬───────────────────────────┘
                                      │
                                      ▼
       ┌──────────────────────────────────────────────────────────┐
       │ Phase 2 — pipeline::partition::partition                 │
       │  Vec<ScanItem>  →  Vec<WorkItem>                         │
       │                                                          │
       │  Per item, ffprobe → classify_item():                    │
       │   • Skip: already meets target (h265, ≤ max res/bitrate) │
       │   • Skip: too short (< --min-duration, default 1s)       │
       │   • Skip: 10-bit on non-10-bit-HEVC NVENC silicon        │
       │   • Skip: dest dir not writable (pre-flight probe)       │
       │   • Resume: existing .transcoded.* + matching marker     │
       │   • Queue: build WorkItem with probed metadata           │
       │                                                          │
       │  Multi-file ISOs probe every inner file and aggregate    │
       │  (max bitrate, summed duration) for accurate -maxrate.   │
       └──────────────────────────────┬───────────────────────────┘
                                      │
                                      ▼
       ┌──────────────────────────────────────────────────────────┐
       │ Phase 3 — pipeline::worker::run_worker × jobs            │
       │           pipeline::render::run_render × 1               │
       │                                                          │
       │   Workers pop the next WorkItem off a shared atomic      │
       │   counter, encode via transcode::transcode(_iso),        │
       │   surface progress through their WorkerSlot. Failures    │
       │   walk the RetryDecision state machine:                  │
       │                                                          │
       │     ReencodeAudio → ReencodeSubtitles → SkipSubtitles    │
       │       (cheap codec fallbacks)                            │
       │     DiskSpace    — wait, retry                           │
       │     SessionLimit — lower max, possibly freeze            │
       │     Bail         — surface to user                       │
       │                                                          │
       │   On success: write `<output>.hvac.complete` marker so   │
       │   the next run can adopt it instead of re-encoding.      │
       │                                                          │
       │   Render thread: 200ms tick, paints per-worker viewport  │
       │   and the overall progress bar; drives auto-ramp by      │
       │   watching aggregate speed.                              │
       └──────────────────────────────┬───────────────────────────┘
                                      │
                                      ▼
       ┌──────────────────────────────────────────────────────────┐
       │ Phase 4 (--replace) — pipeline::replace::run             │
       │   For each `.transcoded.<ext>` next to a source, run     │
       │   replace_original (re-validates via ffprobe + marker,   │
       │   then renames over the original).                       │
       └──────────────────────────────────────────────────────────┘
```

## Module map

| Module | Responsibility |
| --- | --- |
| `main.rs` | Orchestration only. Reads `Cli`, drives the four phases. |
| `cli.rs` | `clap` struct. Deliberately **no** `Debug` derive (SDK key). |
| `ui.rs` | `Symbols` table, terminal width, banner, progress bar. |
| `pipeline/scan.rs` | Phase 1 — ISO expansion → `Vec<ScanItem>`. |
| `pipeline/partition.rs` | Phase 2 — probe + classify → `Vec<WorkItem>`. |
| `pipeline/worker.rs` | Phase 3 — encode loop + typed retry state machine. |
| `pipeline/render.rs` | Phase 3 — progress UI + auto-ramp driver. |
| `pipeline/replace.rs` | Phase 4 — optional `--replace` pass. |
| `config.rs` | YAML config + embedded defaults. |
| `gpu.rs` | NVIDIA / Intel / Apple GPU detection. |
| `iso.rs` | Disc-image analysis (DVD / Blu-ray / AVCHD / bare). |
| `probe.rs` | ffprobe wrapper with watchdog timeout. |
| `scanner.rs` | Recursive media walk + NFS/SMB mount detection. |
| `transcode.rs` | ffmpeg invocation, validation, completion marker. |
| `flags.rs` | LaunchDarkly client (CLI-only SDK key). |
| `telemetry.rs` | OpenTelemetry → LaunchDarkly bridge. |
| `setup.rs` | One-shot LaunchDarkly project provisioner. |
| `util.rs` | `format_size`, `available_disk_space`. |

## Concurrency model

Workers + the renderer run in `std::thread::scope`. Shared state is one of:

1. **Atomics** for the hot path. `WorkerSlot` is all atomics + one `Mutex`
   (only for the name+size pair which has to update together). Worker
   slots are read by the renderer every 200ms without blocking workers.
2. **`Mutex<Vec<_>>`** for the completed-lines queue and the registered
   tmp-dirs list. Both are touched rarely (once per finished file / once
   at start-of-Phase-3) so lock contention is irrelevant.
3. **`Arc<T>`** for everything else. `WorkerCtx` bundles the Arcs that
   each worker needs so the spawn loop doesn't pile up `Arc::clone(&...)`
   noise.

No `tokio` / `async`: this is GPU-bound work behind a small number of
ffmpeg processes. Threads are simpler and the wall-clock cost is dominated
by the encoder anyway.

## Retry tiers

`classify_failure(err_str, &state)` is the only place we decide what to do
on a failed encode. The tier order matters: deterministic codec fallbacks
fire before resource-pressure waits before nuclear "drop subtitles". Each
tier fires at most once per file (except disk-space and session-limit,
which can retry up to `MAX_SESSION_RETRIES`).

```
disk space  ┐
audio copy  │
sub copy    │  cheap, deterministic — try these first
sub reenc   │
skip subs   ┘
session ╶── may apply at any tier; tracks cumulative hits and freezes
            the parallel-encoder ceiling after MAX_SESSION_LIMIT_BEFORE_FREEZE
bail    ╶── nothing we know how to recover from
```

See `RetryDecision` in `src/pipeline/worker.rs` for the enum, and
`classify_failure` for the matching logic. Both have inline tests pinning
the tier ordering, the "fires once" semantics, and the cap behaviour.

## On-disk artefacts

A run touches three classes of sidecar file:

- **`.hvac_tmp_<stem>.<ext>`** — in-progress encode in `--overwrite` mode.
  Renamed over the source on success. The SIGINT-twice handler sweeps any
  that linger.
- **`<output>.hvac.complete`** — JSON marker written after final-rename
  success. The next run reads this to decide if it can resume / adopt an
  existing output rather than re-encoding.
- **`.hvac_writable_check_<pid>_<nanos>`** — momentary probe file for the
  Phase 2 pre-flight writable check. Created + deleted in the same call;
  uses nanosecond timestamps + `create_new` so it can't clobber a user
  file even on PID collision.

`.transcoded.<ext>` is the real output file in `--no-overwrite` mode; it
isn't a sidecar.
