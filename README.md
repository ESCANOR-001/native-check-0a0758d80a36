# Native Connect backend experiment

This is an isolated Rust foundation, **not a completed backend migration** and
not an npm release. The production entrypoint still uses the TypeScript backend.
The React dashboard and Android app are unchanged by this crate.

Implemented here:

- Closed-owner export: `--legacy-export ABSOLUTE_NEW_ROOT CLOSED_OWNER_JSON
  CONFIG_JSON` builds the profile snapshots, history manifest and preparation
  plans automatically. The TypeScript gateway's local lifecycle hook reserves
  request admission, refuses unfinished/unknown work, captures complete
  supported histories and checks native transcript hashes after its own workers
  stop. No phone-accessible export endpoint is added. A raced transcript cannot
  produce a ready generation. Partial exports remain available for diagnosis;
  use a new destination for another capture. Verified upload/tool-image
  inventories and native-UUID-bound omitted Claude attachments are retained.
  Histories beyond the current bounded export limit remain gated, never omitted.
  A durable source hold blocks the old gateway from starting new workers.
  If final validation fails after the living owner confirms its workers stopped,
  but before receipt publication, only its exact unpublished hold can be released.
  An uncertain stop, partial receipt or newer selection stays blocked; all source
  history and recovery copies remain intact.
  After confirmed worker shutdown the owner also writes a durable
  `stopped-owner.json`. If that exporter subsequently exits before publishing
  `closed-owner.json`, `--legacy-export recover-source STOPPED_OWNER_JSON` can
  release only that exact unselected source hold. It never stops a process or
  starts a service. Missing/uncertain shutdown proof, a live or reused owner PID,
  partial receipt, selected service, or changed hold stays blocked. A durable
  cancellation journal makes release retryable without deleting any history.
- Isolated service selection: `--service-cutover select SERVICE_DIR PLAN_JSON`,
  then `--service-cutover run SERVICE_DIR`. The version-1 plan contains
  `export_root`, `bind`, `public_url` and `gui`. A private exclusive journal binds
  the prepared generation and configuration. It records native startup before
  spawning the existing hidden-window supervisor; a failed launch or crash does
  not start the old backend over new history. `cancel SERVICE_DIR` is available
  only before any native startup. Cancellation revokes the unused imported
  generation before releasing the old startup hold and resumes interrupted
  release without erasing a newer selection. Source and service leases exclude
  competing preparation/selection/recovery. This is a launcher boundary, **not** OS service
  registration or replacement of the published npm entrypoint. It never stops
  or adopts another service. Codex Desktop-owned transport migration remains
  outside this path.
- Explicit Linux user-service registration: `--background-service prepare
  REGISTRATION_DIR CUTOVER_DIR` captures the current executable and an already
  selected, stopped generation. Then use `install`, `start`, `status` or
  `uninstall` with the registration directory. Install enables a unique per-user
  systemd unit without starting it; start is separate. Foreign units, overrides,
  altered enablement links and active or incompletely stopped workers are refused.
  Uninstall removes only the verified unit/link and retains all chat data. Stop
  the owned bridge through its existing dashboard before uninstalling. Registration
  retains a bounded, hash-verified private copy of the selected runtime: replacing
  or removing the npm package cannot silently replace the registered executable.
  Version-2 registrations bind that copy. Corrupt/missing copies block start;
  their exact stopped registration can still be removed without deleting data.
  Installation never overwrites an existing runtime copy or registration.
  It does not replace the ordinary bridge, enable system-wide services, or activate the
  published npm entrypoint. Windows/macOS registration remains unqualified and is
  explicitly refused; source-only CI is not a physical Desktop integration test.
- Stopped Linux service replacement: after preparing a separate registration,
  `--background-service replace NEW_REGISTRATION_DIR PREVIOUS_REGISTRATION_DIR`
  changes only their exact shared service unit. Both private runtimes must pass
  hash checks and declare supported configuration and native-storage formats.
  Unknown older runtimes remain gated. Active/foreign services and changed units
  are refused. Durable intent and atomic unit replacement support same-input
  retries; start reloads systemd's cached unit before launching. Replacement never
  starts/stops a process or restores a data snapshot. To roll back a compatible
  executable after a safe stop, reverse the registration arguments: current chat
  data and both runtime copies remain intact. This does not replace a legacy
  TypeScript service or establish compatibility with arbitrary future schemas.
- Existing-profile handoff: new closed-owner exports select `profile_mode:
  "existing"`. The original configured Codex and Claude directories remain the
  live source of agent history; captured snapshots are recovery/evidence copies,
  not replacement execution profiles. Preparation and first startup require
  unchanged source data. Later restarts verify the retained snapshot and original
  directory binding without requiring the live folder to equal an old snapshot.
  Data rollback after startup is refused; compatible executable rollback keeps
  current history. Older plans retain copy mode for isolated rehearsals.
  An idle external Claude observer may migrate only with its original profile,
  verified native transcript, shared-session identity and committed event cursor.
  Its worker remains with the original owner; migration closes only the observer
  connection and reattaches it after startup. Pending deliveries and missing
  ownership evidence remain blocked. Normal agent folders are still excluded by
  this experiment's isolation checks; this is not a shipping first-run upgrader.
- Install preparation coordinator: `--legacy-upgrade prepare|check|rollback
  ABSOLUTE_NEW_ROOT PLAN_JSON` combines verified staging, native-profile binding,
  activation and the atomic rollback check. The version-1 plan contains
  `environment`, `export_snapshot`, `native_plan` and `configuration` absolute
  paths (except `environment`, which is the retained environment ID).
  The native plan uses the existing handoff format; configuration must bind
  the same original profiles and `ROOT/native` as `native_profile_handoff`.
  Prepared state is `ROOT/bridge/state`; use its retained `ROOT/configuration.json`
  when launching a compatible candidate. A private journal binds the exact
  inputs and resumes interrupted preparation. Rollback preserves recovery
  copies and refuses to discard any generation that has started handling chats.
  This command does not export or stop a running installation, change a service,
  take over another process's worker, or publish the candidate.
- Resumable offline export staging: `--legacy-stage ABSOLUTE_EXPORT_SNAPSHOT
  ABSOLUTE_NEW_GENERATION ENVIRONMENT_ID` imports one verified snapshot into
  `GENERATION/state`. Its version-1 `stage.json` names relative JSON files for
  `connection`, `claude_settings`, `codex_projects`, optional `codex_headers`,
  and `codex`/`claude` arrays of `{id, history}`. Every imported Codex alias or
  header must have a history entry. Optional per-chat `uploads` supplies
  `{original_root, snapshot, manifest}`; both agents may supply `native`
  `{snapshot, transcript}`. Claude also supports `images`
  `{original_root, export}` and `resume`.
  Snapshot/export paths are relative inside the captured export; original
  roots are historical link metadata and are never opened. Native snapshots,
  histories and upload manifests use the existing validated importer formats.
  The generation exclusively binds that exact export, keeps the verified copy,
  resumes committed imports after interruption, and refuses altered history,
  settings or receipts. Completed stages also pass the full rollback preview.
  No service, agent or original profile is started or stopped. This remains
  offline staging: live legacy cutover and owner/profile reconciliation are
  still required, and the migration startup barrier stays enabled.
- Explicit owned-session activation: `--legacy-activate ABSOLUTE_OFFLINE_STATE_DIR
  ISOLATED_CHAT_CONFIG_JSON` accepts only a verified staged import with a matching
  prepared native-profile generation. Every Codex/Claude chat needs native
  transcript proof; linked Claude chats also need their resume receipt and Remote Control.
  Missing proof, unsupported ownership, changed profiles and mismatched
  configuration keep the import offline. Historical Codex native items bind
  to public message IDs by native/client ID, never repeated prompt text.
  Verified historical replay preserves original rows instead of duplicating
  bubbles. The startup receipt binds the environment and selected profile mode.
  Older owned Claude CLI chats without a Remote Control link can activate from
  their verified native history without inventing a shared-session ID or cursor.
  Chats that already have a shared link still require its verified resume receipt
  and Remote Control configuration.
  An unused activation can be rolled back atomically; once startup is claimed,
  data rollback refuses and retains current history. Executable rollback must
  retain the same state and native generation and use a compatible build.
  Existing-profile service replacement additionally requires the runtime's
  `existingProfileHandoffVersion: 1` declaration. A pre-support executable cannot
  be selected as a rollback target merely because its storage version matches.
  External Claude observers additionally require existing-profile mode and their
  saved shared-event cursor; copied profiles cannot adopt their ownership.
  This is isolated qualification, not an automatic production cutover or
  permission to take over Desktop-owned sessions.
- Coordinated offline rollback: `--legacy-rollback check|apply
  ABSOLUTE_OFFLINE_MIGRATION_DIR` verifies imported Claude/Codex chats, settings,
  headers, pairing and copied assets in dependency order. Check rolls back its
  transaction; Apply commits all removals together. A failed final step leaves
  every earlier chat intact. Retained copies and source receipts are never
  deleted, and the migration startup barrier remains enabled. The command
  refuses missing, normal and already-running profiles. Windows retained-file
  checks use the same native path separators as the storage ledger.
- Offline connection rollback recognizes verified retained upload bundles after
  each imported chat has been rolled back. Copies and their quota accounting
  remain intact; altered files, unrelated receipts, unfinished copies and
  unclassified storage rows block the rollback without clearing pairing.

- Historical uploads omitted by an older public transcript have metadata-only
  display descriptors after offline native-evidence verification. Outgoing
  snapshots and history pages show the original filenames and image/file type,
  with descriptor bytes included in the page budget. Display never reads upload
  bytes or native transcripts, and source messages stay unchanged. Existing
  links/attachments are not duplicated; rollback removes display eligibility
  without deleting retained copies. This remains staged behind the legacy
  startup barrier until coordinated cutover is qualified.
- Candidate configuration preflight: `--check-config ISOLATED_CHAT_CONFIG_JSON`
  validates bounded configuration without opening state, changing pairing or
  launching agents. Normal startup performs the same validation before claiming
  the state directory. Installers can use this before stopping a working bridge.
  Success checks configuration compatibility, not credentials, native-generation
  ownership, full legacy migration, or a successful live connection.
- Explicit native-profile generations for an isolated, owned Codex/Claude
  connection. A coordinator first stops admission and its CLI workers, exports
  both profiles, then prepares private writable copies. Codex rollout indexes
  are relocated only after matching each native session header; other rows,
  timestamps and unknown exported files are preserved. Immutable exports and
  checkpoints remain available. Interrupted attempts are retained and retry
  uses a fresh numbered copy with retry, disk and memory bounds.
  `--native-handoff capture SOURCE SNAPSHOT ENVIRONMENT_ID`,
  `--native-handoff prepare GENERATION PLAN_JSON` and
  `--native-handoff rollback GENERATION ENVIRONMENT_ID` are local experimental
  maintenance operations, never HTTP or phone actions. The version-1 plan binds
  `environment` and `codex`/`claude` inputs, each with an explicit
  `original_profile` and verified `snapshot`. The normal chat configuration
  retains those original profile paths and opts in with
  `native_profile_handoff: "/absolute/isolated/generation"`.
  Startup verifies the source exports have not changed, takes an exclusive
  generation lease before probes/workers, and durably binds the connection to
  that generation. Restart keeps its new history. Missing/stale configuration
  fails instead of falling back to original profiles. Data rollback is allowed
  only while both copies match their initial checkpoints; after new work it
  refuses and preserves every copy. Executable downgrade compatibility still
  needs the release coordinator; deleting the configuration field is not a
  rollback. Shared Codex servers and externally owned Claude sessions require
  their original owner's coordination and are refused here. This does not
  remove the legacy-import startup barrier or complete production upgrades.
- Optional serving of an explicitly selected existing React build. Local browser
  sessions use the existing five-minute bootstrap/header/CSRF contract. Host,
  Origin and the actual loopback socket must agree; phone tokens never authorize
  dashboard administration. Sessions and asset reads are bounded. Static files
  cannot escape the selected build or expose arbitrary JSON/source maps.
- Dashboard phone status, live socket presence, pairing and device removal.
  The saved control switch suspends phone access without erasing paired phones
  or chats. Disabled phone endpoints return temporary unavailability, not a 401
  that would cause Android to discard its connection. Re-enabling can reconnect
  with the same credential. Browser administration remains available while off.
  Targeted repair codes retain the old credential until the phone exchanges
  the code successfully; replacement and revocation are one transaction.
  Expired records remain visible for repair. Other paired devices and all
  history are preserved, including when the database rejects a replacement.
  LAN access can be disabled without stopping agents or deleting phone credentials.
  Existing remote streams close and new remote sockets are rejected before HTTP
  parsing; the bound local administration listener stays available. The saved
  setting survives restart. The awake switch retains the original stored-preference
  behavior; it does not install an OS sleep inhibitor.
- The native supervisor supports local dashboard drain-and-restart. It stops
  admitting new prompts, Steer, project changes and storage maintenance while
  existing tasks and their already accepted queue finish. History, approvals,
  answers and Stop stay usable during the 60-second drain. At the deadline,
  normal durable shutdown interrupts only owned work and detaches external
  observers; leftover queued messages remain paused across restart. Stop and OS
  shutdown override replacement. The supervisor owns only its explicit child,
  never locates or restarts a normal Desktop/bridge process, and bounds repeated
  explicit restarts. Agent/service installation remains a separate release gate.
- Quick tunnels use a dedicated phone-only listener: the local dashboard and
  administrator APIs are never forwarded. A printed address is not enough;
  the public endpoint must report this bridge's exact environment identity.
  Paired Android clients learn verified addresses through the existing config
  subscription. An explicit retry gives existing sockets a short interval to
  save available LAN routes before the old helper exits. Slow initial DNS
  verification keeps the same registered connector instead of rotating addresses.
  Temporary quick-tunnel addresses cannot guarantee remote-only recovery after
  their helper exits; stable named tunnels remain a release gate.
- Enabling a quick tunnel lazily installs the pinned cloudflared helper inside
  this isolated profile. Downloads stream to disk with a size cap and SHA-256
  verification; macOS archives admit only the regular helper executable.
  Cached binaries are rechecked, native version checks have a timeout, and
  Windows helpers launch without a command window. Interrupted downloads never
  publish an executable; unknown or modified cache content is preserved and
  reported as unavailable. `REMODEX_NATIVE_CLOUDFLARED` explicitly selects an
  existing executable instead. The normal bridge's cache, credentials and
  processes are not modified. Named tunnels, first-run agent bootstrap and
  service installation remain incomplete.
- Claude setup status and future-session automatic connection controls use only
  the configured test profile. The native sign-in probe is bounded, cached and
  excludes account identity fields. Preference writes preserve unrelated settings,
  respect explicit disabling policy, and replace files atomically. Existing sessions
  stay connected. Desktop's own setting is reported as unknown, never guessed.
  Bridge-side enable/sharing switches persist separately. Changes wait for idle
  Claude tasks and controls, save before detaching, and retain all history/identities.
  Re-enabling sharing reconnects saved hosts without replaying input. Disabled
  agents refuse new work, including after restart. Codex workers stay independent.
  Executable selection persists with atomic model-cache invalidation. Empty input
  restores the explicitly configured default; a rejected save does not disconnect
  workers. Windows accepts native `.exe` files without shell invocation.
  First-user installation/authentication bootstrap remains unsupported.
- The dashboard activity/usage API incrementally reads the selected isolated
  Codex profile, with a two-second cache and bounded files, records and read work.
  Only native turn state and measured token counters are retained; prompts and
  tool content are excluded. Copied sessions and inherited fork usage are not
  double-counted. Partial scans are labeled. This preserves the current dashboard's
  Codex-only activity contract; it does not claim Claude usage in that surface.
- SQLite-backed paired-client identity and token digests, with transactional
  replacement and revocation.
- One-use pairing codes and WebSocket tickets, bounded retry recovery, request
  bodies, connection counts and queues.
- A dedicated database thread so database contention does not block networking.
- Android-compatible discovery, token exchange and session/ticket HTTP shapes.
- Direct JSON-lines transports for Codex CLI app-server and Claude Code CLI.
  These transports do not use a JavaScript backend, the Claude SDK or a base64
  relay.
- An opt-in shared Codex app-server transport over an authenticated loopback
  WebSocket or a private Unix socket. Closing a client does not kill the server;
  cancellation targets only the verified turn owned by that client. Foreign
  thread/turn notifications cannot approve tools or complete another chat.
- Open shared Codex chats catch up Desktop-origin messages through bounded
  native history pages. Page receipts and cursors commit together. Idle readers
  expire when the phone leaves, and sending waits for history catch-up before
  creating another turn. A running Desktop task can be joined without changing
  its model, permissions or workspace. Queue, Steer, explicit Stop and pending
  approvals use its verified native turn identity; leaving the chat or closing
  this observer does not cancel that task. A queued message can only steer the
  task it was queued during, so a delayed tap cannot change its successor.
  No prompt is replayed after a connection failure.
- Opt-in `claude.remote_control` connects the chat manager to a persistent
  Claude Remote Control host. It keeps its owned CLI alive between results,
  verifies reattached native/shared identities, and gates prompt delivery on
  a durable identity receipt. Bounded native consumption receipts distinguish
  pending Android input from older Desktop results. Queue, native Steer and
  Stop reuse the connection; a native cancellation before turn start finishes
  without waiting for a result that will never arrive. Internal metadata input
  is not exposed as user text; only the exact native peer-message envelope is
  unwrapped into a visible remote prompt. Input identities are tracked
  separately, including native start receipts that precede their user echo.
  An idle disconnect records a sync error without rewriting the finished turn.
  Idle connections do not
  occupy an active-turn slot. This experiment allows at most eight connected
  Claude hosts. Saved owned hosts reattach automatically after worker exit or
  bridge restart, with at most two concurrent handshakes and bounded retry
  backoff. Reattachment verifies both native and shared identities before
  delivering remote events, never resends saved prompts, and preserves finished
  turns and paused queues when a handshake fails.
- Existing shared Claude sessions can attach through a separate native HTTP/SSE
  observer without launching or terminating their owner. A fresh worker handshake
  and local history binding verify the native/shared IDs and workspace. Projection,
  stream cursors and local input receipts commit atomically. Stream reads continue
  during bounded command delivery; reconnect never resends a prompt. Native read
  receipt support gates server processing confirmations, including mid-task Steer
  messages absent from ordinary result UUIDs. Bounded server receipt pages recover
  lost delivery updates after reconnect. Old receipts cannot finish a newer turn.
  Periodic worker checks distinguish a live SSE socket from an available owner.
  Truncated or missing history ranges recover in ascending bounded pages with a
  fixed upper boundary, atomic checkpoints and a send barrier until recovery
  finishes. Large pages shrink automatically; a missing/reordered event fails
  closed with existing messages intact. Stop uses the matching acknowledgement,
  terminal result and input-processing proof; native interruption notices cannot
  become extra prompts or occupy worker slots. Unknown platform labels do not
  hide explicitly human-origin messages. Read-only tool catalogs and context usage
  use the verified original worker while idle or running; disconnected observers
  never launch a fallback CLI. Read timeouts release bounded waiters without
  stopping the task, and stale replies cannot complete a newer read. When Remote
  Control omits its MCP status list, tool names come from its native context report;
  loaded schemas never imply a connected server. Idle tool controls target the
  existing worker. Reservations are saved before sending, and acknowledgements
  commit with history/cursors; disconnects never repeat a control. Unsupported
  native controls return errors without failing the chat or changing settings.
  A prompt that selects another model or effort first validates both against
  the CLI catalog (including the wire ID an alias resolves to), then applies
  `set_model` and `apply_flag_settings` through the original worker, one durable
  step at a time. The prompt is sent only after every matching acknowledgement.
  Rejections keep earlier settings; a confirmed model with a rejected effort is
  saved and reported; a still-pending model switch, timeout or disconnect is
  uncertain. In each case nothing is sent and no control or prompt is replayed.
  Queued prompts apply their own settings only when they reach the head of an
  idle queue; Steer never changes settings. Each connection records the
  original worker's process ID after catch-up. If another process now owns the
  session, a turn the previous process left without a result ends as
  interrupted (saved output kept); its inputs become terminal and are never
  resent. If the relay remains connected across a worker restart, an idle
  health report for a quiet active turn triggers a bounded identity check.
  Only a different worker PID interrupts the orphaned turn. Checks continue
  after earlier healthy/replacement replies; lost metadata replies expire
  without replaying a prompt or mutating control. Historical shutdown notices
  before the fresh handshake do not prevent catch-up to the current worker;
  a shutdown after that handshake immediately revokes control. If the local
  transcript records a changed relay for the same settled native chat and folder,
  the observer saves the old cursor and follows the new link transactionally.
  Existing history and receipts stay intact, queues stay paused, and catch-up
  cannot resurrect resolved approvals. Unresolved deliveries or active work,
  changed folders, revoked links and previously retired links fail closed;
  creating a replacement native session is not implemented by this recovery.
  A reconnect to the same
  process leaves the turn running. "Steer
  instead" on an idle shared or hosted chat sends that queued message now and
  resumes the rest, as the TypeScript bridge does. Desktop-owned chats return
  the same checkpoint, branch and restore outcomes as the TypeScript bridge and
  can start a new isolated worktree chat that never inherits the shared
  identity. Actual Desktop UI qualification remains a release gate.
- Opt-in native chat orchestration: create/send/resume, streamed replies and basic
  tool details, public thinking summaries, authoritative diffs, questions, approvals, Stop, queue editing/cancellation/reordering and native
  Steer. Android HTTP creation and WebSocket subscriptions use the same controller.
- Claude foreground results wait for reported background tasks before completing
  the turn or releasing queued work. Task metadata survives observer cursor
  reloads; late notifications for an older run cannot finish a resumed tool.
  A new consumed steer requires its own completion. Individual background-tool
  Stop controls are still pending qualification.
- Codex repository cloning uses a bounded asynchronous Git job and a durable
  destination receipt. Ambiguous failures are never retried automatically;
  completed retries preserve local edits. The existing folder picker
  placeholder model resolves to the selected agent's actual default on project
  creation only. Explicit model edits remain validated.
- SQLite command receipts committed with chat state, so reconnect/restart does
  not automatically repeat a prompt. Interrupted queues stay paused.
- Atomic new-chat + first-prompt requests for registered projects and No Project.
  Validation failure leaves no empty chat; retries and restart use the original
  command receipt. Worktree bootstrap prepares a separate checkout
  and the configured setup script before accepting the first prompt. Its durable
  reservation recovers a completed checkout after a failed chat commit without
  rerunning setup. Explicit origin fetch updates only the selected remote-tracking
  branch, has a one-minute timeout and never opens an interactive login prompt.
  Retrying a saved preparation uses the pinned commit instead of fetching again.
  Existing registered worktrees (including detached checkouts) can also start
  chats without changing their branch, staged files or local changes.
- Separate recent/archived chat snapshots with durable archive/unarchive commands.
  Restoring a chat retains its original conversation and native session identity.
- Prompt edits prepare a separate native history before committing a replacement.
  The original conversation is retained as a private revision, earlier prompts stay
  visible, and edited/later turns are excluded from the resumed CLI context.
  Codex forks before the verified native turn; Claude forks through a verified
  preceding assistant response, allowing linked CLI prompt-snapshot bookkeeping.
  Invalid attachments are rejected before native preparation. Missing or ambiguous
  native history leaves the original unchanged. Revision restore UI is pending.
- Native MCP input forms for both agents, with explicit typed answers and bounded
  string, number, boolean and enum-list validation. Unsupported schemas and browser
  or identity steps are canceled with a notice, never implicitly accepted. Unknown
  CLI control requests receive protocol errors; repeated unsupported requests stop
  the owned turn. Codex clock requests receive the current timestamp.
- Startup CLI model discovery without creating a chat, named resolved defaults,
  validated model/effort/access/plan settings, and durable plan implementation links.
- Project and No Project creation, validated per-agent project model defaults and
  bounded saved script settings (saving does not execute them), confined file/folder search, text read/write,
  read-only Git status/ref/diff inspection with bounded output and time limits.
- Tool image capture with chat-scoped, lazy thumbnail/full-image capabilities.
  Atomic, synced writes preserve the previous file on failure. Replaying the
  same authoritative native image repairs a damaged stored copy without adding
  duplicate history. Original bytes remain on disk; decoded previews are not cached.
- Provider-reported context and quota readings, with bounded metadata probes,
  caching and explicit unavailable readings rather than fabricated zero values.
- Claude tools/skills/plugin catalogs with coalesced active-session reads and
  short-lived metadata-only checks for idle chats. No model prompt is sent by
  these checks. Explicit isolated MCP/plugin paths are supported.
- Idle Claude MCP enable/disable/reconnect and skill/plugin reload controls.
  Per-chat enablement and command results persist transactionally; disabled
  servers are disabled before the next model prompt. Recovered owned hosts restore
  that policy before enabling Remote Control. Running tasks and queued messages
  prevent tool changes. No normal Desktop settings are modified.
- Storage diagnostics use only the configured Codex profile, with bounded
  traversal, a single cached blocking scan and at most four queued readers.
  Linked/reparse entries are skipped; row counts include committed WAL data
  without changing source files. Unknown counts and incomplete scans are explicit.
- Local authenticated dashboard cleanup previews, quarantine, exact restore and
  resumable permanent deletion use a durable file journal and bounded row backups.
  Active tasks, queued work and referenced history prevent cleanup. Restore
  preserves timestamps, attachments, review metadata and read receipts, and
  refuses newer conflicting files/rows. Recognized timestamp triggers are disabled
  only on the restoration connection; other CLI connections are unchanged.
  Complete quarantines made by older JavaScript versions are adopted into the
  durable journal before restoring exact schema-matched rows and files. Serialized
  database paths are never opened. Unknown precision/types, incomplete older
  restore records and schema conflicts are retained with an explicit error rather
  than guessed. Interrupted native adoption resumes without overwriting data.
- Cleanup policies are off by default and stored in the isolated bridge profile.
  Manual/startup/daily/weekly runs share one worker and reserve idle chat admission.
  Byte targets select exact archive sets; active agents defer cleanup. Concurrent
  settings edits survive completion, and scheduled due windows are saved before
  deletion. There is no implicit cleanup of normal profiles or user workspaces.
- Explicit setup runs use the first project script marked for worktree creation,
  with the same project/worktree environment variables as the existing runtime.
  Setup runs in the new checkout with a two-minute limit and 64 KiB per output
  stream. Commands and completion are displayed; raw output is not stored.
  Failed or interrupted setup preserves the checkout and will not replay on retry.
  Persistent interactive terminals and background preview servers are not migrated.
- Phone-local question draft updates are accepted without submitting any answers,
  refreshing the CLI, or replacing pending request IDs.
- Claude isolated worktree chats, using the committed HEAD and a durable Git
  operation reservation. Retrying after a lost response reuses the same checkout
  and chat. Source files and index remain untouched. The configured test workspace
  must provide room outside the source repository; nested checkouts are rejected.
- Claude conversation branching with a pinned native transcript point and a
  transactional, paged history copy. First use forks through the CLI with a new
  identity; later source prompts are excluded. File checkpoints start fresh.
- Claude file checkpoints and preview/restore controls. Previews are scoped to
  the chat and phone, expire after sixty seconds, and verify affected file contents
  before restoring. Only two preview/control workers are retained; new prompts
  invalidate old previews. Restore results have durable duplicate protection,
  and neither conversation history nor untracked tool-external changes are erased.
- Configurable active CLI budget (four by default, one to eight supported),
  independent controls per chat, and fair event polling. Queued handoff waits for process cleanup.
  Unix process-group cleanup includes runtime cancellation. Bounded Git/setup
  helpers also clean up descendants on timeout, cancellation and successful exit; Windows uses a
  kill-on-close Job Object with hidden child windows.
- Durable row-based transcript storage with stable older-page cursors. Streaming
  updates restore older tool/message rows by ID; legacy experimental blobs migrate
  transactionally. A live window holds up to 24 rows / 160 KiB, with one larger
  individual row allowed up to 384 KiB. Earlier history stays on disk.
  Idle chats retain only metadata; opening a chat loads its recent page on demand.
  End-of-turn cleanup also resolves pending controls outside that recent page.
- Private image/file uploads using the Android command format. Queue state keeps
  paths and metadata, not base64. Codex receives local-image input; Claude receives
  native image content blocks. Other files are supplied as explicit local paths.
- Revocable, expiring image capabilities, streamed original downloads (64 KiB
  chunks), and bounded 256 px thumbnails. A transfer permit stays with the
  response until the download finishes or is cancelled. Four bounded original
  streams can overlap; thumbnail decoding remains serialized with cancellation-safe
  ownership and a bounded admission queue. Conversion checks dimensions and decoded
  size and retains no decoded cache. Truncated shared Codex tool images recover
  from a verified, bounded native rollout reader with durable scan cursors; only
  exact matching images are copied into private chat-scoped storage. Android
  gallery entries remain lazy and do not expose raw native history.
- Text file links use the verified chat workspace, and read-only project browsing
  recognizes registered projects and same-agent chat folders outside the default
  root. Caller-supplied paths never become authorization roots. Text reads are
  bounded, reject binary/special files and keep independent write boundaries.
- Configurable private-storage admission limits for history, uploads and tool images.
  Existing data is never evicted to make room. Full storage pauses queues and stops
  owned workers with a saved error; a 64 MiB SQLite reserve permits final status
  and cancellation receipts. Upload accounting is rebuilt from disk after restart.
  An OS profile lock prevents a second server from bypassing those limits.
- Incremental cleanup of unreferenced staged uploads and tool images after a
  24-hour grace period. All stored history and queued attachments are retained;
  cleanup never targets user workspaces or CLI profiles.
- Codex exposes prompt editing only for its latest regular prompt, with no
  Revert action. The backend checks both the complete stored history and the
  current native turn before preparing an edit, and checks the native head again
  before committing it. Older prompts and steering messages are refused without
  replacing history. Edits keep a durable prior revision. The opt-in Android
  policy affects only this candidate's Codex connection; Claude retains its
  separate edit, branch and file-checkpoint behavior.

Without an orchestration config, chat RPCs still fail explicitly. With one, only
the implemented methods are enabled; unsupported features never fabricate success.
The offline connection-migration foundation preserves original phone IDs,
credential hashes and expiry, permission scope, environment identity and an exact
private copy of the legacy connection ledger. Preparation and rollback are
transactional; changed destinations and unsupported named-tunnel credentials are
refused. A prepared destination cannot start the server or agent workers until
history and asset migration is complete. This component is currently a library
with fixture tests, not an exposed upgrade command; normal installations are
never discovered or migrated automatically.

`/healthz` continues to report `migrationComplete: false`. This is not yet a
replacement for the normal bridge.

The orchestration slice has a 10,000-chat metadata budget, eight queued prompts
per chat (64 across the PC), a configurable active CLI budget, and a 32 KB prompt limit. Transcript size no longer prevents further turns; individual display items
still have a 384 KiB limit. Uploads allow up to eight files, 10 MiB per file and
40 MiB combined, with one transfer at a time. Thumbnails reject images above
16 megapixels or 64 MiB decoded pixels. File restore previews are limited to
100 files, 16 MiB per file and 128 MiB combined; hashing uses a 64 KiB buffer.
Default storage budgets are 1 GiB of live database pages, 2 GiB of copied assets,
65,536 asset files, and a 256 MiB free-space floor. These cover the private bridge
store, not files created in projects or the separate agent CLI profiles. Existing
history remains readable at capacity; no automatic history deletion is performed.
`server.getStorageStatus` reports measured usage and configured limits. The
remaining advanced controls and production settings UI still need migration.

## Validation

Run sequentially to keep memory use low:

```sh
cargo test -j 1
cargo clippy -j 1 --all-targets -- -D warnings
cargo build -j 1 --release --bins --examples
```

Tests cover token expiry, pairing retry identity, database rollback, durable
authorization, re-pairing/revocation, live WebSocket revocation, SQLite contention,
large Unicode protocol messages, oversized/truncated frames and owned process
cleanup. Orchestration fixtures cover both protocols, durable duplicate protection,
resumption, queue/Steer/Stop, approvals, CLI crashes and the Android HTTP creation
path, including rejected steering, approval cancellation and oversized output.
Additional tests cover 5,000-row history paging, updates across page boundaries,
legacy history migration, attachment queue/restart/duplicate receipts, malformed
uploads, preview dimensions and revocation. Worktree tests preserve dirty source
files and recover a checkout created before its chat receipt. Restore tests cover
changed files, invalidated/foreign previews, restart receipts and retained history.
Edit tests cover middle-turn replacement, history revision transaction rollback,
malformed attachments before CLI forks, stale native points, prompt snapshots,
repeated commands and restart. Bootstrap checks cover both agents, failed first
prompts, capacity, No Project and durable duplicate suppression. Project settings
checks cover agent isolation, explicit overrides, clearing, restart, invalid scripts
and persisting scripts during creation. Setup/worktree bootstrap checks cover
first-prompt validation, both agents, unchanged dirty source files, configuration
selection/environment, failed setup, interrupted descendants and recovering
completed setup when the first chat transaction fails.
Origin checks use a local Git remote with a newer commit, preserve the source
index/worktree, ignore unrelated fetch mappings and never silently fall back to a
stale local branch. Existing-worktree checks reject foreign/outside folders and
stale branch selections, retain staged/untracked files and never recreate a missing
checkout. Archive checks verify agent separation and complete retained history.
Shared-server checks cover client ownership, early notifications, bounded frames,
read-only paged catch-up, streaming updates, cursor transaction rollback, restart,
duplicate suppression and expired subscriptions. Fixtures alone do not establish
Desktop synchronization or platform parity.
New sends require a history check started for that exact pending request; an
older background read cannot authorize sending or replacing an empty session.
Short Unix socket aliases are verified against Codex's fixed, canonical `/tmp`
rendezvous directory, including macOS's `/private/tmp` spelling. A different
`TMPDIR` does not change that root or weaken the owner, permissions or path-hash
checks.
Worktree paths keep their canonical identity in storage and confinement checks;
only Windows Git arguments use ordinary drive/UNC spelling, avoiding Git's
rejection of verbatim paths when creating the checkout's `.git` file.
Claude host fixtures also cover automatic worker/restart reattachment with no
prompt replay, failed handshakes preserving completed turns, interrupted queues
remaining paused, reconnect opt-out, and remote input arriving during the
identity handshake. Exact native peer envelopes produce one visible prompt;
unknown metadata and tool/subagent input remain hidden.

`examples/history-memory.rs` compares a whole-blob history load with loading the
newest page of the same synthetic history. This isolates history-loading cost;
it does not measure the production bridge or provider CLI memory. The remaining
messages stay on disk and are available through older-page requests.

`examples/probe.rs` initializes a real CLI with an explicitly supplied isolated
profile. It sends no model prompt. Never point it at a profile used by a running
normal bridge or desktop application.

## Isolated launch

```sh
target/release/remodex-connect-native /absolute/private/test-state 127.0.0.1:18960 http://127.0.0.1:18960
```

The process writes an expiring pairing invitation to a private file in that
directory and prints the file path, not its token. It does not install a service,
read the normal Remodex profile or replace an existing bridge. Add an absolute
JSON config path as the fourth argument to enable native chat orchestration:

```json
{
  "max_active_chats": 4,
  "storage": {"history_bytes": 1073741824, "asset_bytes": 2147483648, "asset_files": 65536, "free_bytes": 268435456},
  "workspace": "/absolute/disposable/workspace",
  "codex": {"executable": "/absolute/codex", "profile": "/absolute/test-codex", "model": "your-codex-model"},
  "claude": {"executable": "/absolute/claude", "profile": "/absolute/test-claude", "model": "your-claude-model"}
}
```

Profiles must be isolated and already authenticated. Normal Desktop integration
is not enabled by this test path. Remote Control is off by default; setting
`claude.remote_control` to `true` enables it only for the configured isolated
Claude profile and its owned test chats. Codex account
plugins/apps and Claude hooks are disabled. Claude MCP servers are excluded by
default; `claude.tools` may explicitly select `mcp_config` and `plugin_dirs`
inside the isolated workspace or profile. Those test workers never inherit
normal Desktop or implicit project tool configuration.
Use a disposable
directory. Authentication and network hardening still require a production review.

## Remaining before replacement

1. Complete remaining advanced controls for Codex and Claude according to each
   agent's behavior. Standard, local and origin-fetch worktree first-prompt
   bootstrap, existing-worktree bootstrap, bounded setup and verified prompt
   editing are implemented. Codex has no Revert action and only its latest
   regular prompt is editable. Claude file checkpoint preview/restore remains a
   separate supported operation. The bounded new/resumed-chat slice above is
   implemented, not full parity.
2. Complete shared Desktop-session qualification for both agents, including
   recovery and controls on Desktop-owned turns. Codex shared transport and
   read-only catch-up are implemented; a CLI handshake alone does not validate
   Desktop synchronization. Claude Remote Control is opt-in for the isolated
   profile; shared controls still depend on support in the original worker.
3. Production project discovery/registration, richer asset types and Git/worktree mutations.
4. Finish first-run setup, stable named tunnels and service lifecycle. Dashboard
   settings, phone management, diagnostics, storage cleanup/restore/policy and
   quick tunnels are implemented; their qualification records are separate from
   the remaining release gates.
5. Compatibility migration/rollback for existing installations and pairing data.
6. npm distribution and native platform packages, installation, upgrades,
   Windows/macOS live agent tests as well as Linux. Automated protocol, lifecycle
   and offline npm checks run on all three systems; they do not replace live
   Desktop/phone qualification on each platform.
7. End-to-end parity and full-workload memory/performance validation.

The repository's separate provider-proxy/routing subsystem is not ported here.
It must not be counted as migrated.

## Transport measurement

`benchmark.mjs` and `examples/benchmark.rs` exchange the same 32 KiB JSON frames
with the same native echo fixture. `measurements.json` records three fresh-process
samples at each size on Linux. Parent high-water RSS excludes the identical
fixture in both cases. This is a small transport comparison, **not** a claim about
total bridge RAM, CLI memory, model response time or full migration savings.

The older `../rust-backend/` experiment keeps the TypeScript backend and places a
Rust supervisor underneath it. Its measurements include a different workload and
an extra relay; do not compare those numbers as equivalent full applications.
