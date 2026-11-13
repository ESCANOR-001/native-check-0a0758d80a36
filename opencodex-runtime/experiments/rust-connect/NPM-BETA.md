# Remodex Rust server — Windows beta

This is an experimental, opt-in native server for Windows x64. It includes the
Rust server, a background startup helper, and the browser dashboard. Node.js and
npm are used to install this package; the installed server does not require Bun
or a JavaScript server. The normal `latest` npm release remains separate.

For a new installation:

```powershell
npm install -g @remodex/rmx@beta
rmx version
rmx start
```

## Existing installations

The Windows reader accepts the saved format from published **1.2.27** after the
old server has stopped cleanly. Keep the same Windows account and saved-data
folder. Finish active work first, then use:

```powershell
rmx stop
npx --yes --package @remodex/rmx@2.0.0-beta.6 rmx migrate
```

Only after migration reports success:

```powershell
npm install -g @remodex/rmx@2.0.0-beta.6
rmx start
```

Preparing through `npx` leaves the old global package installed until the saved
data has passed the import checks. Original chat files, phone credentials and
recovery records are retained. Do not delete the saved-data folder or clear the
Android app's data. A running 1.2.27 server is not forcibly stopped by the reader.

**Beta 4 compatibility fix:** this build also accepts a valid older
saved-data folder without `.opencodex-owner.json`. Version 1.2.27 only creates
that uninstall record when its folder starts empty. Keep the original folder
and the same `OPENCODEX_HOME` path spelling: the old server uses that path to
identify the PC and its saved credentials. A present but damaged, linked, or
wrong-folder record still stops migration. This fix was not included in beta 3;
do not fabricate or delete an ownership record to force an upgrade.

The direct reader currently supports local Wi-Fi and existing permanent
addresses. It refuses a selected temporary internet address because 1.2.27 stops
that address's helper during shutdown. Choose a permanent address in the old
dashboard before upgrading if automatic remote reconnection is required.

Settled Remodex-managed Claude chats, their verified attachments, and saved
settings are included. Finish running work and answer pending questions before
upgrading. If a transcript or attachment cannot be verified, preparation stops
and keeps the old installation intact. Native Codex and Claude profiles are
never moved or overwritten.

Already migrated native installations use their existing saved state. Updating
the npm package alone does not replace or stop an already running server.

### Beta 6: Windows tray

After migration and starting the server, run `rmx tray`. The Remodex icon appears
beside the Windows clock (possibly under the small hidden-icons arrow).
Right-click it for status/Refresh, Open dashboard, Start server, Stop server,
Restart Remodex, and Quit tray. There is no separate controller window.

Quit tray keeps the server running. Stop server disconnects the phone until the
server is started again. `rmx tray status` checks the icon; `rmx status` checks
the server. Starting the tray again does not create a duplicate icon for that
installation. The tray needs no administrator permission of its own and opens
no inbound network port.

The tray is started explicitly; this beta does not add automatic tray startup
at Windows sign-in. Existing server background startup is independent and is
preserved by the supported upgrade. After future updates, use `rmx tray quit`
and then `rmx tray` to load the updated tray. A development-only test panel is
excluded from the published package.

### Beta 5 fixes retained

The server checks whether a phone connection has stopped responding and releases
it promptly, so reconnecting does not leave abandoned connections filling the
available slots. The current Android network-recovery update also cancels an
unfinished connection attempt when the phone changes networks.

Windows recovery records now use a single file move when first published. This
fixes an intermittent file-access failure during recovery while preserving the
checks that prevent an existing record from being overwritten.

### Windows firewall permission

The phone-facing server now runs from a persistent `windows-runtime` folder in
its saved installation, even when npm or a background-service update puts the
package in a new folder. Stop the old server before starting an updated build.
The new file is verified and installed at the same path before it starts.
Existing chats and phone credentials remain in place.

Windows may ask once when first adopting this location (including an upgrade
from beta 2 or npm/Bun). Later updates and ordinary restarts reuse that path and
its existing firewall rules. Setup checks no longer open temporary network
listeners. Remodex does not disable firewall notifications, add broad port
exceptions, or change Defender settings. A different network profile, removed
permission, or an administrator's policy can still require a permission change.

Codex Desktop updates that remove its old managed CLI file are also handled:
Remodex resolves the single replacement in that same Windows installation when
it next reads history or starts a connection. For Microsoft Store installations,
it asks Windows for the current user's registered Codex package. Custom
executable paths are never
silently replaced with a program from PATH. If an update leaves multiple
replacement builds, finish the update or select the intended file in settings.

### Older Windows background services

Starting with beta 3, Remodex can transfer a verified npm 1.2.27 WinSW 2.12.0 service
to the Rust startup helper. Stop the old service cleanly before preparing the
upgrade. Its Windows account, stored service password, automatic/manual/disabled
startup setting, and recovery settings are kept. The old service registration is
updated in place, so it does not create a second background server. A disabled
service stays disabled. A service owned by another account or saved-data folder
is not adopted.

Windows may ask for administrator approval when updating that older service or
disabling a protected old startup task. The request runs the prepared, verified
startup change only. If approval is declined, the saved upgrade can be retried
with `rmx start`; chats and pairing are retained. No security setting is disabled
and no Windows password is collected by Remodex. Existing per-user Rust startup
continues using Task Scheduler without this system-service conversion.

## Limitations

- Windows x64 only. Linux and macOS are not supported by this beta package.
- This beta is not a replacement for every older proxy or JavaScript library
  feature. Run `rmx features` for supported and retired features.
- A temporary remote address cannot be guaranteed after a computer reboot or
  the connection helper exiting. Use a configured permanent address for that.
- The executable is not publisher-signed. Microsoft has reviewed the earlier
  Defender report and said the detection was removed. This is not a guarantee
  that every future build or another PC will receive the same result. Keep
  Defender enabled and do not add exclusions for this beta.

Run `rmx --help` for commands. This package has no install scripts and does not
start a server or modify your saved chats when npm installs it.
