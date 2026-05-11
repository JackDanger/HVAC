use opentelemetry::{global, trace::Span, trace::Tracer, KeyValue};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
// `SdkTracerProvider` is the SDK-side concrete type (renamed from
// `TracerProvider` in 0.28). It implements the trait
// `opentelemetry::trace::TracerProvider`, which is what `global::set_tracer_provider`
// requires. `SimpleSpanProcessor::new` now takes the exporter by value
// (no `Box`). `Resource` switched to a builder API.
use opentelemetry_sdk::{
    trace::{SdkTracerProvider, SimpleSpanProcessor},
    Resource,
};
use std::collections::HashMap;

const OTLP_ENDPOINT: &str = "https://otel.observability.app.launchdarkly.com:4318";

/// RAII span — ends automatically on drop.
//
// `OtelSpan` and the attribute setters are the future call surface for
// per-encode telemetry events (`span("transcode")` with file/codec/duration
// attributes). The current binary doesn't emit any spans yet — once it
// does, these methods become live; until then clippy's -D warnings would
// reject the unused public API.
#[allow(dead_code)]
pub struct OtelSpan {
    inner: opentelemetry::global::BoxedSpan,
}

#[allow(dead_code)]
impl OtelSpan {
    pub fn str_attr(&mut self, key: &str, val: &str) {
        self.inner
            .set_attribute(KeyValue::new(key.to_string(), val.to_string()));
    }

    pub fn int_attr(&mut self, key: &str, val: i64) {
        self.inner
            .set_attribute(KeyValue::new(key.to_string(), val));
    }

    pub fn float_attr(&mut self, key: &str, val: f64) {
        self.inner
            .set_attribute(KeyValue::new(key.to_string(), val));
    }

    pub fn error(&mut self, msg: &str) {
        self.inner
            .set_status(opentelemetry::trace::Status::error(msg.to_string()));
    }
}

impl Drop for OtelSpan {
    fn drop(&mut self) {
        self.inner.end();
    }
}

/// OpenTelemetry → LaunchDarkly Observability bridge.
/// Enabled when an SDK key is supplied (same key used for feature flags).
/// Sends traces to the LD OTLP endpoint via HTTP/proto with Bearer auth.
///
/// The SDK key is **CLI-only** by design — never read from the environment.
/// Pass it via `--launchdarkly-sdk-key <KEY>` per invocation.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    /// Initialise tracing with the supplied SDK key. Pass `None` (or an
    /// empty string) to get a no-op telemetry instance.
    pub fn new(sdk_key: Option<&str>) -> Self {
        let sdk_key = match sdk_key {
            Some(v) if !v.is_empty() => v,
            _ => return Telemetry { provider: None },
        };

        match Self::build(sdk_key) {
            Ok(provider) => {
                global::set_tracer_provider(provider.clone());
                log::info!("LD Observability: tracing enabled → {}", OTLP_ENDPOINT);
                Telemetry {
                    provider: Some(provider),
                }
            }
            Err(e) => {
                log::warn!("LD Observability init error: {e}");
                Telemetry { provider: None }
            }
        }
    }

    fn build(sdk_key: &str) -> anyhow::Result<SdkTracerProvider> {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {}", sdk_key));

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/traces", OTLP_ENDPOINT))
            .with_headers(headers)
            .build()
            .map_err(|e| anyhow::anyhow!("OTel exporter: {e}"))?;

        let resource = Resource::builder()
            .with_attributes([
                KeyValue::new("service.name", "hvac"),
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            ])
            .build();

        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter))
            .with_resource(resource)
            .build();

        Ok(provider)
    }

    /// Start a named span. Safe to call when not connected (returns no-op span).
    /// Currently unused — see the OtelSpan #[allow(dead_code)] note above.
    #[allow(dead_code)]
    pub fn span(&self, name: &str) -> OtelSpan {
        OtelSpan {
            inner: global::tracer("hvac").start(name.to_string()),
        }
    }

    /// Flush any pending spans. Idempotent — safe to call from a Drop guard
    /// alongside the explicit end-of-run shutdown so callers that exit via
    /// early-return paths still flush.
    pub fn shutdown(&mut self) {
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_new_with_none_is_noop() {
        let t = Telemetry::new(None);
        assert!(t.provider.is_none());
    }

    #[test]
    fn telemetry_new_with_empty_str_is_noop() {
        let t = Telemetry::new(Some(""));
        assert!(t.provider.is_none());
    }
}
