# Isolated native backend validation

Reviewed Rust backend snapshot with synthetic fixtures. Windows, Linux and macOS are checked on x64 and ARM64 runners. The folder layout matches the development source so the tested files need no path rewrites.

No Android application, user profiles, credentials, chat history or private Git history are included. Tests use temporary folders and fake agents. The small TypeScript source-owner module is retained only to test upgrading from the legacy backend.

Run checks inside `opencodex-runtime/experiments/rust-connect`. Full Rust tests, lint and release compilation run on all six platforms. The older experimental npm launcher supports only Linux x64, Windows x64 and macOS ARM64; its installation check runs on those three platforms. Other platforms are not advertised as published npm binaries.

Actions have read-only permissions and never publish to npm or deploy. Test binaries are unsigned, identified by OS, architecture and commit, and expire after seven days. Passing CI does not replace real phone testing or antivirus review.
