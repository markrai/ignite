//! Real DNS-SD over multicast on Linux (opt-in: `SCRIBE_MDNS_IT=1`; normal unit tests never
//! depend on multicast availability). Publishes exactly Scribe Server's records and browses
//! them back through a second, independent mDNS daemon.

#[cfg(target_os = "linux")]
#[test]
fn advertisement_is_discoverable_over_real_multicast() {
    use std::time::{Duration, Instant};

    use control_protocol::{parse_advertisement, ServerId, SERVICE_TYPE};
    use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

    if std::env::var("SCRIBE_MDNS_IT").as_deref() != Ok("1") {
        eprintln!("skipped (set SCRIBE_MDNS_IT=1 on a host with multicast)");
        return;
    }
    let sid = ServerId::from_parts(1_790_000_000_000, &[9; 10]);
    let (instance, host, port, txt) =
        scribe_server::discovery::service_parts(sid, "Test Server B", "test-server-b", 80).unwrap();
    let publisher = ServiceDaemon::new().unwrap();
    // Same host: loop multicast back so the browser daemon below can hear it.
    publisher.set_multicast_loop_v4(true).unwrap();
    let props: std::collections::HashMap<String, String> = txt.into_iter().collect();
    let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, props)
        .unwrap()
        .enable_addr_auto();
    publisher.register(info).unwrap();

    let browser = ServiceDaemon::new().unwrap();
    browser.set_multicast_loop_v4(true).unwrap();
    let rx = browser.browse(SERVICE_TYPE).unwrap();
    let t = Instant::now();
    let mut found = None;
    while t.elapsed() < Duration::from_secs(10) {
        if let Ok(ServiceEvent::ServiceResolved(r)) = rx.recv_timeout(Duration::from_millis(500)) {
            let txt: Vec<(String, String)> = r
                .txt_properties
                .iter()
                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                .collect();
            found = Some((
                r.port,
                parse_advertisement(&txt).unwrap(),
                r.addresses.len(),
            ));
            break;
        }
    }
    let _ = publisher.shutdown();
    let _ = browser.shutdown();
    let (port, ad, addrs) = found.expect("Scribe Server advertisement not discovered");
    assert_eq!(port, 80);
    assert_eq!(ad.server_id, sid);
    assert_eq!(ad.name, "Test Server B");
    assert!(addrs >= 1);
}
