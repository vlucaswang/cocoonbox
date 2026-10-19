# Android box

This box is an Ubuntu 24.04 VM without a desktop that runs Android 16 (720x1280) in docker. The agent's screen tools act on Android.

- adb on PATH reaches the phone (`ANDROID_SERIAL=127.0.0.1:5555`): `adb shell`, `adb install app.apk`, `adb push`/`pull`, `adb shell am start -n <package>/<activity>`, `adb shell pm list packages`, `adb shell settings`.
- `adb exec-out screencap -p > shot.png` saves a screenshot.
- Android reaches the internet only through the egress proxy: proxy-aware apps use 172.18.0.1:3128, and the TCP of other apps is redirected to it; the proxy's port rules apply, and UDP and QUIC have no route.
- Android's `/data` lives in the VM at `/var/lib/redroid-data`.

## GUI plans and scripts

- gui_plan and script_run use the desk API of `/opt/desk/lib/desk.mts`; on this box its calls act on Android.
- A window is an app's activity or dialog, titled `<title> - <package>`, such as `Settings - com.android.settings`; `{app: "<package>"}` opens or brings up an app.
- Elements are the accessibility nodes on the screen: a list row is named by its text, a row with a switch is one check box. A press scrolls an element in a scrollable list into view, also from under the navigation bar or the keyboard; any other element off the screen escalates. A list row that Android has not loaded yet is not in the tree: swipe the list first.
- Keys keep their desktop meaning (`home` and `end` move in a text field); the phone's keys are `back`, `home_screen` and `recents`.
- One accessibility client runs at a time: while shuttle's UI server runs, `uiautomator dump` fails; use computer_state or `desk snapshot`.
