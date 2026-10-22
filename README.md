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
