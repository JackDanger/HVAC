use opentelemetry::{global, trace::Span, trace::Tracer, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{SdkTracerProvider, SimpleSpanProcessor},
    Resource,
};

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

/// OpenTelemetry → Highlight bridge.
/// No-op when HIGHLIGHT_PROJECT_ID is unset.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    pub fn new() -> Self {
        let project_id = match std::env::var("HIGHLIGHT_PROJECT_ID") {
            Ok(v) if !v.is_empty() => v,
            _ => return Telemetry { provider: None },
        };

        match Self::build(&project_id) {
            Ok(provider) => {
                global::set_tracer_provider(provider.clone());
                log::info!("Highlight: connected (project {})", project_id);
                Telemetry {
                    provider: Some(provider),
                }
            }
            Err(e) => {
                log::warn!("Highlight init error: {e}");
                Telemetry { provider: None }
            }
        }
    }

    fn build(project_id: &str) -> anyhow::Result<SdkTracerProvider> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint("https://otel.highlight.io:4318")
            .build()
            .map_err(|e| anyhow::anyhow!("OTel exporter: {e}"))?;

        let resource = Resource::new([
            KeyValue::new("highlight.project_id", project_id.to_string()),
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
