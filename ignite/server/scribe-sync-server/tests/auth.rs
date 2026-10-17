//! S2B server authentication over real HTTP: loopback-without-auth development mode, refusal
//! of an unauthenticated LAN bind, and bearer-token enforcement on every `/v1` route
//! (including blobs).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
use sync_protocol::sha256_hex;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "scribe-s2b-auth-{label}-{n}-{}",
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn call(
    method: &str,
    url: &str,
    auth: Option<&str>,
    body: Option<&[u8]>,
) -> (u16, Option<String>, String) {
    let mut req = ureq::request(method, url);
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    let res = match body {
        Some(b) => req.send_bytes(b),
        None => req.call(),
    };
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport: {e}"),
    };
    let status = resp.status();
    let www = resp.header("WWW-Authenticate").map(str::to_string);
    (status, www, resp.into_string().unwrap_or_default())
}

fn authed(label: &str, bind: &str) -> (ServerHandle, String) {
    let token = generate_token().unwrap();
    let h = ServerHandle::start_with(
        bind.parse().unwrap(),
        Store::open(&tmp(label)).unwrap(),
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    (h, token)
}

/// `base_url` of a server bound to 0.0.0.0 reached over loopback.
fn loopback_url(h: &ServerHandle) -> String {
    format!("http://127.0.0.1:{}", h.addr().port())
}

const LIB: &str = "019a8ef2-0000-7000-8000-000000000001";

#[test]
fn loopback_without_auth_still_serves_for_local_development() {
    let h = ServerHandle::start(
        "127.0.0.1:0".parse().unwrap(),
        Store::open(&tmp("noauth")).unwrap(),
    )
    .unwrap();
    let (s, www, _) = call(
        "GET",
        &format!("{}/v1/libraries/{LIB}", h.base_url()),
        None,
        None,
    );
    assert_eq!(s, 404, "reaches routing, no auth demanded");
    assert!(www.is_none());
}

#[test]
fn non_loopback_bind_without_auth_refuses_to_start() {
    for bind in ["0.0.0.0:0", "[::]:0"] {
        let err = ServerHandle::start(bind.parse().unwrap(), Store::open(&tmp("lan")).unwrap())
            .err()
            .expect("refused");
        assert!(err.starts_with("REFUSE"), "{err}");
        assert!(err.contains("requires authentication"), "{err}");
    }
}

#[test]
fn non_loopback_bind_with_auth_starts_and_requires_the_token() {
    let (h, token) = authed("lan-auth", "0.0.0.0:0");
    let base = loopback_url(&h);
    let url = format!("{base}/v1/libraries/{LIB}");
    let (s, www, body) = call("GET", &url, None, None);
    assert_eq!(s, 401);
    assert_eq!(www.as_deref(), Some("Bearer"));
    assert!(body.contains("unauthorized"));
    let (s, _, _) = call("GET", &url, Some(&format!("Bearer {token}")), None);
    assert_eq!(s, 404, "authenticated request reaches routing");
}

#[test]
fn missing_malformed_or_wrong_tokens_get_401_without_echo() {
    let (h, token) = authed("wrong", "127.0.0.1:0");
    let url = format!("{}/v1/libraries/{LIB}", h.base_url());
    let wrong = generate_token().unwrap();
    for header in [
        None,
        Some(String::new()),
        Some(token.clone()),             // no scheme
        Some(format!("Basic {token}")),  // wrong scheme
        Some(format!("bearer {token}")), // scheme is case-sensitive here
        Some(format!("Bearer {}", &token[..63])),
        Some(format!("Bearer {wrong}")),
    ] {
        let (s, www, body) = call("GET", &url, header.as_deref(), None);
        assert_eq!(s, 401, "{header:?}");
        assert_eq!(www.as_deref(), Some("Bearer"));
        assert!(
            !body.contains(&token) && !body.contains(&wrong),
            "credential echoed"
        );
    }
    let (s, _, _) = call("GET", &url, Some(&format!("Bearer {token}")), None);
    assert_eq!(s, 404);
}

#[test]
fn every_v1_route_including_blobs_is_protected() {
    let (h, token) = authed("routes", "127.0.0.1:0");
    let b = h.base_url();
    let blob = b"background bytes".to_vec();
    let sha = sha256_hex(&blob);
    let routes: Vec<(&str, String, Option<&[u8]>)> = vec![
        ("POST", format!("{b}/v1/libraries"), Some(b"{}")),
        ("GET", format!("{b}/v1/libraries/{LIB}"), None),
        ("GET", format!("{b}/v1/libraries/{LIB}/genesis"), None),
        (
            "POST",
            format!("{b}/v1/libraries/{LIB}/operations"),
            Some(b"{\"envelopes\":[]}"),
        ),
        (
            "GET",
            format!("{b}/v1/libraries/{LIB}/operations?after=0&limit=10"),
            None,
        ),
        ("GET", format!("{b}/v1/libraries/{LIB}/snapshot"), None),
        ("PUT", format!("{b}/v1/blobs/{sha}"), Some(&blob)),
        ("GET", format!("{b}/v1/blobs/{sha}"), None),
        ("GET", format!("{b}/v1/nonexistent"), None),
    ];
    for (m, u, body) in &routes {
        let (s, _, _) = call(m, u, None, *body);
        assert_eq!(s, 401, "{m} {u}");
    }
    // Blob round trip with the token.
    let auth = format!("Bearer {token}");
    let (s, _, _) = call(
        "PUT",
        &format!("{b}/v1/blobs/{sha}"),
        Some(&auth),
        Some(&blob),
    );
    assert!(s == 200 || s == 201, "{s}");
    let (s, _, got) = call("GET", &format!("{b}/v1/blobs/{sha}"), Some(&auth), None);
    assert_eq!(s, 200);
    assert_eq!(got.as_bytes(), blob);
}

#[test]
fn verifier_file_round_trips_and_never_holds_the_token() {
    let dir = tmp("verifier");
    std::fs::create_dir_all(&dir).unwrap();
    let token = generate_token().unwrap();
    let v = TokenVerifier::for_token(&token);
    let path = dir.join("verifier");
    std::fs::write(&path, format!("{}\n", v.to_hex())).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&token));
    assert_eq!(TokenVerifier::load(&path).unwrap(), v);
    assert!(TokenVerifier::from_hex("zz").is_err());
}
