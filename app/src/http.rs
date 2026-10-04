//! Loopback HTTP transport with explicit bearer authentication and bounded inputs.
use crate::{Application, Error, Submission};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct Web {
    app: Arc<Application>,
    token: Arc<String>,
}

pub fn router(app: Arc<Application>, token: String) -> Result<Router, Error> {
    if token.len() < 32 || token.len() > 256 || !token.bytes().all(|c| c.is_ascii_graphic()) {
        return Err(Error::Invalid(
            "RELAY_TOKEN must be 32–256 non-whitespace ASCII bytes".into(),
        ));
    }
    let state = Web {
        app,
        token: Arc::new(token),
    };
    let api = Router::new()
        .route("/config", get(config))
        .route("/status", get(status))
        .route("/tasks", get(list).post(submit))
        .route("/tasks/{id}", get(detail))
        .route("/tasks/{id}/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(96 * 1024))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Ok(Router::new()
        .route("/", get(index))
        .nest("/api", api)
        .with_state(state)
        .layer(middleware::from_fn(security_headers)))
}
async fn authorize(State(state): State<Web>, request: Request, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected = state.token.as_bytes();
    let supplied = supplied.as_bytes();
    let mut mismatch = expected.len() ^ supplied.len();
    for (index, byte) in expected.iter().enumerate() {
        mismatch |= (*byte ^ supplied.get(index).copied().unwrap_or(0)) as usize;
    }
    if mismatch != 0 {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"authentication required"})),
        )
            .into_response();
    }
    next.run(request).await
}
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}
async fn index() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(include_str!("../static/index.html")))
        .expect("static response")
}
async fn config(State(state): State<Web>) -> Json<Value> {
    Json(state.app.public_config())
}
async fn status(State(state): State<Web>) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.app.status()?))
}
#[derive(Deserialize)]
struct Page {
    before: Option<i64>,
}
async fn list(
    State(state): State<Web>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<relay::Task>>, ApiError> {
    Ok(Json(state.app.list(page.before)?))
}
async fn detail(
    State(state): State<Web>,
    Path(id): Path<i64>,
) -> Result<Json<relay::Task>, ApiError> {
    Ok(Json(state.app.get(id)?))
}
async fn submit(
    State(state): State<Web>,
    Json(input): Json<Submission>,
) -> Result<(StatusCode, Json<relay::Task>), ApiError> {
    Ok((StatusCode::CREATED, Json(state.app.submit(input)?)))
}
async fn cancel(State(state): State<Web>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.app.cancel(id)?))
}
struct ApiError(Error);
impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::Core(relay::Error::NotFound) => StatusCode::NOT_FOUND,
            Error::Core(relay::Error::IdempotencyConflict | relay::Error::StaleClaim)
            | Error::RecoveryRequired => StatusCode::CONFLICT,
            Error::Core(relay::Error::Invalid(_)) | Error::Invalid(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            eprintln!("API: {}", self.0);
            "internal error".to_string()
        } else {
            self.0.to_string()
        };
        (status, Json(json!({"error":message}))).into_response()
    }
}
