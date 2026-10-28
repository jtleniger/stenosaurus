use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use stenosaurus::auth::AuthConfig;
use stenosaurus::db::Database;
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
        let db = Database::open(guard.path()).await.unwrap();
        let store = Arc::new(DictationStore::new(db));
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

async fn create_draft(app: &TestApp, cookie: &str) -> i64 {
    let response = app
        .request("POST", "/api/drafts", Some(cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = text(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    json["id"].as_i64().expect("draft id")
}

async fn update_draft(
    app: &TestApp,
    cookie: &str,
    id: i64,
    json: &str,
) -> axum::response::Response {
    app.request(
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(cookie),
        Some("application/json"),
        Some(Body::from(json.to_string())),
    )
    .await
}

async fn save_draft(app: &TestApp, cookie: &str, id: i64, json: &str) -> axum::response::Response {
    app.request(
        "POST",
        &format!("/api/drafts/{id}/save"),
        Some(cookie),
        Some("application/json"),
        Some(Body::from(json.to_string())),
    )
    .await
}

async fn add_note(app: &TestApp, cookie: &str, content: &str) -> String {
    let id = create_draft(app, cookie).await;
    let updated = update_draft(
        app,
        cookie,
        id,
        &serde_json::json!({ "text": content }).to_string(),
    )
    .await;
    assert_eq!(updated.status(), StatusCode::OK);
    let response = save_draft(
        app,
        cookie,
        id,
        &serde_json::json!({ "text": content }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    json["id"].as_str().expect("note id").to_string()
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
    assert!(
        body.contains("Invalid username or password"),
        "body: {body}"
    );
}

#[tokio::test]
async fn home_page_has_nav_recorder_and_search() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let response = app.request("GET", "/", Some(&cookie), None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(body.contains("id=\"dictate\""), "body: {body}");
    assert!(body.contains("id=\"live\""), "body: {body}");
    assert!(body.contains("action=\"/search\""), "body: {body}");
    assert!(body.contains("href=\"/notes\""), "body: {body}");
    assert!(body.contains("href=\"/drafts\""), "body: {body}");
    assert!(body.contains("alice"), "body: {body}");
    assert!(body.contains("action=\"/logout\""), "body: {body}");
    assert!(body.contains("/static/app.js"), "body: {body}");
    assert!(!body.contains("No notes yet"), "body: {body}");
    assert!(!body.contains("No drafts yet"), "body: {body}");
}

#[tokio::test]
async fn notes_and_drafts_pages_list_their_contents() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let notes = app
        .request("GET", "/notes", Some(&cookie), None, None)
        .await;
    assert_eq!(notes.status(), StatusCode::OK);
    let notes_body = text(notes).await;
    assert!(notes_body.contains("No notes yet"), "body: {notes_body}");

    let drafts = app
        .request("GET", "/drafts", Some(&cookie), None, None)
        .await;
    assert_eq!(drafts.status(), StatusCode::OK);
    let drafts_body = text(drafts).await;
    assert!(drafts_body.contains("No drafts yet"), "body: {drafts_body}");
}

#[tokio::test]
async fn api_requires_json_401_not_redirect() {
    let app = TestApp::new().await;
    let response = app.request("POST", "/api/drafts", None, None, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body = text(response).await;
    assert!(body.contains("Not signed in."), "body: {body}");
}

#[tokio::test]
async fn drafts_autosave_without_creating_notes_or_search_hits() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let id = create_draft(&app, &cookie).await;
    let response = update_draft(
        &app,
        &cookie,
        id,
        r#"{"name":"Groceries","text":"hello draft"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(body.contains("Groceries"), "body: {body}");
    assert!(body.contains("hello draft"), "body: {body}");

    // Autosaved text lives only in the draft, not in notes or search results.
    assert!(app.store.list().await.unwrap().is_empty());
    let search = app
        .request("GET", "/search?q=hello", Some(&cookie), None, None)
        .await;
    assert_eq!(search.status(), StatusCode::OK);
    let search_body = text(search).await;
    assert!(
        search_body.contains("No notes match"),
        "body: {search_body}"
    );

    let recorder = app
        .request("GET", &format!("/?draft={id}"), Some(&cookie), None, None)
        .await;
    assert_eq!(recorder.status(), StatusCode::OK);
    let recorder_body = text(recorder).await;
    assert!(
        recorder_body.contains("value=\"Groceries\""),
        "body: {recorder_body}"
    );
    assert!(
        recorder_body.contains("hello draft"),
        "body: {recorder_body}"
    );

    let notes = app
        .request("GET", "/notes", Some(&cookie), None, None)
        .await;
    let notes_body = text(notes).await;
    assert!(notes_body.contains("No notes yet"), "body: {notes_body}");
}

#[tokio::test]
async fn saving_a_draft_promotes_it_to_a_note() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let id = create_draft(&app, &cookie).await;
    assert_eq!(
        update_draft(&app, &cookie, id, r#"{"text":"old autosave"}"#)
            .await
            .status(),
        StatusCode::OK
    );

    // The client sends its full text so a failed autosave still saves correctly.
    let response = save_draft(&app, &cookie, id, r#"{"text":"hello world"}"#).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body = text(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["text"], "hello world");
    let note_id = json["id"].as_str().unwrap().to_string();

    assert!(app.store.get_draft(id).await.unwrap().is_none());
    let list = app.store.list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, note_id);
    assert_eq!(
        app.store.get(&note_id).await.unwrap().unwrap().text,
        "hello world"
    );

    let detail = app
        .request(
            "GET",
            &format!("/dictations/{note_id}"),
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let detail_body = text(detail).await;
    assert!(detail_body.contains("hello world"), "body: {detail_body}");
}

#[tokio::test]
async fn rejects_saving_an_empty_draft() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let id = create_draft(&app, &cookie).await;

    for payload in ["{}", r#"{"text":"   \n\t  "}"#] {
        let response = save_draft(&app, &cookie, id, payload).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = text(response).await;
        assert!(body.contains("Draft is empty"), "body: {body}");
    }
    assert!(app.store.list().await.unwrap().is_empty());
    assert!(app.store.get_draft(id).await.unwrap().is_some());
}

#[tokio::test]
async fn rejects_overlong_draft_text() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let id = create_draft(&app, &cookie).await;

    let long = "a".repeat(200_001);
    let payload = serde_json::json!({ "text": long }).to_string();
    let response = update_draft(&app, &cookie, id, &payload).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = text(response).await;
    assert!(body.contains("too long"), "body: {body}");
    assert!(app
        .store
        .get_draft(id)
        .await
        .unwrap()
        .unwrap()
        .text
        .is_empty());
}

#[tokio::test]
async fn rejects_malformed_draft_json() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let id = create_draft(&app, &cookie).await;

    let response = save_draft(&app, &cookie, id, "not json").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body = text(response).await;
    assert!(body.contains("JSON body"), "body: {body}");
}

#[tokio::test]
async fn discards_a_draft() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let id = create_draft(&app, &cookie).await;
    assert_eq!(
        update_draft(&app, &cookie, id, r#"{"text":"throw me away"}"#)
            .await
            .status(),
        StatusCode::OK
    );

    let response = app
        .request(
            "DELETE",
            &format!("/api/drafts/{id}"),
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(app.store.get_draft(id).await.unwrap().is_none());
    let recorder = app
        .request("GET", &format!("/?draft={id}"), Some(&cookie), None, None)
        .await;
    assert_eq!(recorder.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn multiple_drafts_are_listed_and_selectable() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let first = create_draft(&app, &cookie).await;
    let second = create_draft(&app, &cookie).await;
    assert_eq!(
        update_draft(&app, &cookie, first, r#"{"name":"First","text":"alpha"}"#)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        update_draft(&app, &cookie, second, r#"{"name":"Second","text":"beta"}"#)
            .await
            .status(),
        StatusCode::OK
    );

    let recorder = app
        .request(
            "GET",
            &format!("/?draft={first}"),
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(recorder.status(), StatusCode::OK);
    let body = text(recorder).await;
    assert!(body.contains("value=\"First\""), "body: {body}");
    assert!(body.contains("alpha"), "body: {body}");

    let drafts = app
        .request("GET", "/drafts", Some(&cookie), None, None)
        .await;
    assert_eq!(drafts.status(), StatusCode::OK);
    let drafts_body = text(drafts).await;
    assert!(drafts_body.contains("First"), "body: {drafts_body}");
    assert!(drafts_body.contains("Second"), "body: {drafts_body}");
    assert!(
        drafts_body.contains(&format!("/?draft={second}")),
        "body: {drafts_body}"
    );

    assert_eq!(app.store.list_drafts().await.unwrap().len(), 2);
}

#[tokio::test]
async fn unknown_draft_endpoints_are_404() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;

    let update = update_draft(&app, &cookie, 999, r#"{"text":"x"}"#).await;
    assert_eq!(update.status(), StatusCode::NOT_FOUND);
    let save = save_draft(&app, &cookie, 999, r#"{"text":"x"}"#).await;
    assert_eq!(save.status(), StatusCode::NOT_FOUND);
    let delete = app
        .request("DELETE", "/api/drafts/999", Some(&cookie), None, None)
        .await;
    assert_eq!(delete.status(), StatusCode::NOT_FOUND);
    let recorder = app
        .request("GET", "/?draft=999", Some(&cookie), None, None)
        .await;
    assert_eq!(recorder.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn transcript_is_html_escaped() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let saved = app.store.save("<script>alert(1)</script>").await.unwrap();

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
async fn search_requires_login() {
    let app = TestApp::new().await;
    let response = app
        .request("GET", "/search?q=hello", None, None, None)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/login");
}

#[tokio::test]
async fn search_finds_and_highlights_dictation() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    add_note(&app, &cookie, "hello world").await;
    add_note(&app, &cookie, "goodbye").await;

    let response = app
        .request("GET", "/search?q=HELLO", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(body.contains("<mark>hello</mark> world"), "body: {body}");
    assert!(!body.contains("goodbye"), "body: {body}");
}

#[tokio::test]
async fn search_requires_every_term() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    add_note(&app, &cookie, "alpha beta").await;
    add_note(&app, &cookie, "alpha only").await;

    let response = app
        .request("GET", "/search?q=alpha%20beta", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(
        body.contains("<mark>alpha</mark> <mark>beta</mark>"),
        "body: {body}"
    );
    assert!(!body.contains("only"), "body: {body}");
}

#[tokio::test]
async fn search_with_blank_query_redirects_home() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let response = app
        .request("GET", "/search?q=%20", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/");
}

#[tokio::test]
async fn search_escapes_transcript_html() {
    let app = TestApp::new().await;
    let cookie = login(&app).await;
    let saved = app.store.save("<script>alert(1)</script>").await.unwrap();
    assert_eq!(saved.text, "<script>alert(1)</script>");

    let response = app
        .request("GET", "/search?q=alert", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(
        body.contains("&#60;script&#62;<mark>alert</mark>(1)&#60;/script&#62;"),
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
    assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
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
