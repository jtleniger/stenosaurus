//! HTTP surface: pages, JSON endpoints, model download and security headers.

use askama::Template;
use askama_web::WebTemplate;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, Request, State};
use axum::http::header::{self, HeaderValue};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tower_sessions::cookie::SameSite;
use tower_sessions::{Expiry, MemoryStore, Session, SessionManagerLayer};

use crate::auth::SESSION_KEY;
use crate::error::{ApiError, PageError};
use crate::state::AppState;
use crate::storage::{DictationSummary, SearchHit};

const MAX_TEXT_CHARS: usize = 200_000;
const MAX_SEARCH_RESULTS: usize = 100;

const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
worker-src 'self'; connect-src 'self'; img-src 'self'; style-src 'self'; \
base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'";

#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
struct IndexTemplate {
    username: String,
    dictations: Vec<DictationSummary>,
    empty: bool,
    query: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "dictation.html")]
struct DictationTemplate {
    iso: String,
    display: String,
    text: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "search.html")]
struct SearchTemplate {
    username: String,
    query: String,
    hits: Vec<SearchHit>,
    total: usize,
    empty: bool,
    truncated: bool,
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
}

#[derive(Deserialize)]
struct NewDictation {
    text: String,
}

#[derive(serde::Serialize)]
struct DictationResponse {
    id: String,
    text: String,
}

/// Page requests redirect to the login form when unauthenticated.
pub struct AuthedUser(#[allow(dead_code)] pub String);

impl FromRequestParts<AppState> for AuthedUser {
    type Rejection = Redirect;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| Redirect::to("/login"))?;
        match session.get::<String>(SESSION_KEY).await {
            Ok(Some(username)) => Ok(AuthedUser(username)),
            Ok(None) => Err(Redirect::to("/login")),
            Err(err) => {
                tracing::error!(%err, "could not read session");
                Err(Redirect::to("/login"))
            }
        }
    }
}

/// API requests get a JSON 401 instead of an HTML redirect.
pub struct ApiUser(#[allow(dead_code)] pub String);

impl FromRequestParts<AppState> for ApiUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::unauthorized())?;
        match session.get::<String>(SESSION_KEY).await {
            Ok(Some(username)) => Ok(ApiUser(username)),
            Ok(None) => Err(ApiError::unauthorized()),
            Err(err) => {
                tracing::error!(%err, "could not read session");
                Err(ApiError::unauthorized())
            }
        }
    }
}

async fn index(
    State(state): State<AppState>,
    AuthedUser(username): AuthedUser,
) -> Result<IndexTemplate, PageError> {
    let dictations = state.store.list().await.map_err(|err| {
        tracing::error!(%err, "could not list dictations");
        PageError::internal()
    })?;
    let empty = dictations.is_empty();
    Ok(IndexTemplate {
        username,
        dictations,
        empty,
        query: String::new(),
    })
}

async fn login_page(session: Session) -> Result<Response, PageError> {
    let signed_in = session
        .get::<String>(SESSION_KEY)
        .await
        .map_err(|err| {
            tracing::error!(%err, "could not read session");
            PageError::internal()
        })?
        .is_some();
    if signed_in {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(LoginTemplate {
        error: String::new(),
    }
    .into_response())
}

async fn login_submit(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<LoginForm>,
) -> Result<Response, PageError> {
    if !state.auth.verify(&form.username, &form.password) {
        return Ok((
            StatusCode::UNAUTHORIZED,
            LoginTemplate {
                error: "Invalid username or password.".to_string(),
            },
        )
            .into_response());
    }
    session.cycle_id().await.map_err(|err| {
        tracing::error!(%err, "could not cycle session id");
        PageError::internal()
    })?;
    session
        .insert(SESSION_KEY, form.username)
        .await
        .map_err(|err| {
            tracing::error!(%err, "could not persist session");
            PageError::internal()
        })?;
    Ok(Redirect::to("/").into_response())
}

async fn logout(AuthedUser(_): AuthedUser, session: Session) -> Result<Redirect, PageError> {
    session.flush().await.map_err(|err| {
        tracing::error!(%err, "could not flush session");
        PageError::internal()
    })?;
    Ok(Redirect::to("/login"))
}

async fn show_dictation(
    State(state): State<AppState>,
    AuthedUser(_): AuthedUser,
    Path(id): Path<String>,
) -> Result<DictationTemplate, PageError> {
    let Some(dictation) = state.store.get(&id).await.map_err(|err| {
        tracing::error!(%err, "could not read dictation");
        PageError::internal()
    })? else {
        return Err(PageError::not_found());
    };
    Ok(DictationTemplate {
        iso: dictation.iso,
        display: dictation.display,
        text: dictation.text,
    })
}

async fn search(
    State(state): State<AppState>,
    AuthedUser(username): AuthedUser,
    Query(params): Query<SearchParams>,
) -> Result<Response, PageError> {
    let query = params.q.unwrap_or_default();
    let query = query.trim();
    if query.is_empty() {
        return Ok(Redirect::to("/").into_response());
    }
    let results = state
        .store
        .search(query, MAX_SEARCH_RESULTS)
        .await
        .map_err(|err| {
            tracing::error!(%err, "could not search dictations");
            PageError::internal()
        })?;
    let truncated = results.total > results.hits.len();
    let empty = results.hits.is_empty();
    Ok(SearchTemplate {
        username,
        query: query.to_string(),
        hits: results.hits,
        total: results.total,
        empty,
        truncated,
    }
    .into_response())
}

async fn save_dictation(
    State(state): State<AppState>,
    ApiUser(_): ApiUser,
    payload: Result<Json<NewDictation>, JsonRejection>,
) -> Result<Json<DictationResponse>, ApiError> {
    let Json(payload) =
        payload.map_err(|_| ApiError::bad_request("Expected a JSON body with a text field."))?;
    if payload.text.trim().is_empty() {
        return Err(ApiError::unprocessable("No speech detected. Try again."));
    }
    if payload.text.chars().count() > MAX_TEXT_CHARS {
        return Err(ApiError::unprocessable("Dictation is too long."));
    }
    let saved = state.store.save(&payload.text).await.map_err(|err| {
        tracing::error!(%err, "could not save dictation");
        ApiError::internal()
    })?;
    Ok(Json(DictationResponse {
        id: saved.id,
        text: saved.text,
    }))
}

async fn model(State(state): State<AppState>, ApiUser(_): ApiUser) -> Result<Response, ApiError> {
    let file = tokio::fs::File::open(&state.model_path).await.map_err(|err| {
        tracing::error!(%err, path = %state.model_path.display(), "could not open model");
        ApiError::internal()
    })?;
    let length = file
        .metadata()
        .await
        .map(|meta| meta.len())
        .map_err(|err| {
            tracing::error!(%err, path = %state.model_path.display(), "could not stat model");
            ApiError::internal()
        })?;

    let mut response = Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(file)));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok(response)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn not_found() -> PageError {
    PageError::not_found()
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

pub fn app(state: AppState) -> Router {
    let session_layer = SessionManagerLayer::new(MemoryStore::default())
        .with_name("steno.sid")
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(state.secure_cookies)
        .with_expiry(Expiry::OnInactivity(time::Duration::hours(12)));
    let static_dir = state.static_dir.clone();

    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/dictations/{id}", get(show_dictation))
        .route("/search", get(search))
        .route("/api/dictations", post(save_dictation))
        .route("/model/whistle.cact", get(model))
        .route("/healthz", get(healthz))
        .nest_service("/static", ServeDir::new(static_dir))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(1_048_576))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn(security_headers))
        .layer(session_layer)
        .with_state(state)
}
