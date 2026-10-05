//! Loopback HTTP transport with explicit bearer authentication and bounded inputs.
use crate::{Application, Error, Submission, auth::Auth};
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
    auth: Arc<Auth>,
}

pub fn router(app: Arc<Application>, token: String) -> Result<Router, Error> {
    let auth = Auth::bearer(token).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(router_with_auth(app, auth))
}
pub fn router_with_auth(app: Arc<Application>, auth: Auth) -> Router {
    let state = Web {
        app,
        auth: Arc::new(auth),
    };
    let api = Router::new()
        .route("/config", get(config))
        .route("/resources", get(resources))
        .route("/permission-challenge", post(permission_challenge))
        .route("/capabilities", get(capabilities))
        .route("/capabilities/{name}/refresh", post(refresh_capabilities))
        .route("/status", get(status))
        .route("/tasks", get(list).post(submit))
        .route("/tasks/{id}", get(detail))
        .route("/tasks/{id}/operator", get(operator))
        .route("/tasks/{id}/cancel", post(cancel))
        .route("/tasks/{id}/retry", post(retry))
        .route(
            "/tasks/{id}/replacement-challenge",
            post(replacement_challenge),
        )
        .route("/tasks/{id}/continue-review", post(continue_review))
        .layer(DefaultBodyLimit::max(96 * 1024))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .route("/", get(index))
        .route("/auth/status", get(auth_status))
        .route(
            "/auth/login",
            post(login).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/auth/logout", post(logout))
        .nest("/api", api)
        .with_state(state)
        .layer(middleware::from_fn(security_headers))
}
async fn authorize(State(state): State<Web>, request: Request, next: Next) -> Response {
    if !state.auth.authorized(request.headers(), request.method()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"authentication required"})),
        )
            .into_response();
    }
    next.run(request).await
}
async fn auth_status(State(state): State<Web>, headers: axum::http::HeaderMap) -> Json<Value> {
    Json(json!({"mode":state.auth.mode(), "authenticated":state.auth.session_valid(&headers)}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    username: String,
    password: String,
}
async fn login(
    State(state): State<Web>,
    headers: axum::http::HeaderMap,
    Json(input): Json<Login>,
) -> Response {
    let auth = state.auth.clone();
    let result = tokio::task::spawn_blocking(move || {
        let password = zeroize::Zeroizing::new(input.password);
        auth.login(&input.username, &password, &headers)
    })
    .await
    .unwrap_or(Err(503));
    auth_response(result, true)
}
async fn logout(State(state): State<Web>, headers: axum::http::HeaderMap) -> Response {
    auth_response(state.auth.logout(&headers), false)
}
fn auth_response(result: Result<String, u16>, authenticated: bool) -> Response {
    match result {
        Ok(cookie) => (
            [(header::SET_COOKIE, cookie)],
            Json(json!({"authenticated":authenticated})),
        )
            .into_response(),
        Err(code) => {
            let mut response = (
                StatusCode::from_u16(code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                Json(json!({"error":"authentication unavailable or credentials invalid"})),
            )
                .into_response();
            if code == 429 {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            }
            response
        }
    }
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceQuery {
    repository: String,
    workflow: Option<String>,
    reviewer_profile: Option<String>,
}
async fn resources(
    State(state): State<Web>,
    Query(query): Query<ResourceQuery>,
) -> Result<Json<Value>, ApiError> {
    let result = tokio::task::spawn_blocking(move || {
        state.app.resource_estimate_with_reviewer(
            &query.repository,
            query.workflow.as_deref(),
            query.reviewer_profile.as_deref(),
        )
    })
    .await
    .map_err(|_| Error::Poisoned)??;
    Ok(Json(json!(result)))
}
async fn operator(State(state): State<Web>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        tokio::task::spawn_blocking(move || state.app.operator(id))
            .await
            .map_err(|_| Error::Poisoned)??,
    ))
}
async fn capabilities(State(state): State<Web>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"profiles": state.app.capabilities()?})))
}
async fn refresh_capabilities(
    State(state): State<Web>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let result = tokio::task::spawn_blocking(move || state.app.refresh_capabilities(&name))
        .await
        .map_err(|_| Error::Poisoned)??;
    Ok(Json(json!(result)))
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
) -> Result<Json<Vec<crate::TaskView>>, ApiError> {
    Ok(Json(state.app.list_views(page.before)?))
}
async fn detail(
    State(state): State<Web>,
    Path(id): Path<i64>,
) -> Result<Json<crate::TaskView>, ApiError> {
    Ok(Json(state.app.get_view(id)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionChallengeInput {
    job: crate::host::Job,
}
async fn permission_challenge(
    State(state): State<Web>,
    Json(input): Json<PermissionChallengeInput>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.app.permission_challenge(input.job)?))
}
async fn submit(
    State(state): State<Web>,
    Json(input): Json<Submission>,
) -> Result<(StatusCode, Json<relay::Task>), ApiError> {
    Ok((StatusCode::CREATED, Json(state.app.submit(input)?)))
}
async fn replacement_challenge(
    State(state): State<Web>,
    Path(id): Path<i64>,
    Json(input): Json<crate::ReplacementChallengeRequest>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.app.replacement_challenge(id, input)?))
}
async fn retry(
    State(state): State<Web>,
    Path(id): Path<i64>,
    Json(input): Json<crate::RetryRequest>,
) -> Result<(StatusCode, Json<relay::Task>), ApiError> {
    Ok((StatusCode::CREATED, Json(state.app.retry(id, input)?)))
}
async fn cancel(State(state): State<Web>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.app.cancel(id)?))
}
async fn continue_review(
    State(state): State<Web>,
    Path(id): Path<i64>,
    Json(input): Json<crate::ReviewContinuationRequest>,
) -> Result<(StatusCode, Json<relay::Task>), ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(state.app.continue_review(id, input)?),
    ))
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
            | Error::RecoveryRequired
            | Error::DiscoveryUnavailable(_)
            | Error::ActionUnavailable { .. } => StatusCode::CONFLICT,
            Error::Core(relay::Error::Invalid(_)) | Error::Invalid(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            eprintln!("API: {}", self.0);
            "internal error".to_string()
        } else {
            self.0.to_string()
        };
        let failure = match &self.0 {
            Error::ActionUnavailable { code, cause } => {
                Some(crate::resources::Failure::new(code, "request", cause))
            }
            Error::Invalid(cause) => Some(crate::resources::Failure::new(
                "invalid_request",
                "request",
                cause,
            )),
            _ => None,
        };
        (status, Json(json!({"error":message,"failure":failure}))).into_response()
    }
}
