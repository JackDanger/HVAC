use launchdarkly_server_sdk::{
    Client, ConfigBuilder, Context, ContextBuilder, MultiContextBuilder,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// LaunchDarkly feature-flag client. All methods are no-ops when no SDK key
/// is supplied; the app runs normally with defaults.
///
/// The SDK key is **CLI-only** by design — it is never read from the
/// environment. Pass it explicitly per invocation via
/// `hvac --launchdarkly-sdk-key <KEY> ...`. This prevents a key in a shell
/// rc from silently controlling every run.
pub struct Flags {
    client: Option<Arc<Client>>,
    hostname: String,
    username: String,
    gpu_name: Option<String>,
    gpu_encoder: Option<String>,
    gpu_kind: Option<String>,
}

impl Flags {
    /// Connect to LaunchDarkly using the supplied SDK key. Pass `None`
    /// (or an empty string) to get a no-op client. Blocks up to 2 s for
    /// initialization when a key is present.
    pub fn new(sdk_key: Option<&str>) -> Self {
        let hostname = get_hostname();
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        let client = sdk_key.filter(|k| !k.is_empty()).and_then(build_client);
        Flags {
            client,
            hostname,
            username,
            gpu_name: None,
            gpu_encoder: None,
            gpu_kind: None,
        }
    }

    /// Enrich the evaluation context with GPU info after detection.
    pub fn set_gpu(&mut self, name: &str, encoder: &str, kind: &str) {
        self.gpu_name = Some(name.to_string());
        self.gpu_encoder = Some(encoder.to_string());
        self.gpu_kind = Some(kind.to_string());
    }

    // ── Boolean flags ─────────────────────────────────────────────────────────

    /// Master kill-switch: set false to abort processing remaining files.
    pub fn enable_transcoding(&self) -> bool {
        self.bool_flag("enable-transcoding", true)
    }

    /// Pause all encoding. Workers spin until this goes false; in-flight jobs finish.
    pub fn pause_transcoding(&self) -> bool {
        self.bool_flag("pause-transcoding", false)
    }

    // ── Integer flags ─────────────────────────────────────────────────────────

    /// Override parallel job count. 0 = auto-detect from GPU.
    pub fn max_parallel_jobs(&self) -> usize {
        self.int_flag("max-parallel-jobs", 0).max(0) as usize
    }

    // ── Event tracking ────────────────────────────────────────────────────────

    pub fn track_gpu_detected(&self, name: &str, encoder: &str, kind: &str, max_sessions: usize) {
        self.emit_metric(
            "gpu-detected",
            max_sessions as f64,
            serde_json::json!({
                "gpu_name": name,
                "encoder": encoder,
                "kind": kind,
                "max_sessions": max_sessions as i64,
            }),
        );
    }

    pub fn track_scan_completed(&self, file_count: usize, total_size: u64) {
        self.emit_metric(
            "scan-completed",
            file_count as f64,
            serde_json::json!({
                "file_count": file_count as i64,
                "total_size_bytes": total_size as i64,
            }),
        );
    }

    pub fn track_transcode_started(
        &self,
        filename: &str,
        bitrate_kbps: u32,
        duration_secs: f64,
        source_size: u64,
        pix_fmt: &str,
    ) {
        self.emit_metric(
            "transcode-started",
            source_size as f64,
            serde_json::json!({
                "filename": filename,
                "bitrate_kbps": bitrate_kbps as i64,
                "duration_secs": duration_secs,
                "source_size_bytes": source_size as i64,
                "pix_fmt": pix_fmt,
            }),
        );
    }

    pub fn track_transcode_completed(
        &self,
        filename: &str,
        source_size: u64,
        output_size: u64,
        saved_pct: i32,
    ) {
        let bytes_saved = (source_size as i64 - output_size as i64).max(0);
        self.emit_metric(
            "transcode-completed",
            bytes_saved as f64,
            serde_json::json!({
                "filename": filename,
                "source_size_bytes": source_size as i64,
                "output_size_bytes": output_size as i64,
                "saved_pct": saved_pct,
                "bytes_saved": bytes_saved,
            }),
        );
    }

    pub fn track_transcode_failed(&self, filename: &str, error_type: &str) {
        self.emit(
            "transcode-failed",
            serde_json::json!({
                "filename": filename,
                "error_type": error_type,
            }),
        );
    }

    pub fn track_disk_wait(&self, filename: &str, available_gb: f64) {
        self.emit_metric(
            "disk-wait",
            available_gb,
            serde_json::json!({
                "filename": filename,
                "available_gb": available_gb,
            }),
        );
    }

    pub fn track_session_limit_hit(&self, active_sessions: u32) {
        self.emit(
            "session-limit-hit",
            serde_json::json!({ "active_sessions": active_sessions as i64 }),
        );
    }

    pub fn track_subtitle_retry(&self, filename: &str) {
        self.emit(
            "subtitle-retry",
            serde_json::json!({ "filename": filename }),
        );
    }

    pub fn track_auto_ramp_increased(&self, old_max: u32, new_max: u32, total_speed: u64) {
        self.emit_metric(
            "auto-ramp-increased",
            total_speed as f64,
            serde_json::json!({
                "old_max": old_max as i64,
                "new_max": new_max as i64,
                "total_speed": total_speed as i64,
            }),
        );
    }

    // ── Pause / resume ────────────────────────────────────────────────────────

    pub fn track_transcoding_paused(&self, active_workers: usize) {
        self.emit(
            "transcoding-paused",
            serde_json::json!({ "active_workers": active_workers as i64 }),
        );
    }

    pub fn track_transcoding_resumed(&self, active_workers: usize) {
        self.emit(
            "transcoding-resumed",
            serde_json::json!({ "active_workers": active_workers as i64 }),
        );
    }

    // ── Run start ─────────────────────────────────────────────────────────────

    pub fn track_run_started(
        &self,
        total_files: usize,
        total_bytes: u64,
        jobs: usize,
        auto_ramp: bool,
    ) {
        self.emit_metric(
            "run-started",
            total_files as f64,
            serde_json::json!({
                "total_files": total_files as i64,
                "total_bytes": total_bytes as i64,
                "jobs": jobs as i64,
                "auto_ramp": auto_ramp,
            }),
        );
    }

    // ── Transcode queue / retry ───────────────────────────────────────────────

    pub fn track_transcode_queued(&self, filename: &str, position: usize, max: u32) {
        self.emit(
            "transcode-queued",
            serde_json::json!({
                "filename": filename,
                "queue_position": position as i64,
                "max_parallel": max as i64,
            }),
        );
    }

    pub fn track_transcode_retry(&self, filename: &str, attempt: u32, reason: &str) {
        self.emit(
            "transcode-retry",
            serde_json::json!({
                "filename": filename,
                "attempt": attempt as i64,
                "reason": reason,
            }),
        );
    }

    pub fn track_auto_ramp_stopped(&self, final_max: u32, reverted: bool) {
        self.emit(
            "auto-ramp-stopped",
            serde_json::json!({
                "final_max": final_max as i64,
                "reverted": reverted,
            }),
        );
    }

    pub fn track_run_completed(
        &self,
        transcoded: u32,
        skipped: u32,
        errors: u32,
        bytes_saved: u64,
        bytes_input: u64,
        bytes_output: u64,
    ) {
        let ratio = if bytes_input > 0 {
            bytes_output as f64 / bytes_input as f64
        } else {
            1.0
        };
        self.emit_metric(
            "run-completed",
            bytes_saved as f64,
            serde_json::json!({
                "transcoded": transcoded as i64,
                "skipped": skipped as i64,
                "errors": errors as i64,
                "bytes_saved": bytes_saved as i64,
                "bytes_input": bytes_input as i64,
                "bytes_output": bytes_output as i64,
                "compression_ratio": ratio,
            }),
        );
    }

    /// Flush all buffered events and shut down. Call once at end of main().
    pub fn close(&self) {
        if let Some(ref c) = self.client {
            c.close();
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn make_context(&self) -> Context {
        let mut ub = ContextBuilder::new(&self.hostname);
        ub.kind("user")
            .set_string("hostname", self.hostname.as_str())
            .set_string("username", self.username.as_str());
        let user_ctx = ub
            .build()
            .unwrap_or_else(|_| ContextBuilder::new("fallback").build().expect("fallback"));

        match (&self.gpu_name, &self.gpu_encoder) {
            (Some(gpu_name), Some(gpu_encoder)) => {
                let gpu_key = format!("{}.{}", gpu_encoder, self.hostname);
                let mut db = ContextBuilder::new(&gpu_key);
                db.kind("device")
                    .set_string("gpu_name", gpu_name.as_str())
                    .set_string("encoder", gpu_encoder.as_str());
                if let Some(ref kind) = self.gpu_kind {
                    db.set_string("kind", kind.as_str());
                }
                let device_ctx = db.build().unwrap_or_else(|_| {
                    ContextBuilder::new("fallback-device")
                        .build()
                        .expect("fallback device")
                });

                let mut mb = MultiContextBuilder::new();
                mb.add_context(user_ctx).add_context(device_ctx);
                mb.build().unwrap_or_else(|_| {
                    ContextBuilder::new(&self.hostname)
                        .build()
                        .expect("fallback multi")
                })
            }
            _ => user_ctx,
        }
    }

    fn bool_flag(&self, key: &str, default: bool) -> bool {
        match &self.client {
            None => default,
            Some(c) => {
                let ctx = self.make_context();
                let detail = c.bool_variation_detail(&ctx, key, default);
                let value = detail.value.unwrap_or(default);
                log::debug!("flag {key:?} = {value:?} ({:?})", detail.reason);
                value
            }
        }
    }

    fn int_flag(&self, key: &str, default: i64) -> i64 {
        match &self.client {
            None => default,
            Some(c) => c.int_variation(&self.make_context(), key, default),
        }
    }

    fn emit(&self, key: &str, data: serde_json::Value) {
        if let Some(ref c) = self.client {
            let _ = c.track_data(self.make_context(), key, data);
        }
    }

    fn emit_metric(&self, key: &str, metric: f64, data: serde_json::Value) {
        if let Some(ref c) = self.client {
            c.track_metric(self.make_context(), key, metric, data);
        }
    }
}

fn build_client(sdk_key: &str) -> Option<Arc<Client>> {
    if sdk_key.is_empty() {
        return None;
    }

    let config = match ConfigBuilder::new(sdk_key).build() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("LaunchDarkly config error: {e}");
            return None;
        }
    };
    let client = match Client::build(config) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("LaunchDarkly client build error: {e}");
            return None;
        }
    };
    if let Err(e) = client.start_with_runtime() {
        log::warn!("LaunchDarkly start error: {e}");
        return None;
    }

    // Wait up to 2 s for flag data to arrive.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !client.initialized() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    if client.initialized() {
        log::info!("LaunchDarkly: connected");
    } else {
        log::warn!("LaunchDarkly: not initialized within 2s — evaluations use defaults");
    }

    Some(Arc::new(client))
}

fn get_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    unsafe {
        let mut buf = vec![0u8; 256];
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char);
            if let Ok(s) = cstr.to_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_new_with_none_is_noop() {
        // No SDK key → no client → all flag reads return defaults.
        let flags = Flags::new(None);
        assert!(flags.client.is_none());
        assert!(flags.enable_transcoding()); // default: true
        assert!(!flags.pause_transcoding()); // default: false
    }

    #[test]
    fn flags_new_with_empty_str_is_noop() {
        // Empty string is treated identically to None.
        let flags = Flags::new(Some(""));
        assert!(flags.client.is_none());
    }
}
