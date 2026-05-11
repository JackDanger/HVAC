//! Command-line interface definition.
//!
//! All user-facing flags live in one place so the help output stays coherent
//! and so secrets like the LaunchDarkly SDK key are visible-but-explicit (no
//! sneaky `env=` attributes, no `Debug` derive that would leak them on
//! accidental `dbg!`).

use clap::Parser;
use std::path::PathBuf;

use crate::pipeline::MIN_TRANSCODE_DURATION_SECS;

// NOTE: deliberately no `Debug` derive — fields include the LaunchDarkly SDK
// key, and a stray `dbg!(cli)` would leak it into logs. Keeping this struct
// off Debug means accidental prints fail to compile.
#[derive(Parser)]
#[command(name = "hvac", version, about = "GPU-accelerated media transcoder")]
pub struct Cli {
    /// Directory to scan for media files
    #[arg(required_unless_present_any = ["dump_config", "setup_launchdarkly"])]
    pub path: Option<PathBuf>,

    /// Path to YAML config file (uses built-in defaults if omitted)
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Print the built-in default config to stdout and exit
    #[arg(long)]
    pub dump_config: bool,

    /// Suppress the banner shown when running with built-in default config
    #[arg(short, long)]
    pub quiet: bool,

    /// Keep originals: write `.transcoded.<ext>` copies alongside instead of overwriting in place
    #[arg(long, default_value_t = false)]
    pub no_overwrite: bool,

    /// Dry run — print what would be transcoded and exit without touching anything
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Number of parallel encode jobs (auto-detected from GPU if not set)
    #[arg(short, long)]
    pub jobs: Option<usize>,

    /// Output directory for transcoded files (overrides config)
    #[arg(short, long)]
    pub output_dir: Option<PathBuf>,

    /// Replace originals with transcoded copies after all encodes complete
    #[arg(long, default_value_t = false)]
    pub replace: bool,

    /// Skip files shorter than this many seconds (animated GIFs, single-frame stubs)
    #[arg(long, default_value_t = MIN_TRANSCODE_DURATION_SECS)]
    pub min_duration: f64,

    /// Maximum seconds ffprobe may run on a single file before being killed.
    /// Protects against hangs caused by stale NFS / unresponsive SMB mounts.
    #[arg(long, default_value_t = 30)]
    pub probe_timeout: u64,

    /// LaunchDarkly SDK key for runtime feature-flag control (pause, kill-switch,
    /// parallel-job throttle). Omit to disable remote control entirely.
    ///
    /// CLI-only by design: hvac never reads this from the environment, so a
    /// key in your shell rc cannot silently affect every run. Pass it
    /// explicitly per invocation when you want remote control to be active.
    #[arg(long, value_name = "KEY")]
    pub launchdarkly_sdk_key: Option<String>,

    /// Provision a LaunchDarkly project + all flags using your LD account
    /// API key, then print the SDK key. Idempotent: re-runs on an existing
    /// project just reuse the SDK key. Pair with --ld-api-key.
    #[arg(long, default_value_t = false)]
    pub setup_launchdarkly: bool,

    /// LaunchDarkly account API key (used only with --setup-launchdarkly).
    #[arg(long, value_name = "KEY")]
    pub ld_api_key: Option<String>,
}

impl Cli {
    /// Overwriting originals in place is the default; `--no-overwrite` opts out.
    pub fn overwrite(&self) -> bool {
        !self.no_overwrite
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_command_factory_parses() {
        // Smoke test: the derive macro produced a valid clap::Command.
        let cmd = Cli::command();
        let _ = cmd.get_name();
    }

    #[test]
    fn overwrite_default_is_true() {
        let cli = Cli::try_parse_from(["hvac", "/some/path"]).unwrap();
        assert!(cli.overwrite());
        assert!(!cli.no_overwrite);
    }

    #[test]
    fn overwrite_false_when_no_overwrite_set() {
        let cli = Cli::try_parse_from(["hvac", "--no-overwrite", "/some/path"]).unwrap();
        assert!(!cli.overwrite());
    }

    #[test]
    fn min_duration_defaults_to_constant() {
        let cli = Cli::try_parse_from(["hvac", "/some/path"]).unwrap();
        assert_eq!(cli.min_duration, MIN_TRANSCODE_DURATION_SECS);
    }

    #[test]
    fn probe_timeout_default_is_30() {
        let cli = Cli::try_parse_from(["hvac", "/some/path"]).unwrap();
        assert_eq!(cli.probe_timeout, 30);
    }

    #[test]
    fn launchdarkly_sdk_key_takes_no_env_var() {
        // Even if LAUNCHDARKLY_SDK_KEY is set in the environment, the flag must
        // NOT pick it up. CLI-only by design. We set the env var here and
        // confirm clap leaves the field None when the flag wasn't passed.
        // SAFETY: tests run single-threaded by default but we still scope.
        unsafe {
            std::env::set_var("LAUNCHDARKLY_SDK_KEY", "should-not-be-read");
        }
        let cli = Cli::try_parse_from(["hvac", "/some/path"]).unwrap();
        unsafe {
            std::env::remove_var("LAUNCHDARKLY_SDK_KEY");
        }
        assert!(cli.launchdarkly_sdk_key.is_none());
    }

    #[test]
    fn setup_launchdarkly_does_not_require_path() {
        // Path is required_unless_present_any[dump_config, setup_launchdarkly].
        let cli =
            Cli::try_parse_from(["hvac", "--setup-launchdarkly", "--ld-api-key", "k"]).unwrap();
        assert!(cli.path.is_none());
        assert!(cli.setup_launchdarkly);
    }

    #[test]
    fn dump_config_does_not_require_path() {
        let cli = Cli::try_parse_from(["hvac", "--dump-config"]).unwrap();
        assert!(cli.path.is_none());
        assert!(cli.dump_config);
    }
}
