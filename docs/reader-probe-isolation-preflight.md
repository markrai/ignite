# Reader probe: isolated hardware rehearsal

Status: **isolation rehearsal only**. No MuPDF execution is authorized by this
script. This is not Ignite release physical QA or approval to run a reader
against the primary production Kindle.

## Why this is separate

The existing tools/qa/ignite_qa.py preflight correctly blocks an installed
production Ignite bound to /mnt/us/scribe-data, its sync/device state and
the canonical appmgrd handler. It remains unchanged. The ordinary scribe-app
must never be executed directly for QA. The standalone reader-probe does
not start the app or use its state but still parses native document content,
so it requires its own independently verified sandbox and risk review.

Our initial Kindle Scribe (firmware 5.18.6) demonstrated working mount namespaces,
su to UID 99 and a mounted memory cgroup. These observations alone do not
prove network and PID namespaces, bind-remount behavior, chroot dependencies,
or safe MuPDF execution.

## Rehearsal behavior

tools/qa/reader_probe_isolation_preflight.sh is an experimental, non-production
rehearsal for a first-generation Scribe on firmware 5.18.6. It:

1. Confirms root, device architecture and firmware.
2. Reports whether Ignite is active but neither stops nor launches it.
3. Creates separate mount, PID and network namespaces.
4. Makes mount propagation private within the new namespace.
5. Mounts a tiny temporary tmpfs root, binds system libraries read-only,
   and mounts an isolated /proc.
6. Enters that root via chroot, invokes BusyBox su as UID 99 (nobody),
   and checks that production library/app paths are absent.
7. Unmounts and removes its temporary filesystem, reporting
   READER_SANDBOX_PREFLIGHT_PASS only after all checks succeed.

It does not open a book, run reader-probe, allocate a memory cgroup,
register a second Ignite handler, change any application file, publish a
release, or alter the general QA preflight. The rehearsal temporarily creates
a directory under /tmp and mounts filesystems only within the new namespace.

## Manual experiment on a designated Kindle

Review the source and verify SSH access. From the Linux repository root:

~~~sh
python3 -m unittest discover -s tools/qa -p 'test_reader_sandbox_preflight.py'
scp tools/qa/reader_probe_isolation_preflight.sh root@kindle:/tmp/ignite-reader-isolation-preflight.sh
ssh root@kindle 'sh /tmp/ignite-reader-isolation-preflight.sh'
~~~

Only a READER_SANDBOX_PREFLIGHT_PASS is evidence that this particular
rehearsal passed on that device. If any stage fails, stop and collect stderr;
do not replace a failing check with a permissive fallback.

Even if this rehearsal passes, **do not execute the MuPDF reader yet**.
A follow-up review must establish hash-qualified handoff of the probe and
disposable corpus, production-app shutdown, bounded wall/CPU and address-space
limits, a dedicated enforced memory cgroup, reliable cleanup and failure
evidence. A chroot is not an independent kernel security boundary, and native
parser bugs or kernel faults can still affect this device. A disposable QA
Kindle remains the safer option.

The output is not DEVICE_PREFLIGHT_PASS, PHYSICAL_QA_PASS or authorization
for OTA or public release.
