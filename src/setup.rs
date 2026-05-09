use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const BASE: &str = "https://app.launchdarkly.com/api/v2";
const PROJECT_KEY: &str = "HEVCuum";
const PROJECT_NAME: &str = "HEVCuum";
const ENV_KEY: &str = "production";

pub fn run(api_key: &str) -> Result<()> {
    let (sdk_key, project_action) = find_or_create_project(api_key)?;
    let (created, skipped) = ensure_flags(api_key)?;

    eprintln!("Project:     {} ({})", PROJECT_KEY, project_action);
    eprintln!("Environment: {}", ENV_KEY);
    eprintln!("SDK key:     {}", sdk_key);
    if !created.is_empty() {
        eprintln!("Flags created: {}", created.join(", "));
    }
    if !skipped.is_empty() {
        eprintln!("Flags skipped: {}", skipped.join(", "));
    }
    eprintln!(
        "\nToggle pause: https://app.launchdarkly.com/{}/production/features/pause-transcoding",
        PROJECT_KEY
    );
    // stdout only — lets `eval $(HEVCuum --setup-launchdarkly ...)` set the var directly
    println!("export LAUNCHDARKLY_SDK_KEY={}", sdk_key);

    Ok(())
}

fn find_or_create_project(api_key: &str) -> Result<(String, &'static str)> {
    match api_get(api_key, &format!("/projects/{PROJECT_KEY}"))? {
        Some(_) => {
            let env = api_get(
                api_key,
                &format!("/projects/{PROJECT_KEY}/environments/{ENV_KEY}"),
            )?
            .ok_or_else(|| {
                anyhow::anyhow!("environment '{ENV_KEY}' not found in project '{PROJECT_KEY}'")
            })?;
            let sdk_key = extract_sdk_key(&env)?;
            Ok((sdk_key, "found"))
        }
        None => {
            let body = json!({
                "key": PROJECT_KEY,
                "name": PROJECT_NAME,
                "environments": [{
                    "key": ENV_KEY,
                    "name": "Production",
                    "color": "417505"
                }]
            });
            let resp = api_post(api_key, "/projects", &body)?;
            // Created project response has environments as an array
            let sdk_key = resp
                .get("environments")
                .and_then(|e| e.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|e| e.get("key").and_then(|k| k.as_str()) == Some(ENV_KEY))
                })
                .and_then(|e| e.get("apiKey"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("no SDK key in created project response: {}", resp))
                .and_then(|s| {
                    if s.contains("***") {
                        bail!("SDK key masked — check API key permissions (needs Writer role)")
                    } else {
                        Ok(s.to_string())
                    }
                })?;
            Ok((sdk_key, "created"))
        }
    }
}

fn extract_sdk_key(env: &Value) -> Result<String> {
    let key = env
        .get("apiKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no apiKey in environment response"))?;
    if key.contains("***") {
        bail!("SDK key masked — check API key permissions (needs Writer role)");
    }
    Ok(key.to_string())
}

fn ensure_flags(api_key: &str) -> Result<(Vec<String>, Vec<String>)> {
    let existing: std::collections::HashSet<String> =
        api_get(api_key, &format!("/flags/{PROJECT_KEY}"))?
            .and_then(|v| v.get("items").and_then(|i| i.as_array()).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|f| f.get("key")?.as_str().map(String::from))
            .collect();

    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for spec in flag_specs() {
        let key = spec["key"].as_str().unwrap().to_string();
        if existing.contains(&key) {
            skipped.push(key);
        } else {
            api_post(api_key, &format!("/flags/{PROJECT_KEY}"), &spec)?;
            created.push(key);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    Ok((created, skipped))
}

// ── HTTP helpers (curl subprocess — no extra deps) ────────────────────────────

fn api_get(api_key: &str, path: &str) -> Result<Option<Value>> {
    let url = format!("{BASE}{path}");
    let output = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .arg("-H")
        .arg(format!("Authorization: {api_key}"))
        .arg(&url)
        .output()
        .context("curl not found in PATH")?;

    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status_str) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    match status_str.trim().parse::<u16>().unwrap_or(0) {
        200..=299 => serde_json::from_str(body)
            .with_context(|| format!("invalid JSON from GET {path}"))
            .map(Some),
        404 => Ok(None),
        code => bail!("GET {path} → {code}: {}", ld_message(body)),
    }
}

fn api_post(api_key: &str, path: &str, body: &Value) -> Result<Value> {
    let url = format!("{BASE}{path}");
    let body_str = serde_json::to_string(body)?;
    let output = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "-X", "POST"])
        .arg("-H")
        .arg(format!("Authorization: {api_key}"))
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(&body_str)
        .arg(&url)
        .output()
        .context("curl not found in PATH")?;

    let text = String::from_utf8_lossy(&output.stdout);
    let (resp_body, status_str) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    match status_str.trim().parse::<u16>().unwrap_or(0) {
        200..=299 => serde_json::from_str(resp_body)
            .with_context(|| format!("invalid JSON from POST {path}")),
        code => bail!("POST {path} → {code}: {}", ld_message(resp_body)),
    }
}

fn ld_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| body.to_string())
}

// ── Flag definitions (all flags the binary evaluates, hardcoded) ──────────────
//
// Variation index convention: for boolean flags, index 0 = true, 1 = false.
// offVariation = value served when targeting is disabled (flag toggled off).
// onVariation  = fallthrough value when targeting is enabled with no matching rule.
//
// pause-transcoding and dry-run default to false; turning them ON in the LD
// UI serves true (index 0) to all contexts via the fallthrough rule.
// All "enabled by default" flags have both on/off serving true (index 0); to
// disable them, create a targeting rule that serves false to specific contexts.

fn flag_specs() -> Vec<Value> {
    vec![
        // ── Remote control ────────────────────────────────────────────────────
        json!({
            "key": "pause-transcoding",
            "name": "Pause Transcoding",
            "kind": "boolean",
            "description": "Halt all active workers mid-run. Toggle back to resume.",
            "temporary": false,
            "variations": [{"value": true, "name": "Paused"}, {"value": false, "name": "Running"}],
            "defaults": {"onVariation": 0, "offVariation": 1}
        }),
        json!({
            "key": "enable-transcoding",
            "name": "Enable Transcoding",
            "kind": "boolean",
            "description": "Master kill-switch. Serve false to any context to abort at startup.",
            "temporary": false,
            "variations": [{"value": true, "name": "Enabled"}, {"value": false, "name": "Disabled"}],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "dry-run",
            "name": "Dry Run",
            "kind": "boolean",
            "description": "Show what would be transcoded without encoding. Toggle on to enable.",
            "temporary": false,
            "variations": [{"value": true, "name": "Enabled"}, {"value": false, "name": "Disabled"}],
            "defaults": {"onVariation": 0, "offVariation": 1}
        }),
        // ── Worker behaviour ──────────────────────────────────────────────────
        json!({
            "key": "enable-auto-ramp",
            "name": "Enable Auto Ramp",
            "kind": "boolean",
            "description": "Discover optimal parallel job count at runtime. Disable to use fixed -j.",
            "temporary": false,
            "variations": [{"value": true, "name": "Enabled"}, {"value": false, "name": "Disabled"}],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "enable-iso-support",
            "name": "Enable ISO Support",
            "kind": "boolean",
            "description": "Process .iso and .img disc images via isomage.",
            "temporary": false,
            "variations": [{"value": true, "name": "Enabled"}, {"value": false, "name": "Disabled"}],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "enable-subtitle-retry",
            "name": "Enable Subtitle Retry",
            "kind": "boolean",
            "description": "Retry failed encodes without subtitle streams.",
            "temporary": false,
            "variations": [{"value": true, "name": "Enabled"}, {"value": false, "name": "Disabled"}],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        // ── Tuning (multivariate) ─────────────────────────────────────────────
        json!({
            "key": "gpu-encoder-override",
            "name": "GPU Encoder Override",
            "kind": "multivariate",
            "description": "Override detected GPU encoder. Empty string = auto-detect.",
            "temporary": false,
            "variations": [
                {"value": "",                    "name": "Auto (default)"},
                {"value": "hevc_nvenc",          "name": "NVIDIA NVENC"},
                {"value": "hevc_vaapi",          "name": "Intel VAAPI"},
                {"value": "hevc_videotoolbox",   "name": "Apple VideoToolbox"}
            ],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "transcode-preset",
            "name": "Transcode Preset",
            "kind": "multivariate",
            "description": "Override ffmpeg quality preset. Empty string = use config file.",
            "temporary": false,
            "variations": [
                {"value": "",       "name": "Config default"},
                {"value": "fast",   "name": "Fast"},
                {"value": "medium", "name": "Medium"},
                {"value": "slow",   "name": "Slow"},
                {"value": "p1",     "name": "NVENC P1 (fastest)"},
                {"value": "p7",     "name": "NVENC P7 (best quality)"}
            ],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "max-parallel-jobs",
            "name": "Max Parallel Jobs",
            "kind": "multivariate",
            "description": "Override parallel job count. 0 = auto-detect from GPU.",
            "temporary": false,
            "variations": [
                {"value": 0, "name": "Auto (default)"},
                {"value": 2, "name": "2"},
                {"value": 4, "name": "4"},
                {"value": 8, "name": "8"}
            ],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "max-bitrate-kbps",
            "name": "Max Bitrate (kbps)",
            "kind": "multivariate",
            "description": "Override max output bitrate in kbps. 0 = use config value.",
            "temporary": false,
            "variations": [
                {"value": 0,     "name": "Config default"},
                {"value": 4000,  "name": "4 Mbps"},
                {"value": 8000,  "name": "8 Mbps"},
                {"value": 12000, "name": "12 Mbps"}
            ],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "max-session-retries",
            "name": "Max Session Retries",
            "kind": "multivariate",
            "description": "Max retries on NVENC session-limit errors.",
            "temporary": false,
            "variations": [
                {"value": 5, "name": "5 (default)"},
                {"value": 1, "name": "1"},
                {"value": 10, "name": "10"},
                {"value": 0, "name": "No retries"}
            ],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "disk-headroom-extra-gb",
            "name": "Disk Headroom Extra (GB)",
            "kind": "multivariate",
            "description": "Extra GB disk reserve beyond the base 2 GB safety margin.",
            "temporary": false,
            "variations": [
                {"value": 0.0,  "name": "0 GB (default)"},
                {"value": 5.0,  "name": "5 GB"},
                {"value": 10.0, "name": "10 GB"},
                {"value": 20.0, "name": "20 GB"}
            ],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
        json!({
            "key": "extra-ffmpeg-args",
            "name": "Extra ffmpeg Args",
            "kind": "multivariate",
            "description": "Extra args appended to every ffmpeg encode command (JSON array of strings).",
            "temporary": false,
            "variations": [
                {"value": [],                                   "name": "None (default)"},
                {"value": ["-tune", "hq"],                     "name": "NVENC HQ tune"},
                {"value": ["-rc", "vbr", "-cq", "28"],         "name": "NVENC VBR CQ28"}
            ],
            "defaults": {"onVariation": 0, "offVariation": 0}
        }),
    ]
}
