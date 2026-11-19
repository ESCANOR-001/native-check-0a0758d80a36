#!/usr/bin/env bash
set -euo pipefail

qa_user=remodexqa
qa_password="qa$(openssl rand -hex 16)"
qa_tmp="$(mktemp -d /private/tmp/remodex-switch-probe.XXXXXX)"
chmod 755 "$qa_tmp"
ui="$qa_tmp/ui"
echo "::add-mask::$qa_password"
sudo sysadminctl -addUser "$qa_user" -fullName 'Remodex QA User' -password "$qa_password"
qa_uid="$(id -u "$qa_user")"
sudo defaults write /Library/Preferences/com.apple.loginwindow SHOWFULLNAME -bool YES
sudo defaults write /Library/Preferences/.GlobalPreferences MultipleSessionEnabled -bool YES
defaults -currentHost write com.apple.controlcenter UserSwitcher -int 18
defaults write com.apple.controlcenter 'NSStatusItem Visible UserSwitcher' -bool YES
killall ControlCenter || true
swiftc mac-ui.swift -o "$ui"
sleep 2
osascript <<'APPLESCRIPT'
tell application "System Events" to tell process "ControlCenter"
    click (first menu bar item of menu bar 1 whose description is "User")
    delay 0.4
    click button 2 of group 1 of first window
end tell
APPLESCRIPT
sleep 2
printf 'After selecting account, console user %s\n' "$(stat -f %Su /dev/console)"
/usr/sbin/screencapture -x "$RUNNER_TEMP/macos-test-screen.png" || true
if test -f "$RUNNER_TEMP/macos-test-screen.png"; then "$ui" read-screen "$RUNNER_TEMP/macos-test-screen.png"; fi
printf '%s\n' "$qa_password" | "$ui" type
"$ui" return
for retry in {1..25}; do
    if [[ "$(stat -f %Su /dev/console)" = "$qa_user" ]] && sudo launchctl print "gui/$qa_uid" >/dev/null 2>&1; then
        echo "Actually signed in to the secondary macOS account."
        sudo launchctl asuser "$qa_uid" sudo -n -H -u "$qa_user" "$ui" logout
        break
    fi
    sleep 1
done
for retry in {1..35}; do
    if ! sudo launchctl print "gui/$qa_uid" >/dev/null 2>&1; then
        echo "Confirmed that the test account signed out and its GUI domain ended."
        break
    fi
    sleep 1
done
printf 'After sign-out attempt, console user %s\n' "$(stat -f %Su /dev/console)"
sudo /usr/sbin/screencapture -x "$RUNNER_TEMP/macos-test-after.png" || true
if test -f "$RUNNER_TEMP/macos-test-after.png"; then "$ui" read-screen "$RUNNER_TEMP/macos-test-after.png"; fi
