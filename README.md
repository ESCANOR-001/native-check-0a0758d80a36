# Isolated native backend validation

Experimental Rust backend source and synthetic protocol fixtures for Linux, Windows and macOS CI. This is not the published npm package or a production-ready migration.

All tests use temporary workspaces and fixture agents. No user profiles, tokens, chat history, device data or authenticated agent accounts are included. Passing CI does not establish live Claude/Codex Desktop synchronization or physical Android behavior; those require separate device checks.

Run `cargo test --locked --all-targets -- --test-threads=1`. Background child processes use hidden windows on Windows.

Successful jobs retain their single native test executable as a seven-day Actions
artifact, identified by operating system, runner architecture and source commit.
These are unsigned experimental binaries for isolated validation, not production
installers or npm releases. No configuration, test workspace or credentials are
included in the binary artifact. A passing matrix does not qualify other CPU
architectures or older operating-system versions.

Native profile handoff uses verified, quiesced exports and isolated working copies.
The connection retains its native generation across restarts. Data rollback refuses
new or changed history. Shared owners and complete legacy cutover remain separate
release gates; this does not change the experimental publication status above.

`--check-config CONFIG_JSON` validates configuration without starting agents or
opening connection state. It is a preflight check, not a migration or live-sync
readiness assertion. Invalid startup configurations preserve existing invitations.

Attachment presentation qualification: offline preparation derives bounded filename/type descriptors only from verified native evidence. Outgoing history pages count descriptors in their byte budget; source messages and file bytes are unchanged. Legacy production activation remains blocked.

Offline connection rollback now preserves and verifies retained upload bundles after child-chat rollback, while refusing changed files, orphan metadata and unfinished copies. No legacy activation gate was removed.

Coordinated offline rollback checks or applies all imported chats, settings and connection rows in one transaction. Interrupted or refused rollback preserves every chat; verified recovery files remain retained. Native path construction keeps Windows copied-file accounting consistent. The legacy activation barrier is unchanged.
