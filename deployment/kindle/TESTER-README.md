# Ignite 0.9.11 — namespace migration candidate

Ignite is a local handwriting notebook for a **prepared Kindle Scribe**. Everyday use does not require a computer or a server. For jailbreaking / preparation, follow <https://kindlemodding.org/jailbreaking/>.

This **0.9.11** candidate changes the Kindle Library registration identity from
`com.markrai.scribe` to **`com.markrai.ignite`**, and keeps polarity encoding from 0.9.10:

- 2022 Scribe / firmware 5.18.6 / `grayscale=2` — identity
- 2024 Scribe / firmware 5.19.6 / `grayscale=1` — complement

Notebooks stay at `/mnt/us/scribe-data`. Sync discovery remains `_scribe._tcp`.

Developer device (Scribe / 5.18.6) has completed the 0.9.11 namespace upgrade
(`registration_state=canonical_only`). External tester confirmation on 2024 Scribe
polarity remains useful before calling every firmware path stable.

## Install / upgrade

1. Open **Backup Ignite Data** and copy the backup folder off the Kindle.
2. Leave Ignite. Stay on Home / Library.
3. Copy **`Install Ignite v0.9.11.sh`** (version in the filename) into Documents.
4. Open **Install Ignite** once from the Library.
5. Open **Diagnose Ignite** and send `Ignite-Diagnose.txt` if asked.

Diagnose should show `registration_state=canonical_only`,
`canonical_handler=com.markrai.ignite`, and `canonical_command_ok=yes`.
If you still see `legacy_only` or `both_present`, re-run Install Ignite once.

## Backup / uninstall

**Backup Ignite Data** copies notebooks over USB. **Uninstall Ignite** removes the app and keeps notebooks.
