#!/usr/bin/env bash
set -euo pipefail

binary="$1"
qa_root="$(mktemp -d /private/tmp/remodex-login-check.XXXXXX)"
control_dir="$(mktemp -d /private/tmp/remodex-login-ui.XXXXXX)"
qa_user=remodexqa
qa_label='Remodex QA User'
runner_user="$(id -un)"
ui="$control_dir/ui"
display_file="$control_dir/screen.png"
port=27130
qa_password="qa$(openssl rand -hex 16)"
echo "::add-mask::$qa_password"

if id "$qa_user" >/dev/null 2>&1; then
    echo 'Unexpected pre-existing QA account' >&2
    exit 1
fi
sudo sysadminctl -addUser "$qa_user" -fullName "$qa_label" -password "$qa_password"
qa_uid="$(id -u "$qa_user")"
qa_home="$(dscl . -read "/Users/$qa_user" NFSHomeDirectory | cut -d' ' -f2-)"
cleanup() {
    # The test changes only the disposable VM and its separate QA user.
    if [ "$(stat -f '%Su' /dev/console)" = "$qa_user" ]; then
        sudo launchctl bootout "gui/$qa_uid" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

chmod 755 "$control_dir"
swiftc mac-ui.swift -o "$ui"
cp "$binary" "$qa_root/rmx"
cp onboard_pty.py "$qa_root/"
sudo chown -R "$qa_user:staff" "$qa_root"
sudo defaults write /Library/Preferences/com.apple.loginwindow SHOWFULLNAME -bool YES
sudo defaults write /Library/Preferences/.GlobalPreferences MultipleSessionEnabled -bool YES
# Expose this runner's ordinary Fast User Switching button.
defaults -currentHost write com.apple.controlcenter UserSwitcher -int 18
defaults write com.apple.controlcenter 'NSStatusItem Visible UserSwitcher' -bool YES
killall ControlCenter || true
sleep 3

console_user() { stat -f '%Su' /dev/console; }
has_gui() { sudo launchctl print "gui/$qa_uid" >/dev/null 2>&1; }
wait_for() {
    local reason="$1" limit="$2"
    shift 2
    local deadline=$((SECONDS + limit))
    until "$@"; do
        if (( SECONDS >= deadline )); then
            echo "Timed out waiting for $reason" >&2
            return 1
        fi
        sleep 1
    done
}
take_screen() {
    local window_pid
    window_pid="$(pgrep -u 0 -x loginwindow | tail -1 || true)"
    if [ -n "$window_pid" ]; then
        sudo launchctl bsexec "$window_pid" /usr/sbin/screencapture -x "$display_file" 2>/dev/null || true
    fi
    if [ -f "$display_file" ]; then sudo "$ui" read-screen "$display_file"; fi
}
use_switcher() {
    osascript <<'APPLESCRIPT'
tell application "System Events" to tell process "ControlCenter"
    click (first menu bar item of menu bar 1 whose description is "User")
    delay 0.4
    click button 2 of group 1 of first window
end tell
APPLESCRIPT
}
click_text() {
    local x y
    read -r x y < <(printf '%s\n' "$screen_text" | awk -F '\t' -v n="$1" 'index(tolower($3),tolower(n)){print $1,$2;exit}')
    "$ui" click "$x" "$y"
}
sign_in() {
    if [ "$(console_user)" = "$runner_user" ]; then
        use_switcher
        wait_for 'the selected test account password screen' 18 on_login_window
        # WindowServer has already switched sessions when /dev/console reports
        # the login window. Allow the secure input field to take keyboard focus.
        sleep 8
    elif on_login_window; then
        sleep 6
        local screen_text
        screen_text="$(take_screen)"
        if [[ "$screen_text" = *'Remodex QA'* || "$screen_text" = *'Remodex'* ]]; then
            click_text 'Remodex'
            sleep 2
        else
            if [[ "$screen_text" = *'Name'* ]]; then click_text 'Name'; fi
            printf '%s\n' "$qa_user" | "$ui" type
            "$ui" tab
        fi
    else
        echo "Unexpected console user $(console_user)" >&2
        return 1
    fi
    printf '%s\n' "$qa_password" | "$ui" type
    "$ui" return
    wait_for 'a new macOS desktop session for the QA account' 55 test_console_is_qa
}
on_login_window() { case "$(console_user)" in root|loginwindow) return 0 ;; *) return 1 ;; esac; }
sign_out() {
    test_console_is_qa
    # Target the disposable user's GUI login domain explicitly. The runner's
    # own separate login remains connected while the test signs back in.
    sudo launchctl bootout "gui/$qa_uid"
}
test_console_is_qa() { [ "$(console_user)" = "$qa_user" ] && has_gui; }
as_qa() { sudo launchctl asuser "$qa_uid" sudo -n -H -u "$qa_user" "$@"; }
health() {
    [[ "$(curl --noproxy '*' --silent --max-time 2 "http://127.0.0.1:$((port+5))/healthz" || true)" = *'"status":"running"'* ]]
}
app_gone() {
    ! has_gui && [ "$(console_user)" != "$qa_user" ] && ! health && ! ps -p "$original_pid" -o pid= >/dev/null 2>&1
}
# Record an OS login boundary, then start onboarding once from that login.
if has_gui; then
    echo 'The new QA user already has a GUI domain' >&2
    exit 1
fi
sign_in
echo 'macOS first sign-in created a new GUI domain for the test UID.'
as_qa /usr/bin/python3 "$qa_root/onboard_pty.py" "$qa_root" "$port" || {
    sudo tail -55 "$qa_root/onboard.log" || true
    exit 1
}
original_pid="$(sudo cat "$qa_root/onboard-result.json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["initialPid"])')"
sudo cp "$qa_root/onboard-result.json" "$control_dir/onboard-result.json"
sudo chmod 644 "$control_dir/onboard-result.json"
plist="$(sudo find "$qa_home/Library/LaunchAgents" -maxdepth 1 -name 'remodex-native-*.plist' -print)"
test -n "$plist"
test "$(sudo /usr/libexec/PlistBuddy -c 'Print :RunAtLoad' "$plist")" = true
label="$(sudo /usr/libexec/PlistBuddy -c 'Print :Label' "$plist")"
echo 'Interactive Yes registered one macOS LaunchAgent with RunAtLoad.'
health

sign_out
wait_for 'the macOS GUI domain and initial server to exit' 50 app_gone
echo "macOS signed out and the original server exited. Console user $(console_user)."

sign_in
# No onboarding or service-start call from this point. The new GUI domain
# discovers the saved LaunchAgent through the normal OS login path.
wait_for 'launchd to start the saved Remodex server' 60 health
agent="$(sudo launchctl print "gui/$qa_uid/$label")"
new_host_pid="$(printf '%s\n' "$agent" | awk '/^[[:space:]]*pid = /{ print $3; exit }')"
test -n "$new_host_pid"
ps -p "$new_host_pid" -o command= | grep -F "$qa_root/state/native-installation/login-startup/" >/dev/null
as_qa env OPENCODEX_HOME="$qa_root/state" REMODEX_GUI_DIR="$qa_root/gui" "$qa_root/rmx" status >"$control_dir/status.json"
sudo cat "$qa_root/state/native-installation/native-install.json" | python3 -c '
import json, sys
from pathlib import Path
record = json.load(sys.stdin)
initial = json.loads(Path(sys.argv[1]).read_text())
current = json.loads(Path(sys.argv[2]).read_text())
assert initial["answeredYes"]
assert current["running"] and current["pid"] != initial["initialPid"]
assert record["environment"] == initial["environmentId"]
print(json.dumps(dict(platform="macOS", answeredYes=True, signedOutAndServerStopped=True,
    enteredNewGuiDomain=True, startup="launchd at GUI sign-in", sameInstallation=True,
    newServerPid=True, healthy=True)))
' "$control_dir/onboard-result.json" "$control_dir/status.json"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    echo '- macOS passed a real sign-out and second desktop sign-in. launchd started the same installation and its health check passed.' >> "$GITHUB_STEP_SUMMARY"
fi
