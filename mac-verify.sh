#!/usr/bin/env bash
set -euo pipefail

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
    sudo launchctl bootout "gui/$qa_uid"
    echo "Requested teardown of the isolated GUI login domain."
}
test_console_is_qa() { [ "$(console_user)" = "$qa_user" ] && has_gui; }
as_qa() { sudo launchctl asuser "$qa_uid" sudo -n -H -u "$qa_user" "$@"; }
health() {
    [[ "$(curl --noproxy '*' --silent --max-time 2 "http://127.0.0.1:$((port+5))/healthz" || true)" = *'"status":"running"'* ]]
}
app_gone() {
    ! has_gui && ! health && ! ps -p "$original_pid" -o pid= >/dev/null 2>&1
}
test_session_gone() { ! has_gui && [ "$(console_user)" != "$qa_user" ]; }

sign_in
echo "Actual macOS sign-in 1: $(console_user)"
sign_out
wait_for 'the first macOS GUI domain to finish' 40 test_session_gone || {
    printf 'Console owner after teardown: %s\n' "$(console_user)"
    has_gui && echo 'QA GUI domain remains present'
    exit 1
}
echo "Actual macOS sign-out completed. Console user $(console_user)."
sign_in
echo "Actual macOS sign-in 2: $(console_user)"
