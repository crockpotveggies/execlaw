//! Admin HTTP surface for the inference observability page (M5).
//!
//! Single endpoint:
//!
//!   `GET /api/admin/inference/metrics` — JSON snapshot of the
//!   per-consumer inference metrics (in_flight, total_calls,
//!   total_failures, p50/p95 latency). Backs the `/admin/inference`
//!   SPA page.
//!
//! Read-only; no mutations. Anyone with controller auth can hit it.

use crate::inference_metrics::MetricsSnapshot;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/admin/inference/metrics", get(metrics))
}

#[utoipa::path(
    get,
    path = "/api/admin/inference/metrics",
    responses((status = 200, description = "Per-consumer inference call counters + p50/p95 latencies", body = MetricsSnapshot)),
    security(("bearer_jwt" = [])),
    tag = "inference"
)]
pub async fn metrics(State(state): State<AppState>) -> Result<Json<MetricsSnapshot>, ApiError> {
    Ok(Json(state.inference_metrics.snapshot()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_metrics::InferenceConsumer;
    use crate::routes::test_app_state;
    use axum::body::{self, Body};
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt;

    async fn observe_ok(state: &AppState, c: InferenceConsumer) {
        state
            .inference_metrics
            .observe::<_, &'static str, _>(c, async { Ok::<_, &'static str>("ok") })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn metrics_returns_empty_consumers_on_fresh_state() {
        let state = test_app_state();
        let app = router().with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/inference/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["consumers"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn metrics_reflects_observed_consumers() {
        let state = test_app_state();
        observe_ok(&state, InferenceConsumer::Chat).await;
        observe_ok(&state, InferenceConsumer::Automations).await;
        observe_ok(&state, InferenceConsumer::Automations).await;
        let app = router().with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/inference/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let consumers = v["consumers"].as_array().unwrap();
        assert_eq!(consumers.len(), 2);
        let auto = consumers
            .iter()
            .find(|c| c["consumer"] == "automations")
            .unwrap();
        let chat = consumers.iter().find(|c| c["consumer"] == "chat").unwrap();
        assert_eq!(auto["total_calls"], 2);
        assert_eq!(chat["total_calls"], 1);
    }
}
