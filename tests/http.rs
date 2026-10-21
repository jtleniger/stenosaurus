use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use stenosaurus::auth::AuthConfig;
use stenosaurus::routes::app;
use stenosaurus::state::AppState;
use stenosaurus::storage::DictationStore;

struct TestApp {
    router: Router,
    store: Arc<DictationStore>,
    _guard: tempfile::TempDir,
}

impl TestApp {
    async fn new() -> Self {
        let guard = tempfile::tempdir().unwrap();
        let store = Arc::new(
            DictationStore::new(guard.path().join("dictations"))
                .await
                .unwrap(),
        );
        let static_dir = guard.path().join("static");
        std::fs::create_dir_all(&static_dir).unwrap();
        std::fs::write(static_dir.join("app.js"), "// placeholder").unwrap();
        let model_path = guard.path().join("whistle.cact");
        std::fs::write(&model_path, b"test-model-bytes").unwrap();
        let auth = Arc::new(AuthConfig::from_password("alice", "correcthorse").unwrap());
        let state = AppState {
            store: store.clone(),
            auth,
            static_dir,
            model_path,
            secure_cookies: false,
        };
        Self {
            router: app(state),
            store,
            _guard: guard,
        }
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        content_type: Option<&str>,
        body: Option<Body>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let request = builder.body(body.unwrap_or_else(Body::empty)).unwrap();
        self.router.clone().oneshot(request).await.unwrap()
    }
}

async fn text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

async fn login(app: &TestApp) -> String {
    let response = app
        .request(
            "POST",
            "/login",
            None,
            Some("application/x-www-form-urlencoded"),
            Some(Body::from("username=alice&password=correcthorse")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("login must set a cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.contains("HttpOnly"), "cookie was {set_cookie}");
    let pair = set_cookie.split(';').next().unwrap().to_string();
    assert!(pair.starts_with("steno.sid="), "cookie was {pair}");
    pair
}

async fn save(app: &TestApp, cookie: &str, json: &str) -> axum::response::Response {
    app.request(
        "POST",
        "/api/dictations",
        Some(cookie),
        Some("application/json"),
        Some(Body::from(json.to_string())),
    )
    .await
}

#[tokio::test]
async fn unauthenticated_index_redirects_to_login() {
    let app = TestApp::new().await;
    let response = app.request("GET", "/", None, None, None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/login");
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let app = TestApp::new().await;
    let response = app
        .request(
            "POST",
            "/login",
            None,
            Some("application/x-www-form-urlencoded"),
            Some(Body::from("username=alice&password=wrongpassword")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = text(response).await;
    assert!(body.contains("Invalid username or password"), "body: {body}");
}

#[tokio::test]
async fn login_then_index_renders_recorder() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let response = app.request("GET", "/", Some(&cookie), None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(body.contains("No dictations yet"), "body: {body}");
    assert!(body.contains("id=\"live\""), "body: {body}");
    assert!(body.contains("/static/app.js"), "body: {body}");
}

#[tokio::test]
async fn api_requires_json_401_not_redirect() {
    let app = TestApp::new().await;
    let response = app
        .request(
            "POST",
            "/api/dictations",
            None,
            Some("application/json"),
            Some(Body::from(r#"{"text":"hi"}"#)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json"
    );
    let body = text(response).await;
    assert!(body.contains("Not signed in."), "body: {body}");
}

#[tokio::test]
async fn saves_and_shows_a_dictation() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let response = save(&app, &cookie, r#"{"text":"hello world"}"#).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json"
    );
    let body = text(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["text"], "hello world");
    let id = json["id"].as_str().unwrap().to_string();

    let list = app.store.list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    let content =
        std::fs::read_to_string(app.store.dir().join(format!("{id}.txt"))).unwrap();
    assert_eq!(content, "hello world\n");

    let detail = app
        .request("GET", &format!("/dictations/{id}"), Some(&cookie), None, None)
        .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let detail_body = text(detail).await;
    assert!(detail_body.contains("hello world"), "body: {detail_body}");
}

#[tokio::test]
async fn rejects_whitespace_only_text() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let response = save(&app, &cookie, r#"{"text":"   \n\t  "}"#).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = text(response).await;
    assert!(body.contains("No speech detected"), "body: {body}");
    assert!(app.store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_overlong_text() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let long = "a".repeat(200_001);
    let payload = serde_json::json!({ "text": long }).to_string();
    let response = save(&app, &cookie, &payload).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = text(response).await;
    assert!(body.contains("too long"), "body: {body}");
    assert!(app.store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_malformed_json() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let response = save(&app, &cookie, "not json").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json"
    );
    let body = text(response).await;
    assert!(body.contains("JSON body"), "body: {body}");
}

#[tokio::test]
async fn transcript_is_html_escaped() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let saved = app
        .store
        .save("<script>alert(1)</script>")
        .await
        .unwrap();

    let response = app
        .request(
            "GET",
            &format!("/dictations/{}", saved.id),
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    // Askama escapes HTML with numeric entities rather than named ones.
    assert!(
        body.contains("&#60;script&#62;alert(1)&#60;/script&#62;"),
        "body: {body}"
    );
    assert!(!body.contains("<script>alert(1)</script>"), "body: {body}");
}

#[tokio::test]
async fn unknown_dictation_is_404() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let response = app
        .request("GET", "/dictations/not-an-id", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn model_requires_login_and_streams_bytes() {
    let app = TestApp::new().await;
    let unauthenticated = app
        .request("GET", "/model/whistle.cact", None, None, None)
        .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthenticated.headers()[header::CONTENT_TYPE],
        "application/json"
    );

    let cookie = login(&app).await;
    let response = app
        .request("GET", "/model/whistle.cact", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(headers[header::CONTENT_LENGTH], "16");
    assert!(headers[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .starts_with("private"));
    assert_eq!(text(response).await, "test-model-bytes");
}

#[tokio::test]
async fn logout_invalidates_the_session() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let response = app
        .request("POST", "/logout", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/login");

    let after = app.request("GET", "/", Some(&cookie), None, None).await;
    assert_eq!(after.status(), StatusCode::SEE_OTHER);
    assert_eq!(after.headers()[header::LOCATION], "/login");
}

#[tokio::test]
async fn health_check_is_open() {
    let app = TestApp::new().await;
    let response = app.request("GET", "/healthz", None, None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(response).await, "ok");
}
