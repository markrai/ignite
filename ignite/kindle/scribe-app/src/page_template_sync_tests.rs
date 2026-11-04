//! Slice 3: sync compatibility for page templates (host fixtures / loopback server only).

use std::collections::BTreeSet;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use sync_protocol::{snapshot_sha256, BackgroundEncoding, BackgroundRef, SyncOperationV1};

use super::tests::{
    env, geo, local_snapshot, nb_dir, pending_seqs, replay_local, server, stop, wait_until, write,
    write_config,
};
use super::*;
use crate::library::{
    create_notebook_durable_with_template, open_library_read_only, CreateNotebookHooks,
};
use crate::notebook::{insert_page_after_durable, InsertPageHooks};
use crate::paper_templates::{generate_page_background, PageTemplate, PAPER_WHITE};
use crate::profile::RenderProfile;
use crate::store::{install_background, load_existing_page, BackgroundIdentityV1};

fn profile() -> RenderProfile {
    RenderProfile::sealed_v2_pressure(0x00).unwrap()
}

fn templates() -> [PageTemplate; 4] {
    [
        PageTemplate::Blank,
        PageTemplate::Ruled,
        PageTemplate::Grid,
        PageTemplate::Dotted,
    ]
}

fn durable_bg(template: PageTemplate) -> Vec<u8> {
    let g = geo();
    let mut bg = generate_page_background(template, g.xres, g.yres).unwrap();
    crate::page_label::scrub_chrome_band(&mut bg, &g).unwrap();
    bg
}

fn create_templated(
    sync: &OutboundSync,
    lib_root: &Path,
    template: PageTemplate,
    title: &str,
    after: Option<&str>,
) -> (String, Vec<u8>) {
    let g = geo();
    let mut lib = open_library_read_only(lib_root).unwrap();
    let cn = create_notebook_durable_with_template(
        &mut lib,
        &profile(),
        &g,
        template,
        Some(title),
        &CreateNotebookHooks::default(),
    )
    .unwrap();
    let nb = lib.notebook_dir(&cn.notebook_id);
    let bg = fs::read(nb.join("page-1").join("background.raw")).unwrap();
    assert_eq!(bg, durable_bg(template), "{template:?} durable vs generator");
    sync.notebook_created(&nb, &cn.notebook_id, &cn.title, after);
    (cn.notebook_id, bg)
}

fn last_create_notebook(sync_dir: &Path) -> SyncOperationV1 {
    let ops = parse_journal(&fs::read(sync_dir.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed;
    ops.into_iter()
        .rev()
        .map(|o| o.envelope.operation)
        .find(|o| matches!(o, SyncOperationV1::CreateNotebook { .. }))
        .expect("CreateNotebook in outbox")
}

fn blob_files(sync_dir: &Path) -> BTreeSet<String> {
    let dir = sync_dir.join(BLOB_DIR);
    if !dir.exists() {
        return BTreeSet::new();
    }
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn library_id(sync_dir: &Path) -> LibraryId {
    read_json::<SyncStateV1>(&sync_dir.join(STATE_FILE))
        .unwrap()
        .library_id
}

fn assert_blob_ref_matches(bg: &BackgroundRef, bytes: &[u8], w: u32, h: u32) {
    match bg {
        BackgroundRef::Blank => {
            assert!(bytes.iter().all(|&b| b == PAPER_WHITE));
        }
        BackgroundRef::Blob {
            sha256,
            byte_len,
            format,
        } => {
            assert_eq!(*byte_len, bytes.len() as u64);
            assert_eq!(sha256, &sha256_hex(bytes));
            assert_eq!(format.encoding, BackgroundEncoding::Gray8);
            assert_eq!(format.width, w);
            assert_eq!(format.height, h);
            assert_eq!(format.stride, w); // packed generator output
            assert_eq!(format.stride * format.height, *byte_len as u32);
        }
    }
}

#[test]
fn durable_template_create_captures_create_notebook_and_blob_identity() {
    let e = env("tmpl-out-capture");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let before_blobs = blob_files(&e.sync);

    let mut after = "notebook-1".to_string();
    let mut blank_blob_baseline = before_blobs.clone();
    for template in templates() {
        let title = format!("{template:?}");
        let pending0 = s.pending();
        let (nb_id, bg) = create_templated(&s, &e.lib, template, &title, Some(&after));
        wait_until("journal", Duration::from_secs(5), || s.pending() > pending0);
        let SyncOperationV1::CreateNotebook {
            first_page,
            title: t,
            ..
        } = last_create_notebook(&e.sync)
        else {
            panic!("expected CreateNotebook");
        };
        assert_eq!(t, title);
        let g = geo();
        assert_blob_ref_matches(&first_page.background, &bg, g.xres, g.yres);
        match template {
            PageTemplate::Blank => {
                assert!(matches!(first_page.background, BackgroundRef::Blank));
                assert_eq!(
                    blob_files(&e.sync),
                    blank_blob_baseline,
                    "Blank must not store a background blob"
                );
            }
            _ => {
                let BackgroundRef::Blob { sha256, .. } = &first_page.background else {
                    panic!("{template:?} must be Blob");
                };
                let stored = fs::read(e.sync.join(BLOB_DIR).join(sha256.as_str())).unwrap();
                assert_eq!(stored, bg);
                assert!(matches!(
                    BackgroundIdentityV1::from_pixels(&bg),
                    BackgroundIdentityV1::Blob { .. }
                ));
                blank_blob_baseline.insert(sha256.to_string());
            }
        }
        // Inserted page inherits and captures CreatePage with the same background ref.
        let mut m: crate::notebook::PersistedNotebookManifestV1 =
            serde_json::from_slice(&fs::read(nb_dir(&e.lib, &nb_id).join("notebook.meta")).unwrap())
                .unwrap();
        let pending1 = s.pending();
        let ins = insert_page_after_durable(
            &nb_dir(&e.lib, &nb_id),
            &mut m,
            "page-1",
            &InsertPageHooks::default(),
        )
        .unwrap();
        let page2_bg = fs::read(
            nb_dir(&e.lib, &nb_id)
                .join(&ins.inserted_page_id)
                .join("background.raw"),
        )
        .unwrap();
        assert_eq!(page2_bg, bg, "{template:?} insert inherits background");
        s.page_created(
            &nb_dir(&e.lib, &nb_id),
            &nb_id,
            &ins.inserted_page_id,
            Some("page-1"),
        );
        wait_until("create_page", Duration::from_secs(5), || s.pending() > pending1);
        let create_page = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
            .unwrap()
            .committed
            .into_iter()
            .rev()
            .map(|o| o.envelope.operation)
            .find(|o| matches!(o, SyncOperationV1::CreatePage { .. }))
            .unwrap();
        let SyncOperationV1::CreatePage { page, .. } = create_page else {
            unreachable!()
        };
        assert_blob_ref_matches(&page.background, &page2_bg, g.xres, g.yres);
        after = nb_id;
    }
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
    stop(s);
}

#[test]
fn identical_ruled_backgrounds_dedupe_to_one_blob_and_upload_once() {
    let srv = server("tmpl-dedupe");
    let e = env("tmpl-dedupe");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let (nb_a, bg_a) = create_templated(&s, &e.lib, PageTemplate::Ruled, "Ruled-A", Some("notebook-1"));
    let (nb_b, bg_b) = create_templated(&s, &e.lib, PageTemplate::Ruled, "Ruled-B", Some(&nb_a));
    assert_eq!(bg_a, bg_b);
    let sha = sha256_hex(&bg_a);
    assert_eq!(
        blob_files(&e.sync).iter().filter(|n| *n == sha.as_str()).count(),
        1
    );
    // Only one Ruled blob file overall among nonblank templates created here.
    let ruled_blobs: Vec<_> = blob_files(&e.sync)
        .into_iter()
        .filter(|n| n == sha.as_str())
        .collect();
    assert_eq!(ruled_blobs, vec![sha.to_string()]);

    wait_until("upload", Duration::from_secs(30), || {
        s.pending() == 0 && s.state().acked_client_seq >= 2
    });
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let lid = library_id(&e.sync);
    let (st, body) = srv.get(&format!("/v1/blobs/{sha}"));
    assert!(st < 300, "blob GET status {st} body={body}");
    // Snapshot must reference the same SHA twice and match local.
    let local = local_snapshot(&e.lib);
    let refs: Vec<_> = local
        .notebooks
        .iter()
        .filter(|n| {
            // titles Ruled-A / Ruled-B
            n.title.starts_with("Ruled-")
        })
        .flat_map(|n| n.pages.iter())
        .filter_map(|p| match &p.background {
            BackgroundRef::Blob { sha256, .. } => Some(sha256.as_str().to_string()),
            _ => None,
        })
        .collect();
    assert!(refs.len() >= 2);
    assert!(refs.iter().all(|r| r == sha.as_str()));
    assert_eq!(
        srv.snapshot_hash(&lid),
        snapshot_sha256(&local).unwrap().as_str()
    );
    let _ = nb_b;
    stop(s);
}

#[test]
fn blank_template_uploads_without_background_blob() {
    let srv = server("tmpl-blank");
    let e = env("tmpl-blank");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let before = blob_files(&e.sync);
    create_templated(&s, &e.lib, PageTemplate::Blank, "Blank-T", Some("notebook-1"));
    wait_until("upload", Duration::from_secs(20), || {
        s.pending() == 0 && s.state().acked_client_seq >= 1
    });
    assert_eq!(blob_files(&e.sync), before);
    let local = local_snapshot(&e.lib);
    let blank_nb = local
        .notebooks
        .iter()
        .find(|n| n.title == "Blank-T")
        .unwrap();
    assert!(matches!(
        blank_nb.pages[0].background,
        BackgroundRef::Blank
    ));
    assert_eq!(
        srv.snapshot_hash(&library_id(&e.sync)),
        snapshot_sha256(&local).unwrap().as_str()
    );
    stop(s);
}

#[test]
fn offline_patterned_create_survives_restart_then_uploads_without_duplication() {
    let e = env("tmpl-offline");
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    write_config(&e.sync, &dead_url, &"0".repeat(64), false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let (nb_id, bg) = create_templated(
        &s,
        &e.lib,
        PageTemplate::Grid,
        "Offline-Grid",
        Some("notebook-1"),
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() >= 1);
    wait_until("retry", Duration::from_secs(10), || {
        s.stats().transport_errors >= 1
    });
    assert!(s.fault().is_none());
    let pending_before = pending_seqs(&e.sync);
    let sha = sha256_hex(&bg);
    assert!(e.sync.join(BLOB_DIR).join(sha.as_str()).exists());
    stop(s);

    // Local notebook remains durable across restart.
    let reopened_bg = fs::read(
        nb_dir(&e.lib, &nb_id)
            .join("page-1")
            .join("background.raw"),
    )
    .unwrap();
    assert_eq!(reopened_bg, bg);

    let srv = server("tmpl-offline");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert_eq!(pending_seqs(&e.sync), pending_before);
    wait_until("upload", Duration::from_secs(30), || {
        s.pending() == 0 && s.state().acked_client_seq >= pending_before.len() as u64
    });
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let lid = library_id(&e.sync);
    assert_eq!(
        srv.snapshot_hash(&lid),
        snapshot_sha256(&local_snapshot(&e.lib)).unwrap().as_str()
    );
    // Re-sync converges without changing the background hash.
    let hash1 = sha.clone();
    write(
        &s,
        &e.lib,
        &nb_id,
        "page-1",
        &[(10.0, 10.0), (20.0, 40.0)],
    );
    wait_until("stroke", Duration::from_secs(20), || {
        s.state().acked_client_seq >= pending_before.len() as u64 + 1
    });
    let bg_after = fs::read(
        nb_dir(&e.lib, &nb_id)
            .join("page-1")
            .join("background.raw"),
    )
    .unwrap();
    assert_eq!(sha256_hex(&bg_after), hash1);
    assert_eq!(
        srv.snapshot_hash(&lid),
        snapshot_sha256(&local_snapshot(&e.lib)).unwrap().as_str()
    );
    stop(s);
}

#[test]
fn unjournaled_background_replacement_faults_as_underivable() {
    let srv = server("tmpl-mutate");
    let e = env("tmpl-mutate");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let (nb_id, _bg) = create_templated(
        &s,
        &e.lib,
        PageTemplate::Dotted,
        "Mutate-Me",
        Some("notebook-1"),
    );
    wait_until("upload", Duration::from_secs(20), || {
        s.pending() == 0 && s.state().acked_client_seq >= 1
    });
    stop(s);

    // Unauthorized local background rewrite (no sync journal op exists for this).
    let page = nb_dir(&e.lib, &nb_id).join("page-1");
    let (_store, mut loaded) = load_existing_page(&nb_dir(&e.lib, &nb_id), "page-1").unwrap();
    let mut tampered = loaded.background.clone();
    tampered[0] ^= 0x7F;
    install_background(&page, &tampered, &mut loaded.header).unwrap();

    let before = local_snapshot(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let fault = s.fault().expect("must fault on background change");
    assert!(
        fault.contains("background/render profile changed")
            || fault.contains("not derivable")
            || fault.contains("diverged"),
        "{fault}"
    );
    assert!(!s.upload_permitted());
    stop(s);
    // Notebook files left as the caller wrote them; sync did not rewrite them away.
    assert_eq!(local_snapshot(&e.lib), before);
}

#[test]
fn server_rejects_wrong_blob_hash_for_template_bytes() {
    let srv = server("tmpl-badblob");
    let e = env("tmpl-badblob");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("genesis", Duration::from_secs(20), || {
        s.state().genesis_uploaded
    });
    let lid = library_id(&e.sync);
    stop(s);

    let bytes = durable_bg(PageTemplate::Ruled);
    let real = sha256_hex(&bytes);
    let fake = sha256_hex(b"not-the-bytes");
    let (st, _) = {
        let r = ureq::put(&format!("{}/v1/blobs/{fake}", srv.url()))
            .set("Authorization", &format!("Bearer {}", srv.token))
            .send_bytes(&bytes);
        match r {
            Ok(r) => (r.status(), ()),
            Err(ureq::Error::Status(code, _)) => (code, ()),
            Err(err) => panic!("{err}"),
        }
    };
    assert!(
        st >= 400,
        "PUT under wrong sha must fail, got {st}; real={real}"
    );
    let (st_ok, _) = {
        let r = ureq::put(&format!("{}/v1/blobs/{real}", srv.url()))
            .set("Authorization", &format!("Bearer {}", srv.token))
            .send_bytes(&bytes);
        match r {
            Ok(r) => (r.status(), ()),
            Err(ureq::Error::Status(code, _)) => (code, ()),
            Err(err) => panic!("{err}"),
        }
    };
    assert!(st_ok < 300, "correct sha PUT must succeed ({st_ok})");
    let _ = lid;
}
