# Data, backups and upgrades

[Back to the quick start](../README.md) · [中文](data-and-upgrades.zh_CN.md)

## Where data lives

Settings, channels, rules and discovery statistics live in `data/bilistream.db` beside the executable. Configuration and login credentials are encrypted; the application unlocks them automatically on this computer. Change settings through the Web UI. `BILISTREAM_DATA_DIR` selects another data directory; `BILISTREAM_KEY_FILE` can select a protected key file **outside** it.

Keep the original key: copying the database alone is not a portable backup. Do not delete database `-wal` or `-shm` files. Images remain in cache directories, and `webui/` contains page assets.

## Analytics history

The **统计** tab reads `history.sqlite` in the same data directory. Each server records its own relay sessions, reconnects, stream quality, Bilibili audience counts and notification attempts. In a cluster, **全部节点** combines the available servers when you open the page; a missing server is named in the partial-data notice. Dates and times use your browser's local time zone.

History is separate from settings and is excluded from downloaded backups and legacy exports. To preserve it when moving a server, stop bilistream and copy its history database together with any matching `-wal` and `-shm` files. To clear it, stop bilistream and remove those history files; the next start creates a new history database. Keep `bilistream.db` and its files intact.

By default, raw samples are retained for 7 days, minute summaries for 90 days, and events for up to 180 days. Bilibili totals depend on the room's login and available upstream data; `--` means a total is unavailable. A missing total does not delay closing a relay session. See [history settings](advanced-settings.md#analytics-history) for retention and recording controls.

## Back up and restore

Use **System Settings → 数据与备份** to download a password-protected backup. Keep its password separately. A new installation can restore it through **已有备份？直接恢复** in the setup wizard. Backups exclude the panel password and browser login sessions. Restore preserves the destination installation’s password choice; a fresh installation needs local password setup or initial password bootstrap.

Backups never contain cluster identities. A backup from a cluster member restores as a server outside any cluster, with monitoring off; create or join a cluster again afterwards. A server that is in a cluster or preparing to join one cannot restore a backup.

![Encrypted backup settings](images/data-backup.png)

## Upgrade

Stop the old process before starting the new binary. Upgrade imports existing JSON/Cookie files automatically, including older configurations missing newer settings; those settings use their defaults. Original app-owned files are retired only after a verified encrypted recovery copy. External Cookie files are left untouched.

In a cluster, each server keeps its own database and key. Old and new builds cannot run together: before upgrading, stop every server and take a physical backup of each one (stopped data directory, key file, executable and launch command). Then upgrade all servers and create the cluster again from **System Settings → 集群节点**.

## Downgrade

Stop the service and run this command with the **new** binary:

```bash
./bilistream --export-legacy ./downgrade-data
```

The new directory contains plaintext settings and credentials. Use it with the old binary and remove it when no longer needed.

Older binaries cannot read the panel password saved by the Web UI. Stop the service before rolling back. When starting an older binary on a remote address, explicitly supply `--password-file` or `BILISTREAM_PASSWORD`, or start locally first. Reverting the binary alone does not preserve the newer password protection. Legacy exports also exclude the panel password. Keep the original database and key so you can return to the newer version.

Once a server has created, prepared to join or joined a cluster, or holds a paused old cluster configuration, older versions refuse to open its database. A legacy export from such a server contains a configuration outside any cluster, with monitoring off. To roll back a cluster, stop every server and restore each one’s physical backup taken before the upgrade, all together. Never run old and new versions in one cluster, or replace the executable on only some servers.
