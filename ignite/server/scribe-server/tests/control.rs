//! D1 control-plane HTTP tests against a real `scribe-server` (loopback, ephemeral port, no
//! advertisement). Clock is injectable (pairing expiry); events are captured (redaction).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use scribe_server::config::{Config, DiscoverySection, ServerSection};
use scribe_server::control::ControlStore;
use scribe_server::{serve, Running};
use serde_json::{json, Value};

struct Env {
    dir: PathBuf,
    clock: Arc<AtomicU64>,
    events: Arc<Mutex<Vec<String>>>,
}

impl Env {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "scribe-server-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self {
            dir,
            clock: Arc::new(AtomicU64::new(1_790_000_000_000)),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn config(&self) -> Config {
        Config {
            server: ServerSection {
                listen: "127.0.0.1:0".parse().unwrap(),
                state_dir: self.dir.clone(),
                web_root: None,
                name: "Scribe Server on Testhost".into(),
            },
            discovery: DiscoverySection { advertise: false },
        }
    }

    fn start(&self) -> Srv {
        let c = self.clock.clone();
        let e = self.events.clone();
        let running = serve(
            &self.config(),
            Arc::new(move || c.load(Ordering::SeqCst)),
            Arc::new(move |line: &str| e.lock().unwrap().push(line.to_string())),
        )
        .unwrap();
        Srv {
            base: running.handle.base_url(),
            running: Some(running),
        }
    }

    /// A fresh one-time setup code, as `scribe-server init` would print it.
    fn setup_code(&self) -> String {
        let c = self.clock.clone();
        let (_, _, store) =
            scribe_server::prepare(&self.config(), Arc::new(move || c.load(Ordering::SeqCst)))
                .unwrap();
        store.issue_setup_code().unwrap().expect("unclaimed")
    }

    fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn log(&self) -> String {
        self.events.lock().unwrap().join("\n")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Srv {
    base: String,
    running: Option<Running>,
}

impl Srv {
    fn stop(mut self) {
        if let Some(r) = self.running.take() {
            r.handle.shutdown();
        }
    }
}

#[derive(Default, Clone)]
struct H {
    cookie: Option<String>,
    bearer: Option<String>,
    csrf: bool,
    origin: Option<String>,
}

struct Resp {
    status: u16,
    body: Value,
    set_cookie: Option<String>,
}

fn call(base: &str, method: &str, path: &str, h: &H, body: Option<Value>) -> Resp {
    let mut req = ureq::request(method, &format!("{base}{path}"));
    if let Some(c) = &h.cookie {
        req = req.set("Cookie", &format!("scribe_session={c}"));
    }
    if let Some(b) = &h.bearer {
        req = req.set("Authorization", &format!("Bearer {b}"));
    }
    if h.csrf {
        req = req.set("X-Scribe-Request", "1");
    }
    if let Some(o) = &h.origin {
        req = req.set("Origin", o);
    }
    let r = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    let resp = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport: {e}"),
    };
    let status = resp.status();
    let set_cookie = resp.header("set-cookie").map(str::to_string);
    let text = resp.into_string().unwrap_or_default();
    Resp {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::Null),
        set_cookie,
    }
}

fn cookie_value(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("scribe_session=")
        .to_string()
}

fn csrf() -> H {
    H {
        csrf: true,
        ..H::default()
    }
}

/// Claim and return the admin cookie value.
fn claim(env: &Env, srv: &Srv) -> String {
    let code = env.setup_code();
    let r = call(
        &srv.base,
        "POST",
        "/control/v1/claim",
        &csrf(),
        Some(json!({"setup_code": code})),
    );
    assert_eq!(r.status, 200, "{:?}", r.body);
    cookie_value(&r.set_cookie.unwrap())
}

fn admin(cookie: &str) -> H {
    H {
        cookie: Some(cookie.into()),
        csrf: true,
        ..H::default()
    }
}

/// Pair a Kindle end-to-end; returns its credential.
fn pair_kindle(srv: &Srv, admin_cookie: &str, name: &str) -> (String, String) {
    let c = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(
            json!({"client_type": "kindle", "display_name": name, "client_id": "kindle-serial-x"}),
        ),
    );
    assert_eq!(c.status, 201, "{:?}", c.body);
    let id = c.body["request_id"].as_str().unwrap().to_string();
    let secret = c.body["request_secret"].as_str().unwrap().to_string();
    let a = call(
        &srv.base,
        "POST",
        &format!("/control/v1/admin/pairing/{id}/approve"),
        &admin(admin_cookie),
        None,
    );
    assert_eq!(a.status, 200, "{:?}", a.body);
    let p = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret": secret})),
    );
    assert_eq!(p.status, 200);
    assert_eq!(p.body["state"], "approved");
    (
        p.body["credential"].as_str().unwrap().to_string(),
        p.body["device_id"].as_str().unwrap().to_string(),
    )
}

fn libraries(srv: &Srv, h: &H) -> u16 {
    call(&srv.base, "GET", "/v1/libraries", h, None).status
}

#[test]
fn fresh_server_is_unclaimed_and_claim_is_one_time_and_rate_limited() {
    let env = Env::new("claim");
    let srv = env.start();
    let s = call(&srv.base, "GET", "/control/v1/status", &H::default(), None);
    assert_eq!(s.status, 200);
    assert_eq!(s.body["claimed"], false);
    assert_eq!(s.body["mode"], "product");
    assert!(s.body["session"].is_null());
    // Nothing is usable anonymously: /v1 and pairing are refused while unclaimed.
    assert_eq!(libraries(&srv, &H::default()), 401);
    let p = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({"client_type":"kindle","display_name":"K"})),
    );
    assert_eq!(
        (p.status, p.body["error"].as_str()),
        (409, Some("unclaimed"))
    );

    let code = env.setup_code();
    // CSRF: the marker header is required, and a foreign Origin is refused.
    let r = call(
        &srv.base,
        "POST",
        "/control/v1/claim",
        &H::default(),
        Some(json!({"setup_code": code})),
    );
    assert_eq!(r.status, 403);
    let evil = H {
        csrf: true,
        origin: Some("http://evil.example".into()),
        ..H::default()
    };
    assert_eq!(
        call(
            &srv.base,
            "POST",
            "/control/v1/claim",
            &evil,
            Some(json!({"setup_code": code}))
        )
        .status,
        403
    );
    // Wrong codes are refused and rate limited (5/min/peer).
    for _ in 0..4 {
        let r = call(
            &srv.base,
            "POST",
            "/control/v1/claim",
            &csrf(),
            Some(json!({"setup_code": "0000-0000"})),
        );
        assert_eq!(r.status, 403);
    }
    // 5th attempt (the correct code, but still within the window) is allowed; typed loosely.
    let loose = code.to_lowercase().replace('-', " ");
    let ok = call(
        &srv.base,
        "POST",
        "/control/v1/claim",
        &csrf(),
        Some(json!({"setup_code": loose, "display_name": "Browser on Test Host A"})),
    );
    assert_eq!(ok.status, 200, "{:?}", ok.body);
    assert_eq!(ok.body["role"], "admin");
    let sc = ok.set_cookie.unwrap();
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age="] {
        assert!(sc.contains(attr), "{attr} missing in {sc}");
    }
    assert!(
        !sc.contains("Secure"),
        "plain-HTTP LAN phase: no Secure attribute (documented)"
    );
    let admin_cookie = cookie_value(&sc);
    assert!(
        !ok.body.to_string().contains(&admin_cookie),
        "credential must not be in the JSON body"
    );
    // Rate limit now exhausted for this peer.
    let r = call(
        &srv.base,
        "POST",
        "/control/v1/claim",
        &csrf(),
        Some(json!({"setup_code": code})),
    );
    assert_eq!(r.status, 429);
    env.advance(61_000);
    // The code is consumed: reuse fails; the claim window never reopens.
    let r = call(
        &srv.base,
        "POST",
        "/control/v1/claim",
        &csrf(),
        Some(json!({"setup_code": code})),
    );
    assert_eq!(r.status, 409);
    let store = ControlStore::open(&env.dir, Arc::new(|| 0)).unwrap();
    assert_eq!(store.issue_setup_code().unwrap(), None);
    assert!(!store.has_setup_code().unwrap());
    // The admin session works; status reflects it.
    let s = call(
        &srv.base,
        "GET",
        "/control/v1/status",
        &admin(&admin_cookie),
        None,
    );
    assert_eq!(s.body["claimed"], true);
    assert_eq!(s.body["session"]["role"], "admin");
    assert_eq!(libraries(&srv, &admin(&admin_cookie)), 200);
    srv.stop();
}

#[test]
fn admin_api_requires_an_admin_session() {
    let env = Env::new("admin");
    let srv = env.start();
    let admin_cookie = claim(&env, &srv);
    assert_eq!(
        call(
            &srv.base,
            "GET",
            "/control/v1/admin/devices",
            &H::default(),
            None
        )
        .status,
        401
    );
    assert_eq!(
        call(
            &srv.base,
            "GET",
            "/control/v1/admin/devices",
            &H {
                cookie: Some("junk".into()),
                ..H::default()
            },
            None
        )
        .status,
        401
    );
    let d = call(
        &srv.base,
        "GET",
        "/control/v1/admin/devices",
        &admin(&admin_cookie),
        None,
    );
    assert_eq!(d.status, 200);
    assert_eq!(d.body["devices"].as_array().unwrap().len(), 1);
    assert_eq!(d.body["devices"][0]["current"], true);
    // Mutating admin calls need the CSRF marker even with a valid cookie.
    let no_csrf = H {
        cookie: Some(admin_cookie.clone()),
        ..H::default()
    };
    let id = d.body["devices"][0]["device_id"].as_str().unwrap();
    assert_eq!(
        call(
            &srv.base,
            "POST",
            &format!("/control/v1/admin/devices/{id}/revoke"),
            &no_csrf,
            None
        )
        .status,
        403
    );
    // Nobody can lock themselves out.
    assert_eq!(
        call(
            &srv.base,
            "POST",
            &format!("/control/v1/admin/devices/{id}/revoke"),
            &admin(&admin_cookie),
            None
        )
        .status,
        409
    );
    // A Kindle (member) credential cannot use the admin API.
    let (kcred, _) = pair_kindle(&srv, &admin_cookie, "Kindle Scribe");
    let k = H {
        bearer: Some(kcred),
        csrf: true,
        ..H::default()
    };
    assert_eq!(
        call(&srv.base, "GET", "/control/v1/admin/devices", &k, None).status,
        403
    );
    let st = call(
        &srv.base,
        "GET",
        "/control/v1/admin/server",
        &admin(&admin_cookie),
        None,
    );
    assert_eq!(st.status, 200);
    assert_eq!(st.body["library_count"], 0);
    assert_eq!(st.body["device_count"], 2);
    assert!(st.body["server_id"].as_str().unwrap().len() == 36);
    srv.stop();
}

#[test]
fn kindle_pairing_approve_deny_expiry_and_one_time_delivery() {
    let env = Env::new("pair");
    let srv = env.start();
    let admin_cookie = claim(&env, &srv);
    // Create → admin sees it with the same comparison code.
    let c = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({"client_type":"kindle","display_name":"Kindle Scribe"})),
    );
    assert_eq!(c.status, 201);
    let code = c.body["comparison_code"].as_str().unwrap().to_string();
    assert!(code.len() == 7 && code.as_bytes()[3] == b' ');
    assert_eq!(c.body["expires_in_s"], 300);
    let id = c.body["request_id"].as_str().unwrap().to_string();
    let secret = c.body["request_secret"].as_str().unwrap().to_string();
    let pend = call(
        &srv.base,
        "GET",
        "/control/v1/admin/pairing",
        &admin(&admin_cookie),
        None,
    );
    assert_eq!(pend.body["requests"][0]["comparison_code"], code.as_str());
    assert!(
        !pend.body.to_string().contains(&secret),
        "request secret never shown to the admin"
    );
    // Only the requester (secret holder) can poll.
    let wrong = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret":"AAAA"})),
    );
    assert_eq!(wrong.status, 404);
    let p = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret": secret})),
    );
    assert_eq!(p.body["state"], "pending");
    assert!(p.body.get("credential").is_none());
    // Approve → delivered once → second poll has no credential.
    assert_eq!(
        call(
            &srv.base,
            "POST",
            &format!("/control/v1/admin/pairing/{id}/approve"),
            &admin(&admin_cookie),
            None
        )
        .status,
        200
    );
    let p1 = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret": secret})),
    );
    let cred = p1.body["credential"].as_str().unwrap().to_string();
    assert!(
        p1.set_cookie.is_none(),
        "Kindle credentials are not cookies"
    );
    let p2 = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret": secret})),
    );
    assert_eq!(p2.body["state"], "delivered");
    assert!(p2.body.get("credential").is_none());
    // The credential authenticates /v1 (Bearer), 256-bit secret.
    let (did, sec) = cred.split_once('.').unwrap();
    assert_eq!(sec.len(), 43); // 32 bytes base64url
    assert_eq!(
        libraries(
            &srv,
            &H {
                bearer: Some(cred.clone()),
                ..H::default()
            }
        ),
        200
    );
    assert_eq!(
        libraries(
            &srv,
            &H {
                bearer: Some(format!("{did}.{}", "A".repeat(43))),
                ..H::default()
            }
        ),
        401
    );
    // Deny path.
    let d = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({"client_type":"kindle","display_name":"Other"})),
    );
    let did2 = d.body["request_id"].as_str().unwrap().to_string();
    assert_eq!(
        call(
            &srv.base,
            "POST",
            &format!("/control/v1/admin/pairing/{did2}/deny"),
            &admin(&admin_cookie),
            None
        )
        .status,
        200
    );
    let pd = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{did2}/poll"),
        &csrf(),
        Some(json!({"request_secret": d.body["request_secret"]})),
    );
    assert_eq!(pd.body["state"], "denied");
    assert!(pd.body.get("credential").is_none());
    // Expiry: an expired request can neither be approved nor polled into a credential.
    let e = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({"client_type":"kindle","display_name":"Late"})),
    );
    let eid = e.body["request_id"].as_str().unwrap().to_string();
    env.advance(301_000);
    let ap = call(
        &srv.base,
        "POST",
        &format!("/control/v1/admin/pairing/{eid}/approve"),
        &admin(&admin_cookie),
        None,
    );
    assert_eq!(
        (ap.status, ap.body["error"].as_str()),
        (409, Some("expired"))
    );
    let pe = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{eid}/poll"),
        &csrf(),
        Some(json!({"request_secret": e.body["request_secret"]})),
    );
    assert_eq!(
        (pe.status, pe.body["state"].as_str()),
        (410, Some("expired"))
    );
    assert!(call(
        &srv.base,
        "GET",
        "/control/v1/admin/pairing",
        &admin(&admin_cookie),
        None
    )
    .body["requests"]
        .as_array()
        .unwrap()
        .is_empty());
    srv.stop();
}

#[test]
fn pairing_creation_is_rate_limited_and_bounded() {
    let env = Env::new("prl");
    let srv = env.start();
    let _admin = claim(&env, &srv);
    let mut statuses = Vec::new();
    for i in 0..8 {
        statuses.push(
            call(
                &srv.base,
                "POST",
                "/control/v1/pairing",
                &csrf(),
                Some(json!({"client_type":"browser","display_name": format!("B{i}")})),
            )
            .status,
        );
    }
    assert_eq!(&statuses[..6], &[201; 6]);
    assert_eq!(statuses[6], 429);
    srv.stop();
}

#[test]
fn per_device_credentials_are_independent_and_revocable() {
    let env = Env::new("revoke");
    let srv = env.start();
    let admin_cookie = claim(&env, &srv);
    let (a, a_id) = pair_kindle(&srv, &admin_cookie, "Kindle A");
    let (b, b_id) = pair_kindle(&srv, &admin_cookie, "Kindle B");
    assert_ne!(a_id, b_id);
    let ha = H {
        bearer: Some(a.clone()),
        ..H::default()
    };
    let hb = H {
        bearer: Some(b.clone()),
        ..H::default()
    };
    assert_eq!(libraries(&srv, &ha), 200);
    assert_eq!(libraries(&srv, &hb), 200);
    // A's secret under B's device id (impersonation) fails.
    let a_secret = a.split_once('.').unwrap().1;
    assert_eq!(
        libraries(
            &srv,
            &H {
                bearer: Some(format!("{b_id}.{a_secret}")),
                ..H::default()
            }
        ),
        401
    );
    // Revoke A: A → 401, B and the admin browser unaffected.
    assert_eq!(
        call(
            &srv.base,
            "POST",
            &format!("/control/v1/admin/devices/{a_id}/revoke"),
            &admin(&admin_cookie),
            None
        )
        .status,
        200
    );
    assert_eq!(libraries(&srv, &ha), 401);
    assert_eq!(libraries(&srv, &hb), 200);
    assert_eq!(libraries(&srv, &admin(&admin_cookie)), 200);
    let devs = call(
        &srv.base,
        "GET",
        "/control/v1/admin/devices",
        &admin(&admin_cookie),
        None,
    )
    .body;
    let revoked: Vec<bool> = devs["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["revoked"].as_bool().unwrap())
        .collect();
    assert_eq!(revoked, [false, true, false]);
    srv.stop();
}

#[test]
fn browser_pairing_gets_a_cookie_and_cookie_mutations_need_csrf() {
    let env = Env::new("browser");
    let srv = env.start();
    let admin_cookie = claim(&env, &srv);
    let c = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({"client_type":"browser","display_name":"Browser on Test Host B"})),
    );
    let id = c.body["request_id"].as_str().unwrap().to_string();
    call(
        &srv.base,
        "POST",
        &format!("/control/v1/admin/pairing/{id}/approve"),
        &admin(&admin_cookie),
        None,
    );
    let p = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret": c.body["request_secret"]})),
    );
    assert_eq!(p.body["state"], "approved");
    assert!(
        p.body.get("credential").is_none(),
        "browsers never receive the credential in JSON"
    );
    let sc = p.set_cookie.expect("session cookie");
    assert!(sc.contains("HttpOnly") && sc.contains("SameSite=Strict"));
    let bob = cookie_value(&sc);
    let s = call(
        &srv.base,
        "GET",
        "/control/v1/status",
        &H {
            cookie: Some(bob.clone()),
            ..H::default()
        },
        None,
    );
    assert_eq!(s.body["session"]["role"], "member");
    assert_eq!(s.body["session"]["display_name"], "Browser on Test Host B");
    // Cookie GETs work; cookie POSTs to /v1 need the CSRF marker and a same-host Origin.
    assert_eq!(
        libraries(
            &srv,
            &H {
                cookie: Some(bob.clone()),
                ..H::default()
            }
        ),
        200
    );
    let post = |h: &H| {
        call(
            &srv.base,
            "POST",
            "/v1/libraries/01a0de1f-f9d6-7036-a2b7-478dc58007ff/operations",
            h,
            Some(json!({"envelopes": []})),
        )
        .status
    };
    assert_eq!(
        post(&H {
            cookie: Some(bob.clone()),
            ..H::default()
        }),
        403
    );
    let host = srv.base.trim_start_matches("http://").to_string();
    assert_eq!(
        post(&H {
            cookie: Some(bob.clone()),
            csrf: true,
            origin: Some("http://evil.example".into()),
            ..H::default()
        }),
        403
    );
    // With the marker + matching Origin the request reaches the (unchanged) sync API.
    assert_eq!(
        post(&H {
            cookie: Some(bob.clone()),
            csrf: true,
            origin: Some(format!("http://{host}")),
            ..H::default()
        }),
        400
    );
    // A member browser cannot reach admin routes.
    assert_eq!(
        call(
            &srv.base,
            "GET",
            "/control/v1/admin/devices",
            &H {
                cookie: Some(bob),
                ..H::default()
            },
            None
        )
        .status,
        403
    );
    srv.stop();
}

#[test]
fn pairing_survives_restart_and_server_id_is_stable() {
    let env = Env::new("restart");
    let srv = env.start();
    let admin_cookie = claim(&env, &srv);
    let (cred, _) = pair_kindle(&srv, &admin_cookie, "Kindle Scribe");
    let sid1 =
        call(&srv.base, "GET", "/control/v1/status", &H::default(), None).body["server_id"].clone();
    srv.stop();
    let srv = env.start();
    let s = call(
        &srv.base,
        "GET",
        "/control/v1/status",
        &admin(&admin_cookie),
        None,
    );
    assert_eq!(s.body["server_id"], sid1);
    assert_eq!(s.body["claimed"], true);
    assert_eq!(s.body["session"]["role"], "admin");
    assert_eq!(
        libraries(
            &srv,
            &H {
                bearer: Some(cred),
                ..H::default()
            }
        ),
        200
    );
    srv.stop();
    // Re-running init on a claimed server issues no code and keeps the identity.
    let (id, created, store) = scribe_server::prepare(&env.config(), Arc::new(|| 0)).unwrap();
    assert!(!created);
    assert_eq!(serde_json::to_value(id.server_id).unwrap(), sid1);
    assert_eq!(store.issue_setup_code().unwrap(), None);
    assert!(Path::new(&env.dir).join("server.json").exists());
}

#[test]
fn no_secret_is_ever_logged_or_leaked() {
    let env = Env::new("redact");
    let srv = env.start();
    let code = env.setup_code();
    let ok = call(
        &srv.base,
        "POST",
        "/control/v1/claim",
        &csrf(),
        Some(json!({"setup_code": code})),
    );
    let admin_cookie = cookie_value(&ok.set_cookie.unwrap());
    let c = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({"client_type":"kindle","display_name":"Kindle"})),
    );
    let secret = c.body["request_secret"].as_str().unwrap().to_string();
    let id = c.body["request_id"].as_str().unwrap().to_string();
    call(
        &srv.base,
        "POST",
        &format!("/control/v1/admin/pairing/{id}/approve"),
        &admin(&admin_cookie),
        None,
    );
    let p = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret": secret})),
    );
    let cred = p.body["credential"].as_str().unwrap().to_string();
    libraries(
        &srv,
        &H {
            bearer: Some(cred.clone()),
            ..H::default()
        },
    );
    libraries(
        &srv,
        &H {
            bearer: Some("forged.credential".into()),
            ..H::default()
        },
    );
    let log = env.log();
    assert!(log.contains("event=server_claimed") && log.contains("event=device_paired"));
    for s in [
        &code,
        &admin_cookie,
        &secret,
        &cred,
        &cred.split_once('.').unwrap().1.to_string(),
    ] {
        assert!(!log.contains(s.as_str()), "secret leaked into log");
    }
    assert!(
        !log.to_lowercase().contains("authorization")
            && !log.contains("Bearer")
            && !log.contains("scribe_session")
    );
    // Public/admin JSON never carries secrets.
    let status = call(
        &srv.base,
        "GET",
        "/control/v1/status",
        &admin(&admin_cookie),
        None,
    )
    .body
    .to_string();
    let devices = call(
        &srv.base,
        "GET",
        "/control/v1/admin/devices",
        &admin(&admin_cookie),
        None,
    )
    .body
    .to_string();
    for body in [status, devices] {
        assert!(
            !body.contains(&admin_cookie) && !body.contains(&cred) && !body.contains("verifier")
        );
    }
    srv.stop();
}

#[test]
fn host_operator_can_recover_admin_access_for_a_pending_browser() {
    let env = Env::new("recover");
    let srv = env.start();
    let _lost_admin = claim(&env, &srv); // the only admin browser is then "lost"
    let c = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({"client_type":"browser","display_name":"Chrome on Windows"})),
    );
    let code = c.body["comparison_code"].as_str().unwrap().to_string();
    let store = ControlStore::open(
        &env.dir,
        Arc::new({
            let c = env.clock.clone();
            move || c.load(Ordering::SeqCst)
        }),
    )
    .unwrap();
    assert!(store.approve_admin_by_code("000 000").is_err());
    let (id, name) = store.approve_admin_by_code(&code.replace(' ', "")).unwrap();
    assert_eq!(id, c.body["request_id"].as_str().unwrap());
    assert_eq!(name, "Chrome on Windows");
    let p = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({"request_secret": c.body["request_secret"]})),
    );
    assert_eq!(p.body["state"], "approved");
    let cookie = cookie_value(&p.set_cookie.unwrap());
    let s = call(
        &srv.base,
        "GET",
        "/control/v1/status",
        &H {
            cookie: Some(cookie.clone()),
            ..H::default()
        },
        None,
    );
    assert_eq!(s.body["session"]["role"], "admin");
    assert_eq!(
        call(
            &srv.base,
            "GET",
            "/control/v1/admin/devices",
            &admin(&cookie),
            None
        )
        .status,
        200
    );
    // Kindle requests can never be escalated this way, and the claim window stays closed.
    assert_eq!(store.issue_setup_code().unwrap(), None);
    srv.stop();
}
