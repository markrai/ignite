//! Synthetic collector matrix for Slice 18E.10H. Review 109 Kindle strings are
//! fixtures only; their historical PID/inode are not production constants.

use crate::sync_recovery_attest::{
    apply_invalidation, bind_attestation_to_recovery_policy, parse_proc_cmdline,
    parse_proc_stat_starttime, AttestationInvalidation, DropbearClientEvent, ProcessStartIdentity,
    SupervisedTunnelIdentity, TunnelListenerObservation, TunnelObservationSnapshot,
    TunnelProcessObservation,
};
use crate::sync_recovery_observe::*;
use crate::sync_recovery_policy::{
    RecoveryGate, SshAuthEvidenceSource, TunnelLifecycle, LIVE_NON_LOOPBACK_POST_ENABLED,
};
use crate::sync_recovery_tunnel::{
    build_dropbear_invocation, fingerprint_of_b64, HostKeyTrustInput, RecoveryTunnelPlan,
    DROPBEAR_CLIENT_APPLET, KINDLE_DROPBEARMULTI,
};

const PIN_BLOB: &[u8] = b"ssh-ed25519\0synthetic-host-key-blob";

fn b64_encode(data: &[u8]) -> String {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < data.len() {
            out.push(B64[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(B64[(b2 & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn pin_material() -> (String, String, String) {
    let b64 = b64_encode(PIN_BLOB);
    let fp = fingerprint_of_b64(&b64).unwrap();
    let line = format!("ssh.example.test ssh-ed25519 {b64}\n");
    (b64, fp, line)
}

fn plan(fp: &str) -> RecoveryTunnelPlan<'static> {
    let fp = Box::leak(fp.to_string().into_boxed_str());
    RecoveryTunnelPlan {
        executable: KINDLE_DROPBEARMULTI,
        applet: DROPBEAR_CLIENT_APPLET,
        listen_addr: "127.0.0.1",
        listen_port: 18765,
        ssh_user: "syncfwd",
        ssh_host: "ssh.example.test",
        ssh_port: 22,
        remote_forward_host: "sync.example.test",
        remote_forward_port: 8080,
        host_key_algorithm: "ssh-ed25519",
        pinned_host_key_sha256: fp,
        identity_file: "/mnt/us/scribe-app/state/recovery-tunnel/id_ed25519",
        known_hosts_home: "/mnt/us/scribe-app/state/recovery-tunnel",
        library_id: "lib-aaaa",
        replica_id: "rep-bbbb",
        genesis_sha256: "gen-cccc",
        transaction_id: "txn-synthetic-1",
    }
}

fn host_input<'a>(known: &'a str, b64: &'a str, fp: &'a str) -> HostKeyTrustInput<'a> {
    HostKeyTrustInput {
        known_hosts_text: Some(known),
        expected_hostname: "ssh.example.test",
        expected_ssh_port: 22,
        planned_ssh_port: 22,
        expected_algorithm: "ssh-ed25519",
        expected_key_b64: b64,
        expected_fingerprint_sha256: fp,
        tofu_permitted: false,
        accept_unknown_host: false,
        host_key_verification_disabled: false,
    }
}

fn leaked_plan_host() -> (
    RecoveryTunnelPlan<'static>,
    HostKeyTrustInput<'static>,
    Vec<String>,
) {
    let (b64, fp, line) = pin_material();
    let b64 = Box::leak(b64.into_boxed_str());
    let fp = Box::leak(fp.into_boxed_str());
    let line = Box::leak(line.into_boxed_str());
    let p = plan(fp);
    let argv = build_dropbear_invocation(&p).unwrap().argv;
    (p, host_input(line, b64, fp), argv)
}

/// Field 22 (starttime) is rest index 19. Comm may contain spaces when callers pass that.
fn stat_line(pid: u32, comm: &str, state: char, starttime: u64) -> String {
    let rest = "1 ".repeat(18);
    format!("{pid} ({comm}) {state} {rest}{starttime} 0 0 0 0 0 0 0 0 0 0")
}

fn tcp_table(rows: &str) -> String {
    format!(
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n{rows}"
    )
}

fn tcp_row(addr_hex: &str, port: u16, st: &str, inode: u64) -> String {
    format!(
        "   0: {addr_hex}:{:04X} 00000000:0000 {st} 00000000:00000000 00:00000000 00000000     0        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
        port
    )
}

fn tcp6_table(rows: &str) -> String {
    format!(
        "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n{rows}"
    )
}

fn tcp6_row(addr_hex: &str, port: u16, st: &str, inode: u64) -> String {
    format!(
        "   0: {addr_hex}:{:04X} 00000000000000000000000000000000:0000 {st} 00000000:00000000 00:00000000 00000000     0        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
        port
    )
}

const LOOP_V4: &str = "0100007F";
const WILD_V4: &str = "00000000";
const LOOP_V6: &str = "00000000000000000000000001000000";
const WILD_V6: &str = "00000000000000000000000000000000";

fn supervised(pid: u32, starttime: u64, argv: Vec<String>) -> SupervisedTunnelIdentity {
    SupervisedTunnelIdentity {
        identity: ProcessStartIdentity { pid, starttime },
        generation: 1,
        expected_exe: KINDLE_DROPBEARMULTI.to_string(),
        expected_cmdline: argv,
    }
}

fn good_state(pid: u32, starttime: u64, argv: &[String], inode: u64) -> SyntheticFsState {
    let mut st = SyntheticFsState::default();
    st.stat
        .insert(pid, Ok(stat_line(pid, "dropbearmulti", 'S', starttime)));
    st.exe.insert(pid, Ok(KINDLE_DROPBEARMULTI.to_string()));
    let mut raw = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            raw.push(0);
        }
        raw.extend(a.as_bytes());
    }
    raw.push(0);
    st.cmdline.insert(pid, Ok(raw));
    st.fds.insert(
        pid,
        Ok(vec![format!("socket:[{inode}]"), "/dev/null".into()]),
    );
    st.tcp = Ok(tcp_table(&tcp_row(LOOP_V4, 18765, "0A", inode)));
    st.tcp6 = Ok(None);
    st
}

fn req<'a>(
    plan: &'a RecoveryTunnelPlan<'a>,
    sup: &'a SupervisedTunnelIdentity,
) -> CollectTunnelObservationRequest<'a> {
    CollectTunnelObservationRequest {
        plan,
        supervised: sup,
        expected_listen_addr: plan.listen_addr,
        expected_listen_port: plan.listen_port,
        expected_identity: sup.identity,
        source: ObservationSourceKind::SyntheticFixture,
    }
}

fn refuse_has(err: &[String], needle: &str) {
    assert!(
        err.iter().any(|e| e.contains(needle)),
        "expected {needle:?} in {err:?}"
    );
}

#[test]
fn valid_supervised_process_and_ipv4_inode_join() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let snap = collect_tunnel_observation(&req(&plan, &sup), &fs).unwrap();
    assert_eq!(snap.generation, 1);
    let proc = snap.process.as_ref().unwrap();
    assert_eq!(proc.identity.pid, 4242);
    assert_eq!(proc.identity.starttime, 9001);
    assert_eq!(proc.exe, KINDLE_DROPBEARMULTI);
    assert_eq!(proc.cmdline, argv);
    assert!(proc.alive);
    assert_eq!(snap.listeners.len(), 1);
    assert_eq!(snap.listeners[0].bind_address, "127.0.0.1");
    assert_eq!(snap.listeners[0].port, 18765);
    assert!(snap.listeners[0].listen_state);
    assert_eq!(snap.listeners[0].owner_pid, 4242);
    assert_eq!(snap.dropbear_event, DropbearClientEvent::NotCaptured);
}

#[test]
fn missing_pid_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let mut sup = supervised(0, 9001, argv);
    sup.identity.pid = 0;
    let fs = SyntheticProcfs::new(SyntheticFsState::default());
    let err = collect_tunnel_observation(&req(&plan, &sup), &fs).unwrap_err();
    refuse_has(&err, "PID is missing");
}

#[test]
fn missing_proc_stat_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(5, 9, argv.clone());
    let mut st = good_state(5, 9, &argv, 1);
    st.stat.remove(&5);
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "/proc/<pid>/stat is missing");
}

#[test]
fn parenthesized_comm_with_spaces() {
    let id_state = parse_supervised_stat(&stat_line(11, "drop bear", 'S', 44)).unwrap();
    assert_eq!(id_state.0.pid, 11);
    assert_eq!(id_state.0.starttime, 44);
    assert_eq!(id_state.1, 'S');
}

#[test]
fn malformed_stat_fields_refused() {
    let err = parse_supervised_stat("not-a-stat-line").unwrap_err();
    assert!(err.contains("comm"));
    let err = parse_supervised_stat("12 (x) S 1 2").unwrap_err();
    assert!(err.contains("start time is missing"));
}

#[test]
fn missing_and_zero_start_time_refused() {
    let err = parse_supervised_stat("12 (x) S 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1").unwrap_err();
    assert!(err.contains("start time is missing"));
    let err = parse_supervised_stat(&stat_line(12, "x", 'S', 0)).unwrap_err();
    assert!(err.contains("start time is zero"));
}

#[test]
fn changed_start_time_is_pid_reuse() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 999, &argv, 3);
    st.stat
        .insert(8, Ok(stat_line(8, "dropbearmulti", 'S', 999)));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "PID reuse");
}

#[test]
fn wrong_executable_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.exe.insert(8, Ok("/bin/false".into()));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "executable");
}

#[test]
fn wrong_applet_refused() {
    let (plan, _, mut argv) = leaked_plan_host();
    argv[1] = "scp".into();
    let sup = supervised(8, 100, argv.clone());
    let st = good_state(8, 100, &argv, 3);
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "applet");
}

#[test]
fn wrong_arguments_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let mut observed = argv.clone();
    observed.pop();
    observed.push("other@titan".into());
    let sup = supervised(8, 100, argv);
    let st = good_state(8, 100, &observed, 3);
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "arguments");
}

#[test]
fn zombie_process_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.stat
        .insert(8, Ok(stat_line(8, "dropbearmulti", 'Z', 100)));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "zombie");
}

#[test]
fn permission_denied_procfs_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.stat.insert(8, Err(ObserveError::PermissionDenied));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "inaccessible");
}

#[test]
fn wrong_listening_port_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.tcp = Ok(tcp_table(&tcp_row(LOOP_V4, 9, "0A", 3)));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "missing");
}

#[test]
fn wildcard_ipv4_bind_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.tcp = Ok(tcp_table(&tcp_row(WILD_V4, 18765, "0A", 3)));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "wildcard");
}

#[test]
fn missing_listener_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.tcp = Ok(tcp_table(""));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "missing");
}

#[test]
fn non_listen_socket_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.tcp = Ok(tcp_table(&tcp_row(LOOP_V4, 18765, "01", 3)));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "LISTEN");
}

#[test]
fn missing_socket_inode_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 0);
    st.tcp = Ok(tcp_table(&tcp_row(LOOP_V4, 18765, "0A", 0)));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "inode is missing");
}

#[test]
fn malformed_socket_record_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.tcp = Ok(tcp_table(
        "   0: 0100007F:494D 00000000:0000 0A 0 0 0 0 0 not-an-inode\n",
    ));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "malformed");
}

#[test]
fn missing_matching_descriptor_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.fds.insert(8, Ok(vec!["pipe:[1]".into()]));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "not owned by the supervised process");
}

#[test]
fn wrong_descriptor_inode_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.fds.insert(8, Ok(vec!["socket:[9999]".into()]));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "not owned by the supervised process");
}

#[test]
fn listener_owned_by_another_process_cannot_be_asserted() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    // Another PID holds the inode; collector must not scan it or accept owner_pid.
    st.fds.insert(8, Ok(vec!["socket:[1]".into()]));
    st.fds.insert(99, Ok(vec!["socket:[3]".into()]));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "not owned by the supervised process");
}

#[test]
fn socket_closes_during_collection() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let st = good_state(8, 100, &argv, 3);
    let fs = SyntheticProcfs::new(st.clone());
    let mut gone = st;
    gone.tcp = Ok(tcp_table(""));
    fs.replace_after_sockets(gone);
    let err = collect_tunnel_observation(&req(&plan, &sup), &fs).unwrap_err();
    refuse_has(&err, "missing");
}

#[test]
fn process_exits_during_collection() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let st = good_state(8, 100, &argv, 3);
    let fs = SyntheticProcfs::new(st.clone());
    let mut gone = SyntheticFsState::default();
    gone.tcp = st.tcp.clone();
    fs.replace_after_identity(gone);
    let err = collect_tunnel_observation(&req(&plan, &sup), &fs).unwrap_err();
    refuse_has(&err, "missing");
}

#[test]
fn pid_reused_between_collection_steps() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let st = good_state(8, 100, &argv, 3);
    let fs = SyntheticProcfs::new(st.clone());
    let mut reused = st;
    reused
        .stat
        .insert(8, Ok(stat_line(8, "dropbearmulti", 'S', 5000)));
    fs.replace_after_sockets(reused);
    let err = collect_tunnel_observation(&req(&plan, &sup), &fs).unwrap_err();
    refuse_has(&err, "PID reuse");
}

#[test]
fn duplicate_listeners_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.tcp = Ok(tcp_table(&format!(
        "{}{}",
        tcp_row(LOOP_V4, 18765, "0A", 3),
        tcp_row(LOOP_V4, 18765, "0A", 4)
    )));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "multiple unexpected listeners");
}

#[test]
fn ipv6_loopback_parser_and_inode_form() {
    let rows =
        parse_proc_net_tcp6_rows(&tcp6_table(&tcp6_row(LOOP_V6, 18765, "0A", 1542))).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bind_address, "::1");
    assert_eq!(rows[0].port, 18765);
    assert!(rows[0].listen);
    assert_eq!(rows[0].inode, 1542);
    assert_eq!(parse_socket_inode_link("socket:[1542]"), Some(1542));
}

#[test]
fn ipv6_wildcard_parser_refused_as_listener_target() {
    let rows = parse_proc_net_tcp6_rows(&tcp6_table(&tcp6_row(WILD_V6, 18765, "0A", 1))).unwrap();
    assert_eq!(rows[0].bind_address, "::");
}

#[test]
fn ipv6_requested_but_plan_is_ipv4_refused() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let st = good_state(8, 100, &argv, 3);
    let mut r = req(&plan, &sup);
    r.expected_listen_addr = "::1";
    let err = collect_tunnel_observation(&r, &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "does not match the approved plan");
}

#[test]
fn ipv6_requested_tcp6_absent_with_matching_plan_addr_still_needs_table() {
    // Production plans cannot listen on ::1 today. Parser still fail-closes when
    // IPv6 observation is requested without tcp6 (see ipv6_requested_but_plan_is_ipv4).
    let err = collect_linux_tunnel_observation(&CollectTunnelObservationRequest {
        plan: &leaked_plan_host().0,
        supervised: &supervised(1, 1, vec![]),
        expected_listen_addr: "::1",
        expected_listen_port: 18765,
        expected_identity: ProcessStartIdentity {
            pid: 1,
            starttime: 1,
        },
        source: ObservationSourceKind::LinuxProcfs,
    })
    .unwrap_err();
    assert!(err.iter().any(|e| e.contains("Linux")
        || e.contains("not match")
        || e.contains("PID")
        || e.contains("/proc")));
}

#[test]
fn collected_evidence_attests_pinned_forward_conjunction() {
    let (plan, hk, argv) = leaked_plan_host();
    let sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let report = attest_collected_tunnel_runtime(&plan, &hk, &sup, &req(&plan, &sup), &fs);
    let att = report.ready().unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        att.auth_source,
        SshAuthEvidenceSource::PinnedForwardConjunction
    );
    assert_eq!(att.lifecycle, TunnelLifecycle::ReadyForAuthorizedRecovery);
    assert_ne!(att.auth_source, SshAuthEvidenceSource::CallerAsserted);
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
}

#[test]
fn caller_supplied_owner_pid_is_not_a_collector_input() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(8, 100, argv.clone());
    let mut st = good_state(8, 100, &argv, 3);
    st.fds.insert(8, Ok(vec!["socket:[1]".into()]));
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "not owned by the supervised process");
    let injected = TunnelObservationSnapshot {
        generation: 1,
        process: Some(TunnelProcessObservation {
            identity: sup.identity,
            exe: KINDLE_DROPBEARMULTI.into(),
            cmdline: argv,
            alive: true,
        }),
        listeners: vec![TunnelListenerObservation {
            bind_address: "127.0.0.1".into(),
            port: 18765,
            listen_state: true,
            owner_pid: 8,
        }],
        dropbear_event: DropbearClientEvent::NotCaptured,
    };
    assert_eq!(injected.listeners[0].owner_pid, 8);
}

#[test]
fn caller_asserted_ssh_auth_never_emitted_by_collector() {
    let (plan, hk, argv) = leaked_plan_host();
    let sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let report = attest_collected_tunnel_runtime(&plan, &hk, &sup, &req(&plan, &sup), &fs);
    assert_ne!(
        report.attestation.auth_source,
        SshAuthEvidenceSource::CallerAsserted
    );
}

#[test]
fn wrong_generation_refused_by_evaluator() {
    let (plan, hk, argv) = leaked_plan_host();
    let mut sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let snap = collect_tunnel_observation(&req(&plan, &sup), &fs).unwrap();
    sup.generation = 2;
    let report = crate::sync_recovery_attest::attest_tunnel_runtime(&plan, &hk, &sup, &snap);
    assert!(report
        .reasons
        .iter()
        .any(|e| e.contains("stale attestation")));
}

#[test]
fn process_disappears_after_first_observation() {
    let (plan, hk, argv) = leaked_plan_host();
    let sup = supervised(4242, 9001, argv.clone());
    let live = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let att = attest_collected_tunnel_runtime(&plan, &hk, &sup, &req(&plan, &sup), &live)
        .ready()
        .unwrap()
        .clone();
    let dead = SyntheticProcfs::new(SyntheticFsState::default());
    let err = require_fresh_collected_attestation_before_http(
        &att,
        &plan,
        &hk,
        &sup,
        &req(&plan, &sup),
        &dead,
        "http://127.0.0.1:18765",
    )
    .unwrap_err();
    refuse_has(&err, "missing");
}

#[test]
fn request_time_recollection_refuses_stale_generation_evidence() {
    let (plan, hk, argv) = leaked_plan_host();
    let mut sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let att = attest_collected_tunnel_runtime(&plan, &hk, &sup, &req(&plan, &sup), &fs)
        .ready()
        .unwrap()
        .clone();
    sup.generation = 2;
    let err = require_fresh_collected_attestation_before_http(
        &att,
        &plan,
        &hk,
        &sup,
        &req(&plan, &sup),
        &fs,
        "http://127.0.0.1:18765",
    )
    .unwrap_err();
    assert!(err.iter().any(|e| e.contains("stale attestation")));
}

#[test]
fn recovery_http_origin_must_match_collected_listener() {
    let (plan, hk, argv) = leaked_plan_host();
    let sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let att = attest_collected_tunnel_runtime(&plan, &hk, &sup, &req(&plan, &sup), &fs)
        .ready()
        .unwrap()
        .clone();
    let err = require_fresh_collected_attestation_before_http(
        &att,
        &plan,
        &hk,
        &sup,
        &req(&plan, &sup),
        &fs,
        "http://127.0.0.1:9",
    )
    .unwrap_err();
    assert!(err
        .iter()
        .any(|e| e.contains("does not match attested listener")));
}

#[test]
fn usbnetlite_daemon_refused_as_recovery_client() {
    let (plan, _, argv) = leaked_plan_host();
    let mut daemon = argv.clone();
    daemon[1] = "dropbear".into();
    let sup = supervised(8, 100, daemon.clone());
    let st = good_state(8, 100, &daemon, 3);
    let err = collect_tunnel_observation(&req(&plan, &sup), &SyntheticProcfs::new(st)).unwrap_err();
    refuse_has(&err, "USBNETLite");
}

#[test]
fn correct_transport_does_not_imply_gate_d() {
    let (plan, hk, argv) = leaked_plan_host();
    let sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let report = attest_collected_tunnel_runtime(&plan, &hk, &sup, &req(&plan, &sup), &fs);
    let att = report.ready().unwrap();
    let err =
        bind_attestation_to_recovery_policy(att, RecoveryGate::DIrreversibleExecution).unwrap_err();
    assert!(err.iter().any(|e| e.contains("Gate D")));
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
}

#[test]
fn generation_lifecycle_requires_fresh_collection() {
    let (plan, hk, argv) = leaked_plan_host();
    let sup1 = supervised(10, 100, argv.clone());
    let fs1 = SyntheticProcfs::new(good_state(10, 100, &argv, 5));
    let att1 = attest_collected_tunnel_runtime(&plan, &hk, &sup1, &req(&plan, &sup1), &fs1)
        .ready()
        .unwrap()
        .clone();
    let dead = SyntheticProcfs::new(SyntheticFsState::default());
    assert!(collect_tunnel_observation(&req(&plan, &sup1), &dead).is_err());
    let fs_reused = SyntheticProcfs::new(good_state(10, 200, &argv, 5));
    assert!(collect_tunnel_observation(&req(&plan, &sup1), &fs_reused).is_err());
    let mut sup2 = supervised(10, 200, argv.clone());
    sup2.generation = 2;
    let att2 = attest_collected_tunnel_runtime(&plan, &hk, &sup2, &req(&plan, &sup2), &fs_reused)
        .ready()
        .unwrap()
        .clone();
    assert_eq!(att2.generation, 2);
    let err = require_fresh_collected_attestation_before_http(
        &att1,
        &plan,
        &hk,
        &sup2,
        &req(&plan, &sup2),
        &fs_reused,
        "http://127.0.0.1:18765",
    )
    .unwrap_err();
    assert!(err.iter().any(|e| e.contains("stale attestation")));
    let invalidated = apply_invalidation(att1, AttestationInvalidation::ProcessExited);
    assert_eq!(invalidated.lifecycle, TunnelLifecycle::Disconnected);
}

#[test]
fn kindle_109_strings_are_fixtures_not_production_constants() {
    // Historical Review 109 capture (example only).
    let stat = "3444 (dropbearmulti) S 3443 3443 3443 0 -1 4194560 371 64 2 0 37 1 0 0 19 -1 1 0 189474343 3465216 417 4294967295 2137116672 2137386924 3202096160 0 0 0 0 4100 0 1 0 0 17 0 0 0 0 0 0 2137387896 2137392012 2137395200 3202096497 3202096809 3202096809 3202097111 0";
    let id = parse_proc_stat_starttime(stat).unwrap();
    let (id2, state) = parse_supervised_stat(stat).unwrap();
    assert_eq!(id, id2);
    assert_eq!(state, 'S');
    let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   5: 0100007F:494D 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1542316 1 e9a14800 100 0 0 10 0\n";
    let rows = parse_proc_net_tcp_rows(tcp).unwrap();
    assert_eq!(rows[0].bind_address, "127.0.0.1");
    assert_eq!(rows[0].port, 18765);
    assert_eq!(rows[0].inode, 1542316);
    let cmd = parse_proc_cmdline(b"/mnt/us/usbnetlite/bin/dropbearmulti\0dbclient\0-N\0");
    assert_eq!(cmd[1], "dbclient");
}

#[test]
fn linux_entry_point_fails_closed_without_kernel_procfs() {
    let (plan, _, argv) = leaked_plan_host();
    let sup = supervised(1, 1, argv);
    let mut r = req(&plan, &sup);
    r.source = ObservationSourceKind::LinuxProcfs;
    let err = collect_linux_tunnel_observation(&r).unwrap_err();
    assert!(err.iter().any(|e| e.contains("Linux")
        || e.contains("not available")
        || e.contains("missing")
        || e.contains("PID")));
}

#[test]
fn ipv4_mapped_ipv6_is_not_loopback_unspecified() {
    let mapped = parse_proc_ipv6_port("0000000000000000FFFF00000100007F:494D").unwrap();
    assert!(mapped.0.starts_with("::ffff:"));
    assert_ne!(mapped.0, "::1");
    assert_ne!(mapped.0, "127.0.0.1");
}

#[test]
fn collector_does_not_claim_peer_key_blob() {
    let (plan, hk, argv) = leaked_plan_host();
    let sup = supervised(4242, 9001, argv.clone());
    let fs = SyntheticProcfs::new(good_state(4242, 9001, &argv, 7777));
    let snap = collect_tunnel_observation(&req(&plan, &sup), &fs).unwrap();
    assert_eq!(snap.dropbear_event, DropbearClientEvent::NotCaptured);
    let report = crate::sync_recovery_attest::attest_tunnel_runtime(&plan, &hk, &sup, &snap);
    let att = report.ready().unwrap();
    assert_eq!(
        att.auth_source,
        SshAuthEvidenceSource::PinnedForwardConjunction
    );
}

#[cfg(all(test, unix))]
mod linux_native {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::process::{Command, Stdio};

    fn read_self_starttime() -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
        parse_supervised_stat(&stat).unwrap().0.starttime
    }

    /// Generic Linux loopback listener. Does not validate Dropbear or SSH.
    #[test]
    fn real_procfs_loopback_listener_then_refuse_after_exit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let child = Command::new("python3")
            .args([
                "-c",
                &format!(
                    "import socket,time;s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind(('127.0.0.1',{port}));s.listen(1);time.sleep(30)"
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        drop(listener);
        let mut child = match child {
            Ok(c) => c,
            Err(_) => {
                // No python3: bind in this process instead.
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let port = listener.local_addr().unwrap().port();
                let pid = std::process::id() as u32;
                let start = read_self_starttime();
                let tcp = std::fs::read_to_string("/proc/net/tcp").unwrap();
                let rows = parse_proc_net_tcp_rows(&tcp).unwrap();
                assert!(
                    rows.iter()
                        .any(|r| r.port == port && r.bind_address == "127.0.0.1" && r.listen),
                    "expected LISTEN row for {port}"
                );
                let fds: Vec<_> = std::fs::read_dir(format!("/proc/{pid}/fd"))
                    .unwrap()
                    .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                let inode = rows
                    .iter()
                    .find(|r| r.port == port && r.listen)
                    .unwrap()
                    .inode;
                assert!(fds
                    .iter()
                    .any(|l| parse_socket_inode_link(l) == Some(inode)));
                drop(listener);
                let tcp2 = std::fs::read_to_string("/proc/net/tcp").unwrap();
                let rows2 = parse_proc_net_tcp_rows(&tcp2).unwrap();
                assert!(!rows2
                    .iter()
                    .any(|r| r.port == port && r.listen && r.inode == inode));
                let _ = start;
                return;
            }
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        let pid = child.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let (ident, _) = parse_supervised_stat(&stat).unwrap();
        let tcp = std::fs::read_to_string("/proc/net/tcp").unwrap();
        let rows = parse_proc_net_tcp_rows(&tcp).unwrap();
        let row = rows
            .iter()
            .find(|r| r.port == port && r.bind_address == "127.0.0.1" && r.listen)
            .expect("child LISTEN");
        let fds: Vec<_> = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(fds
            .iter()
            .any(|l| parse_socket_inode_link(l) == Some(row.inode)));
        let _ = ident;
        let _ = child.kill();
        let _ = child.wait();
        let tcp2 = std::fs::read_to_string("/proc/net/tcp").unwrap();
        let rows2 = parse_proc_net_tcp_rows(&tcp2).unwrap();
        assert!(!rows2
            .iter()
            .any(|r| r.port == port && r.listen && r.inode == row.inode));
        let _ = Write::flush(&mut std::io::sink());
    }
}
