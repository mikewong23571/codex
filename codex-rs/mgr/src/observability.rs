use base64::Engine;
use rand::TryRngCore;
use sha2::Digest;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

pub(crate) fn init_tracing() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let layer = fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .compact();
        let subscriber = tracing_subscriber::registry()
            .with(layer)
            .with(tracing_subscriber::filter::LevelFilter::INFO);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

pub(crate) fn new_request_id() -> String {
    let mut bytes = [0u8; 16];
    let mut rng = rand::rngs::OsRng;
    if rng.try_fill_bytes(&mut bytes).is_ok() {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        return format!("req_{encoded}");
    }

    let now_ms = crate::time::now_ms();
    format!("req_{now_ms}")
}

pub(crate) fn hash_opaque_id(value: &str) -> String {
    let digest = sha256_bytes(value.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}

#[derive(Debug, Default)]
pub(crate) struct GatewayMetrics {
    pub(crate) requests_total: AtomicI64,
    pub(crate) requests_inflight: AtomicI64,
    pub(crate) requests_unauthorized_total: AtomicI64,
    pub(crate) requests_5xx_total: AtomicI64,
    pub(crate) redis_errors_total: AtomicI64,
    pub(crate) routing_errors_total: AtomicI64,
    pub(crate) token_errors_total: AtomicI64,
    pub(crate) upstream_requests_total: AtomicI64,
    pub(crate) upstream_errors_total: AtomicI64,
    pub(crate) upstream_responses_2xx_total: AtomicI64,
    pub(crate) upstream_responses_3xx_total: AtomicI64,
    pub(crate) upstream_responses_4xx_total: AtomicI64,
    pub(crate) upstream_responses_5xx_total: AtomicI64,
    pub(crate) upstream_latency_ms_sum: AtomicI64,
    pub(crate) upstream_latency_ms_count: AtomicI64,
    pub(crate) sse_streams_inflight: AtomicI64,
    pub(crate) sse_streams_total: AtomicI64,
    pub(crate) request_duration_ms_sum: AtomicI64,
    pub(crate) request_duration_ms_count: AtomicI64,
}

impl GatewayMetrics {
    pub(crate) fn render_prometheus(&self) -> String {
        let requests_total = self.requests_total.load(Ordering::Relaxed);
        let requests_inflight = self.requests_inflight.load(Ordering::Relaxed);
        let requests_unauthorized_total = self.requests_unauthorized_total.load(Ordering::Relaxed);
        let requests_5xx_total = self.requests_5xx_total.load(Ordering::Relaxed);
        let redis_errors_total = self.redis_errors_total.load(Ordering::Relaxed);
        let routing_errors_total = self.routing_errors_total.load(Ordering::Relaxed);
        let token_errors_total = self.token_errors_total.load(Ordering::Relaxed);
        let upstream_requests_total = self.upstream_requests_total.load(Ordering::Relaxed);
        let upstream_errors_total = self.upstream_errors_total.load(Ordering::Relaxed);
        let upstream_responses_2xx_total =
            self.upstream_responses_2xx_total.load(Ordering::Relaxed);
        let upstream_responses_3xx_total =
            self.upstream_responses_3xx_total.load(Ordering::Relaxed);
        let upstream_responses_4xx_total =
            self.upstream_responses_4xx_total.load(Ordering::Relaxed);
        let upstream_responses_5xx_total =
            self.upstream_responses_5xx_total.load(Ordering::Relaxed);
        let upstream_latency_ms_sum = self.upstream_latency_ms_sum.load(Ordering::Relaxed);
        let upstream_latency_ms_count = self.upstream_latency_ms_count.load(Ordering::Relaxed);
        let sse_streams_inflight = self.sse_streams_inflight.load(Ordering::Relaxed);
        let sse_streams_total = self.sse_streams_total.load(Ordering::Relaxed);
        let request_duration_ms_sum = self.request_duration_ms_sum.load(Ordering::Relaxed);
        let request_duration_ms_count = self.request_duration_ms_count.load(Ordering::Relaxed);

        format!(
            "\
# HELP codex_mgr_gateway_requests_total Total HTTP requests handled by the gateway.\n\
# TYPE codex_mgr_gateway_requests_total counter\n\
codex_mgr_gateway_requests_total {requests_total}\n\
# HELP codex_mgr_gateway_requests_inflight HTTP requests currently in flight (time-to-headers).\n\
# TYPE codex_mgr_gateway_requests_inflight gauge\n\
codex_mgr_gateway_requests_inflight {requests_inflight}\n\
# HELP codex_mgr_gateway_requests_unauthorized_total Requests rejected with 401.\n\
# TYPE codex_mgr_gateway_requests_unauthorized_total counter\n\
codex_mgr_gateway_requests_unauthorized_total {requests_unauthorized_total}\n\
# HELP codex_mgr_gateway_requests_5xx_total Requests returning 5xx.\n\
# TYPE codex_mgr_gateway_requests_5xx_total counter\n\
codex_mgr_gateway_requests_5xx_total {requests_5xx_total}\n\
# HELP codex_mgr_gateway_redis_errors_total Redis errors encountered in the data plane.\n\
# TYPE codex_mgr_gateway_redis_errors_total counter\n\
codex_mgr_gateway_redis_errors_total {redis_errors_total}\n\
# HELP codex_mgr_gateway_routing_errors_total Non-Redis routing errors.\n\
# TYPE codex_mgr_gateway_routing_errors_total counter\n\
codex_mgr_gateway_routing_errors_total {routing_errors_total}\n\
# HELP codex_mgr_gateway_token_errors_total Token/provider errors (non-Redis).\n\
# TYPE codex_mgr_gateway_token_errors_total counter\n\
codex_mgr_gateway_token_errors_total {token_errors_total}\n\
# HELP codex_mgr_gateway_upstream_requests_total Requests sent to upstream.\n\
# TYPE codex_mgr_gateway_upstream_requests_total counter\n\
codex_mgr_gateway_upstream_requests_total {upstream_requests_total}\n\
# HELP codex_mgr_gateway_upstream_errors_total Upstream transport/protocol errors.\n\
# TYPE codex_mgr_gateway_upstream_errors_total counter\n\
codex_mgr_gateway_upstream_errors_total {upstream_errors_total}\n\
# HELP codex_mgr_gateway_upstream_responses_2xx_total Upstream responses in the 2xx range.\n\
# TYPE codex_mgr_gateway_upstream_responses_2xx_total counter\n\
codex_mgr_gateway_upstream_responses_2xx_total {upstream_responses_2xx_total}\n\
# HELP codex_mgr_gateway_upstream_responses_3xx_total Upstream responses in the 3xx range.\n\
# TYPE codex_mgr_gateway_upstream_responses_3xx_total counter\n\
codex_mgr_gateway_upstream_responses_3xx_total {upstream_responses_3xx_total}\n\
# HELP codex_mgr_gateway_upstream_responses_4xx_total Upstream responses in the 4xx range.\n\
# TYPE codex_mgr_gateway_upstream_responses_4xx_total counter\n\
codex_mgr_gateway_upstream_responses_4xx_total {upstream_responses_4xx_total}\n\
# HELP codex_mgr_gateway_upstream_responses_5xx_total Upstream responses in the 5xx range.\n\
# TYPE codex_mgr_gateway_upstream_responses_5xx_total counter\n\
codex_mgr_gateway_upstream_responses_5xx_total {upstream_responses_5xx_total}\n\
# HELP codex_mgr_gateway_upstream_latency_ms_sum Upstream latency sum in ms (time-to-headers).\n\
# TYPE codex_mgr_gateway_upstream_latency_ms_sum counter\n\
codex_mgr_gateway_upstream_latency_ms_sum {upstream_latency_ms_sum}\n\
# HELP codex_mgr_gateway_upstream_latency_ms_count Upstream latency sample count.\n\
# TYPE codex_mgr_gateway_upstream_latency_ms_count counter\n\
codex_mgr_gateway_upstream_latency_ms_count {upstream_latency_ms_count}\n\
# HELP codex_mgr_gateway_sse_streams_inflight Current SSE streams in flight.\n\
# TYPE codex_mgr_gateway_sse_streams_inflight gauge\n\
codex_mgr_gateway_sse_streams_inflight {sse_streams_inflight}\n\
# HELP codex_mgr_gateway_sse_streams_total Total SSE streams started.\n\
# TYPE codex_mgr_gateway_sse_streams_total counter\n\
codex_mgr_gateway_sse_streams_total {sse_streams_total}\n\
# HELP codex_mgr_gateway_request_duration_ms_sum Request duration sum in ms (time-to-headers).\n\
# TYPE codex_mgr_gateway_request_duration_ms_sum counter\n\
codex_mgr_gateway_request_duration_ms_sum {request_duration_ms_sum}\n\
# HELP codex_mgr_gateway_request_duration_ms_count Request duration sample count.\n\
# TYPE codex_mgr_gateway_request_duration_ms_count counter\n\
codex_mgr_gateway_request_duration_ms_count {request_duration_ms_count}\n\
"
        )
    }
}
