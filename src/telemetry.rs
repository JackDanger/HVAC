use opentelemetry::{global, trace::Span, trace::Tracer, KeyValue};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    trace::{SdkTracerProvider, SimpleSpanProcessor},
    Resource,
};
use std::collections::HashMap;

const OTLP_ENDPOINT: &str = "https://otel.observability.app.launchdarkly.com:4318";

/// RAII span — ends automatically on drop.
pub struct OtelSpan {
    inner: opentelemetry::global::BoxedSpan,
}

impl OtelSpan {
    pub fn str_attr(&mut self, key: &str, val: &str) {
        self.inner
            .set_attribute(KeyValue::new(key.to_string(), val.to_string()));
    }

    pub fn int_attr(&mut self, key: &str, val: i64) {
        self.inner.set_attribute(KeyValue::new(key.to_string(), val));
    }

    pub fn float_attr(&mut self, key: &str, val: f64) {
        self.inner.set_attribute(KeyValue::new(key.to_string(), val));
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
/// Enabled when LAUNCHDARKLY_SDK_KEY is set (same key used for feature flags).
/// Sends traces to the LD OTLP endpoint via HTTP/proto with Bearer auth.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    pub fn new() -> Self {
        let sdk_key = match std::env::var("LAUNCHDARKLY_SDK_KEY") {
            Ok(v) if !v.is_empty() => v,
            _ => return Telemetry { provider: None },
        };

        match Self::build(&sdk_key) {
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
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", sdk_key),
        );

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/traces", OTLP_ENDPOINT))
            .with_headers(headers)
            .build()
            .map_err(|e| anyhow::anyhow!("OTel exporter: {e}"))?;

        let resource = Resource::new([
            KeyValue::new("service.name", "tdorr"),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        ]);

        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter))
            .with_resource(resource)
            .build();

        Ok(provider)
    }

    pub fn is_connected(&self) -> bool {
        self.provider.is_some()
    }

    /// Start a named span. Safe to call when not connected (returns no-op span).
    pub fn span(&self, name: &str) -> OtelSpan {
        OtelSpan {
            inner: global::tracer("tdorr").start(name.to_string()),
        }
    }

    pub fn shutdown(self) {
        if let Some(provider) = self.provider {
            let _ = provider.shutdown();
        }
    }
}
