# Security policy

## Reporting a vulnerability

Please report security issues **privately** using GitHub’s
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository (Security → Report a vulnerability), once the project is
published.

Do **not** open a public issue for:

- Remote code execution, privilege escalation, or auth bypass
- Exposure of notebook contents, sync tokens, or pairing secrets
- Flaws that would enable unintended recovery / lineage replacement

Include Ignite version, affected component, and steps to reproduce when safe.

If private reporting is unavailable for any reason, contact the repository
owner through the GitHub profile associated with this project—still without
posting exploit details publicly.

## Supported releases

Ignite is early open-source software. Security fixes target the latest
published source / installer on the default branch. Older Kindle Scriptlets may
not receive backports.

## Important security boundaries

- Ignite requires a **prepared** Kindle (jailbreak / community tooling, SH
  Integration, root helper). It does not jailbreak a stock device. Use the
  community guide at <https://kindlemodding.org/jailbreaking/>.
- Sync and pairing use local / self-hosted networking; treat LAN trust
  carefully. Kindle HTTP clients are not configured for public Internet TLS
  recovery paths.
- **Experimental recovery** code may exist in-tree for research and future
  controlled enablement. In published builds:
  - `LIVE_NON_LOOPBACK_POST_ENABLED` is **false**
  - Ordinary startup does not submit recovery operations
  - No production SSH keys or host pins are shipped
  - The test-only `kindle-supervisor-harness` is **not** part of the customer
    Scriptlet
- Notebook data on `/mnt/us/scribe-data` is user-private. Do not attach real
  notebooks or Diagnose dumps containing personal content to public issues
  without redaction.

## Threat model notes

Ignite runs as a privileged third-party app on a jailbroken Kindle. A malicious
or compromised companion server, USB host, or root process on the device is
outside the “stock Kindle app sandbox” threat model. Reports that assume a
fully compromised root shell are still welcome when they identify unexpected
Ignite-specific behavior.
