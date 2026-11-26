//! Serve 准入与路由：loopback 绑定、Host/Origin 检查、bearer 认证与 golden 路由。

use std::net::SocketAddr;

use trit_host::{Route, ServeConfig, ServeHeaders, authorize, route};

fn config() -> ServeConfig {
    ServeConfig {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        auth_token: None,
        allowed_origins: Vec::new(),
    }
}

fn headers(host: &str) -> ServeHeaders {
    ServeHeaders {
        host: Some(host.into()),
        origin: None,
        authorization: None,
    }
}

#[test]
fn serve_config_rejects_non_loopback_and_blank_token() {
    let config = config();
    assert!(config.validate().is_ok());

    let mut non_loopback = config.clone();
    non_loopback.bind = "0.0.0.0:8080".parse().unwrap();
    assert_eq!(non_loopback.validate().unwrap_err().status, 403);

    let mut blank = config;
    blank.auth_token = Some("  ".into());
    assert_eq!(blank.validate().unwrap_err().status, 403);
}

#[test]
fn authorize_rejects_foreign_host_and_cross_site_origin() {
    let config = config();
    assert!(authorize(&config, &headers("127.0.0.1:8080")).is_ok());
    assert!(authorize(&config, &headers("localhost:8080")).is_ok());

    let error = authorize(&config, &headers("evil.example.com")).unwrap_err();
    assert_eq!(error.status, 403);

    let mut cross = headers("127.0.0.1:8080");
    cross.origin = Some("https://evil.example.com".into());
    assert_eq!(authorize(&config, &cross).unwrap_err().status, 403);

    let mut same_origin = headers("127.0.0.1:8080");
    same_origin.origin = Some("http://127.0.0.1:8080".into());
    assert!(authorize(&config, &same_origin).is_ok());

    // 白名单里的 origin 允许跨源。
    let mut allowed = config.clone();
    allowed.allowed_origins.push("http://localhost:5173".into());
    let mut from_ui = headers("127.0.0.1:8080");
    from_ui.origin = Some("http://localhost:5173".into());
    assert!(authorize(&allowed, &from_ui).is_ok());
}

#[test]
fn authorize_requires_bearer_token_when_configured() {
    let mut config = config();
    config.auth_token = Some("secret".into());

    assert_eq!(
        authorize(&config, &headers("127.0.0.1:8080"))
            .unwrap_err()
            .status,
        401
    );

    let mut wrong = headers("127.0.0.1:8080");
    wrong.authorization = Some("Bearer nope".into());
    assert_eq!(authorize(&config, &wrong).unwrap_err().status, 401);

    let mut ok = headers("127.0.0.1:8080");
    ok.authorization = Some("Bearer secret".into());
    assert!(authorize(&config, &ok).is_ok());
}

#[test]
fn route_matches_golden_table() {
    assert_eq!(route("GET", "/health").unwrap(), Route::Health);
    assert_eq!(
        route("GET", "/v1/session/status").unwrap(),
        Route::SessionStatus
    );
    assert_eq!(route("POST", "/v1/turn").unwrap(), Route::Turn);
    assert_eq!(route("GET", "/v1/events").unwrap(), Route::Events);

    // 查询串被忽略。
    assert_eq!(route("GET", "/health?probe=1").unwrap(), Route::Health);

    // 已知路径配错方法 → 405。
    assert_eq!(route("GET", "/v1/turn").unwrap_err().status, 405);
    assert_eq!(route("POST", "/health").unwrap_err().status, 405);

    // 未知路径 → 404。
    assert_eq!(route("GET", "/nope").unwrap_err().status, 404);
}
