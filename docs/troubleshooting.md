# Ignite troubleshooting (Kindle)

Verified behavior from installer scripts and Diagnose helpers. This is not a
guarantee for every firmware.

## Prerequisites

Ignite needs a Kindle **already prepared** for third-party apps:

- Jailbreak / community firmware tooling
- SH Integration (or equivalent Scriptlet runner)
- A working root helper (commonly `su` / `sudo` on the device)

Ignite **will not** jailbreak a stock Kindle. If the installer reports a missing
privilege helper, stop and prepare the device first using the community guide at
<https://kindlemodding.org/jailbreaking/>.

## Missing privilege helper

**Symptom:** Install fails mentioning root helper / `su` / preparation.

**Meaning:** The device is not prepared for third-party privileged install.

**Action:** Complete jailbreak / SH Integration setup via
<https://kindlemodding.org/jailbreaking/>; do not expect Ignite to
bypass this.

## SH Integration / Scriptlet problems

**Symptom:** `Install Ignite v*.sh` does not appear, will not open, or exits
immediately.

**Checks:**

1. File is in **Documents** (not a nested ZIP folder).
2. Filename starts with `Install Ignite` as packaged.
3. SH Integration / Scriptlet support is installed for your firmware.
4. Copy the file again over USB; avoid editors that change line endings.

## App registration issues

**Symptom:** Install claims success but Ignite never appears in the Library, or
opens the wrong command.

**Action:**

1. Open **Diagnose Ignite** (or copy `Ignite-Diagnose.txt` from Documents).
2. Confirm `registration_state=canonical_only` and handler `com.markrai.ignite`
   points at `/mnt/us/scribe-app/bin/scribe-lifecycle-host.sh`.
3. If Diagnose reports `legacy_only` or `both_present`, re-run **Install Ignite**
   from Home (app not running) to finish the `com.markrai.scribe` →
   `com.markrai.ignite` migration.
4. If needed, **Uninstall Ignite** (notebooks kept) then reinstall.

## Launch returns to Kindle Home

**Symptom:** Tapping Ignite briefly shows a splash or blank panel then returns
to Home / Library.

**Common causes:**

- Binary/architecture mismatch (need ARMv7hf build for the Scribe)
- Lifecycle host misconfigured (`--data-dir` / `--sync-dir` missing)
- App crashed on startup (panel / permissions)

**Action:** Collect Diagnose output; reinstall matching Scriptlet version; ensure
you did not mix host scripts from an older package with new binaries.

## Diagnostic Scriptlet

After a successful unpack, **Diagnose Ignite** is registered. It writes
`Ignite-Diagnose.txt` under Documents even when Library registration is
incomplete (installer also writes a report on failure paths).

Copy that file over USB when asking for help. Redact personal notebook titles if
you paste excerpts publicly.

## Version and firmware reporting

Diagnose includes package version and device/firmware fields. When reporting
issues, include:

- Ignite version (filename / Diagnose)
- Kindle generation (e.g. Scribe 1st gen / 2024 Scribe)
- Firmware string (e.g. 5.18.6)
- Panel polarity / grayscale notes if ink looks inverted

## Safe uninstall

**Uninstall Ignite** removes application files and Library registration.
**Notebooks under the data directory are preserved** by design.

Always run **Backup Ignite Data** before major experiments and copy the backup
folder off-device over USB.

## Sync / pairing (optional)

Ordinary handwriting does not require a server. If you enable self-hosted sync:

- Use a trusted LAN
- Do not expose sync ports to the public Internet without understanding the
  threat model
- Do not enable experimental server lineage-replacement acceptance unless you
  are following a controlled recovery procedure (not part of normal install)
