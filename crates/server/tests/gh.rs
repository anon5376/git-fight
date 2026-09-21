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
