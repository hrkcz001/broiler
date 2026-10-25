# Storage contract: Scoop persist and portable data

Approved requirement update: Scoop installations must use Scoop-managed persistent data, not silently leave all user data in AppData. This supersedes AppData-only passages in the original architecture, parameter guide, implementation handoff and earlier reports. Existing binaries still use AppData until the implementation worker completes this migration. This document changes the plan, not the running application.

## 1. One storage context

Resolve a `StorageContext` once per process and pass it explicitly to configuration, logs, state, capabilities, CLI bootstrap, coordinator and DDC worker. Never let child processes independently rediscover a different storage root.

Root precedence:

1. Explicit `--config-dir <absolute-path>`: treat it as the unified data root, including settings, profiles, local state and bounded logs. Resolve a user-supplied relative path once in the CLI and propagate the absolute result. Do not fall back on failure.
2. A fixed `broiler.portable` marker beside the actual application binaries: select `<executable-directory>/data`. This is the default for the Scoop package; it also supports an intentional portable ZIP installation. Empty marker content, no executable/path directives. Never infer portable mode merely because an arbitrary data directory happens to exist.
3. Ordinary unmarked installation: retain known-folder AppData behavior for backwards compatibility: roaming settings/profiles and local state/cache/logs.

The portable/Scoop layout is:

```text
<application>/broiler.portable
<application>/data/settings.toml
<application>/data/profiles/*.toml
<application>/data/state.json
<application>/data/capabilities.json
<application>/data/logs/broiler.log
```

The marker does not require a hardcoded Scoop path, environment variable or probing the user's home directory. Resolve the actual binary behind a shim; do not use the working directory. For Scoop, the `data` directory is linked by Scoop to its persistent store. Use Scoop variables in manifest hooks; never hardcode `C:\Users\...\scoop` or assume `$env:SCOOP` is set.

Explicitly selecting a portable root that is read-only is an actionable error, not permission to switch to AppData or run elevated. Every GUI and CLI surface must report/use the same resolved roots and storage mode. Add `broiler paths --json`, and include storage roots/mode in `status --json` even when no daemon is running. A live daemon reports its root; a CLI using a conflicting explicit root must fail clearly, never write elsewhere offline or create a second hardware owner.

## 2. Scoop packaging

The draft manifest declares `"persist": "data"`. During staging prepare a marker and an empty `data` directory. Scoop directory linking/preservation is performed by Scoop itself, not by an application-created custom junction. The pre-install hook may ensure the marker and directory exist without starting the application. Verify the ordering of pre-install and persist linking against the actual Scoop version before shipping.

Do not put pre-enabled profiles or nonempty settings defaults in the release's data directory: this could shadow existing AppData profiles during first migration and can change one-profile startup behavior. Ship reference examples separately; creating/installing/linking data must not apply a profile or enable startup.

The application sees `<install>/data`, ordinarily exposed through Scoop's stable `apps/broiler/current/data` path. The persistent backing is `<Scoop root>/persist/broiler/data` for a normal manifest entry `persist: data`; verify actual paths in the install test. Support custom Scoop roots. Do not claim a stable `current` startup path until observed: `current_exe()` can expose a versioned location. Registration must resolve and verify the installation's stable executable path rather than trust incidental path spelling.

Ordinary `scoop uninstall broiler` retains persisted data; `scoop uninstall -p broiler` explicitly purges it. Never invoke purge during tests without a disposable fixture. Stop before upgrade/uninstall to release executable/data handles; remove only this installation's startup entry. No automated reinstall hook may re-import stale AppData files over existing persist data.

## 3. Existing AppData migration

Existing data on the owner's machine is real. Preserve it, including custom profiles and comments. Migration must be explicit, lossless, inspectable and separate from install/startup hardware actions.

Provide:

```text
broiler data migrate --from-appdata --dry-run
broiler data migrate --from-appdata
```

Destination is the resolved selected root, never an arbitrary hardcoded Scoop path. For unmarked AppData mode, report that migration is unnecessary rather than copying onto itself. If a daemon owns the source/destination, reject or coordinate a clean maintenance stop; do not merge while settings/state can be updated.

Migration policy:

- Copy, never move/delete originals. Dry-run lists sources, destinations, conflicts and ignored ephemeral files without changing data.
- Migrate roaming `settings.toml` and `profiles/*.toml` plus local persistent monitor identity/state and validated evidence if present and schema-compatible. Do not treat cached capabilities or logs as required persistent identity evidence; regenerate transient caches and keep logs bounded rather than copying unlimited old logs.
- Preserve file contents/comments byte-for-byte when compatible. Validate before marking complete. Record source version/schema and any intentionally skipped incompatible cache/state file.
- Destination absent: stage files in the destination filesystem and publish atomically. Existing identical destination: leave unchanged. Existing different destination: stop with a conflict report before any commit; never silently choose source or destination and never use a blanket overwrite flag.
- Preflight all files before committing, keep a bounded migration journal/rollback staging for partial filesystem failures, and make interrupted retries idempotent. Never report atomicity for a multi-file operation without an implementation that actually provides/recoverably journals it.
- No hardware writes, daemon residency, launch-at-login enablement or source deletion as a side effect of migration. Print completion and both root paths; do not claim the running binary changed mode if it did not.
- On a portable first launch with old AppData config and a new empty root, offer/require the migration choice before normal startup. In a headless command, fail actionably with the dry-run/migration command rather than ignoring the user's old profiles. No automatic silent import at logon. Intentional no-import must be explicit and recorded in the selected root.

Do not perform the owner's real migration in the planning session. The implementation worker first proves all migration cases in disposable directories, then asks for or follows explicit user authorization for migrating actual data. A test must never modify/delete the original AppData files.

## 4. Required tests and evidence

- Resolver precedence, marker present/absent, working-directory independence, shim launch, explicit root error, unwritable root with no fallback.
- Parent/daemon/worker use identical roots; conflicting roots fail closed under the one-owner invariant.
- Comment-preserving settings updates target the selected root atomically, not a leftover AppData helper.
- Migration dry-run, empty destination, identical rerun, conflicting destination, invalid source, interrupted copy, journal recovery, schema incompatibility and original-data preservation.
- Isolated tests never depend on real AppData or a running user's daemon. No settings/profile pre-seeding merely to make startup tests pass.
- Real Scoop install under its observed root: inspect data junction target, edit a profile/setting, upgrade or reinstall with preserved data, verify contents survive, normal uninstall/reinstall preserves them. Purge semantics only in a disposable fixture.
- Verify autostart's stable path and root after an upgrade. Release checksums, bucket CI and actual install remain mandatory.

## Source

Scoop documents persistent data linking and retention by default, with `-p` as explicit uninstall purge: https://github.com/ScoopInstaller/Scoop/wiki/Persistent-data . Layout and migration policy above are broiler design decisions, not claims that the existing implementation already supports them.

## Follow-up, 2026-10-25: implementation landed

The paragraphs above that say the binaries still use AppData describe the state when this contract
was written. As of commit `ea10f84` and the storage work after it, they are implemented:

- `src/storage.rs` resolves one `StorageContext` per process in the stated order and passes it to
  the CLI, the resident coordinator, migration and the log. A root that cannot be created or written
  is an error; there is no fallback.
- `broiler paths [--json]` reports mode, root, marker, settings, profiles, state and logs, and adds
  the running coordinator's own root when one answers. `status --json` carries the same block in both
  the running and the not-running case.
- `src/migrate.rs` implements the copy-only migration with a staged, journalled publication:
  dry run, empty destination, identical rerun, conflict, invalid source, interrupted copy, journal
  recovery, incompatible state schema and byte-for-byte source preservation are covered by tests on
  disposable directories. A journal entry is rolled back only when the published file still matches
  its recorded hash.
- `broiler data migrate --from-appdata [--dry-run | --decline] [--json]` is the only import path.
  `--decline` records the decision in `<root>/migration.json`. A portable first launch with an older
  AppData configuration refuses to start and prints the three commands.
- A request on the pipe names the root its caller resolved; the coordinator refuses a different root
  with `storage_conflict` instead of acting on data the caller never read.
- `persist: data` and the marker/`data` creation stay in `packaging/scoop/broiler.json.template`,
  guarded by `src/packaging.rs` tests. No pre-seeded profiles are shipped.

Not yet evidenced, and not claimed: a real Scoop install, the observed junction target under an
actual Scoop root, upgrade/uninstall retention, pre-install versus persist-linking ordering, and
autostart pointing at a stable `current` path. Migration was proven on disposable directories only;
the owner's real AppData was read for a dry run and not modified or copied.
