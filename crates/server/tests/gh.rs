use git_fight_server::gh::GitHub;
use serde_json::json;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_PEM: &str = include_str!("fixtures/app_key.txt");
const SECRET: &str = "oauth-client-secret-do-not-leak";

fn client(mock: &MockServer) -> GitHub {
    GitHub::new(
        mock.uri(),
        mock.uri(),
        1,
        APP_PEM.to_string(),
        "cid".into(),
        SECRET.into(),
    )
}

#[tokio::test]
async fn installation_token_is_cached_in_memory() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_cached_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "size": 12,
            "default_branch": "main"
        })))
        .mount(&mock)
        .await;
    let gh = client(&mock);
    let a = gh.get_repo(1, "acme", "box").await.unwrap();
    let b = gh.get_repo(1, "acme", "box").await.unwrap();
    assert_eq!(a.size, 12);
    assert_eq!(b.size, 12);
}

#[tokio::test]
async fn installation_token_rejects_huge_json() {
    let mock = MockServer::start().await;
    let huge = format!(
        r#"{{"token":"{}","expires_at":"2099-01-01T00:00:00Z"}}"#,
        "a".repeat(32 * 1024)
    );
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_string(huge))
        .expect(1)
        .mount(&mock)
        .await;
    let gh = client(&mock);
    assert!(gh.installation_token(1).await.is_err());
}

#[tokio::test]
async fn poll_mergeable_gives_up_with_none() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_cached_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 1,
            "mergeable": null,
            "head": { "sha": "aaa", "ref": "pr" },
            "base": { "sha": "bbb", "ref": "main" },
            "user": { "login": "alice" }
        })))
        .mount(&mock)
        .await;
    let gh = client(&mock).with_poll_wait(Duration::from_millis(1));
    let pr = gh.poll_mergeable(1, "acme", "box", 1).await.unwrap();
    assert_eq!(pr.mergeable, None);
    let pulls = mock
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "GET" && r.url.path() == "/repos/acme/box/pulls/1")
        .count();
    assert_eq!(pulls, 8, "short cap is eight mergeable polls");
}

#[test]
fn debug_omits_secrets() {
    let gh = GitHub::new(
        "http://example.test".into(),
        "http://example.test".into(),
        1,
        APP_PEM.to_string(),
        "cid".into(),
        SECRET.into(),
    );
    let shown = format!("{gh:?}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(!shown.contains("BEGIN"), "{shown}");
    assert!(!shown.contains("PRIVATE KEY"), "{shown}");
}

#[test]
fn github_names_reject_host_tricks() {
    use git_fight_server::gh::is_safe_github_name;
    assert!(is_safe_github_name("acme"));
    assert!(is_safe_github_name("git-fight"));
    assert!(is_safe_github_name(".github"));
    assert!(!is_safe_github_name(""));
    assert!(!is_safe_github_name("acme/other"));
    assert!(!is_safe_github_name("acme.git@evil"));
    assert!(!is_safe_github_name("../acme"));
    assert!(!is_safe_github_name("acme/../x"));
    assert!(!is_safe_github_name("https://github.com"));
}

#[tokio::test]
async fn github_api_rejects_unsafe_owner_before_http() {
    let gh = GitHub::new(
        "http://example.test".into(),
        "http://example.test".into(),
        1,
        APP_PEM.to_string(),
        "cid".into(),
        SECRET.into(),
    );
    assert!(gh.get_repo(1, "acme/other", "box").await.is_err());
    assert!(gh.get_pull(1, "acme", "box.git@evil", 1).await.is_err());
    assert!(gh.comment(1, "../acme", "box", 1, "hi").await.is_err());
    assert!(gh.edit_comment(1, "acme", "..", 99, "hi").await.is_err());
}

#[tokio::test]
async fn login_for_commit_rejects_option_shas() {
    let gh = GitHub::new(
        "http://example.test".into(),
        "http://example.test".into(),
        1,
        APP_PEM.to_string(),
        "cid".into(),
        SECRET.into(),
    );
    assert!(gh
        .login_for_commit(1, "acme", "box", "HEAD")
        .await
        .is_none());
    assert!(gh
        .login_for_commit(1, "acme", "box", "--upload-pack=true")
        .await
        .is_none());
    assert!(gh
        .login_for_commit(
            1,
            "acme/other",
            "box",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        .await
        .is_none());
}

#[tokio::test]
async fn auto_challenge_skips_unsafe_ref() {
    let gh = GitHub::new(
        "http://example.test".into(),
        "http://example.test".into(),
        1,
        APP_PEM.to_string(),
        "cid".into(),
        SECRET.into(),
    );
    assert!(
        !gh.auto_challenge_enabled(1, "acme", "box", "--upload-pack=true")
            .await
    );
    assert!(!gh.auto_challenge_enabled(1, "acme", "box", "../main").await);
}

async fn auto_challenge_with(body: serde_json::Value) -> bool {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_cached_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/contents/.github/git-fight.yml"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&mock)
        .await;
    client(&mock)
        .auto_challenge_enabled(1, "acme", "box", "main")
        .await
}

#[tokio::test]
async fn auto_challenge_requires_small_base64_file() {
    // `auto_challenge: true\n` as GitHub contents (base64, optional wrap newline).
    let b64 = "YXV0b19jaGFsbGVuZ2U6IHRydWUK";
    assert!(
        auto_challenge_with(json!({
            "type": "file",
            "encoding": "base64",
            "size": 21,
            "content": format!("{b64}\n"),
        }))
        .await
    );
    assert!(
        !auto_challenge_with(json!({
            "type": "symlink",
            "encoding": "base64",
            "size": 21,
            "content": b64,
        }))
        .await
    );
    assert!(
        !auto_challenge_with(json!({
            "type": "file",
            "encoding": "utf-8",
            "size": 21,
            "content": "auto_challenge: true\n",
        }))
        .await
    );
    assert!(
        !auto_challenge_with(json!({
            "encoding": "base64",
            "size": 21,
            "content": b64,
        }))
        .await
    );
    assert!(
        !auto_challenge_with(json!({
            "type": "file",
            "encoding": "base64",
            "size": 1_000_000,
            "content": b64,
        }))
        .await
    );
    assert!(!auto_challenge_with(json!([{ "type": "file" }])).await);
}

#[tokio::test]
async fn login_for_email_rejects_injection() {
    let gh = GitHub::new(
        "http://example.test".into(),
        "http://example.test".into(),
        1,
        APP_PEM.to_string(),
        "cid".into(),
        SECRET.into(),
    );
    assert!(gh
        .login_for_email(1, "acme", "box", "x@y.com&per_page=100")
        .await
        .is_none());
    assert!(gh
        .login_for_email(1, "acme", "box", "x@y.com\nAuthorization: bearer x")
        .await
        .is_none());
    assert!(gh
        .login_for_email(1, "acme/other", "box", "bob@example.com")
        .await
        .is_none());
}

#[tokio::test]
async fn login_for_email_queries_author() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_cached_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/commits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "author": { "login": "bob" },
            "commit": { "author": { "email": "bob@example.com" } }
        }])))
        .mount(&mock)
        .await;
    let gh = client(&mock);
    assert_eq!(
        gh.login_for_email(1, "acme", "box", "bob@example.com")
            .await
            .as_deref(),
        Some("bob")
    );
    let asked = mock
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .any(|r| {
            r.method.as_str() == "GET"
                && r.url.path() == "/repos/acme/box/commits"
                && r.url
                    .query_pairs()
                    .any(|(k, v)| k == "author" && v == "bob@example.com")
        });
    assert!(asked, "commits list must filter by author email");
}

#[tokio::test]
async fn login_for_commit_uses_list_api_not_files_payload() {
    let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_cached_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/acme/box/commits/{sha}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sha": sha,
            "author": { "login": "from-files-endpoint" },
            "files": [{ "filename": "huge.rs", "patch": "x".repeat(1024) }]
        })))
        .expect(0)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/commits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "sha": sha,
            "author": { "login": "bob" }
        }])))
        .mount(&mock)
        .await;
    let gh = client(&mock);
    assert_eq!(
        gh.login_for_commit(1, "acme", "box", sha).await.as_deref(),
        Some("bob")
    );
    let asked = mock
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .any(|r| {
            r.method.as_str() == "GET"
                && r.url.path() == "/repos/acme/box/commits"
                && r.url.query_pairs().any(|(k, v)| k == "sha" && v == sha)
                && r.url
                    .query_pairs()
                    .any(|(k, v)| k == "per_page" && v == "1")
        });
    assert!(
        asked,
        "login_for_commit must use list-commits ?sha=&per_page=1"
    );
}

#[tokio::test]
async fn login_for_commit_drops_unsafe_login() {
    let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_cached_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/commits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "sha": sha,
            "author": { "login": "acme/other" }
        }])))
        .mount(&mock)
        .await;
    let gh = client(&mock);
    assert!(gh.login_for_commit(1, "acme", "box", sha).await.is_none());
}

#[tokio::test]
async fn github_http_does_not_follow_redirects() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_cached_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", format!("{}/stolen", mock.uri())),
        )
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/stolen"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "size": 1 })))
        .expect(0)
        .mount(&mock)
        .await;
    let gh = client(&mock);
    assert!(gh.get_repo(1, "acme", "box").await.is_err());
}
