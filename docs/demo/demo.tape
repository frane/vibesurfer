# vibesurfer demo tape — drives the `vs` CLI through a session that
# `docs/demo/render.sh` already opened. The wrapper substitutes:
#
#   __BIN_DIR__   path to a `vs` shim
#   __SESSION__   session id (s_…)
#   __PAGE__      page id (p_…)
#
# Run via the wrapper, not directly:
#
#   docs/demo/render.sh
#
# Output: `docs/demo.gif`, referenced from the README.

Output docs/demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1000
Set Height 640
Set Padding 16
Set TypingSpeed 55ms
Set Theme "Dracula"
Env PATH "__BIN_DIR__:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
Env PS1 "> "

Type "vs status"
Sleep 600ms
Enter
Sleep 2000ms

Type "vs view __PAGE__"
Sleep 700ms
Enter
Sleep 3500ms

Type "vs find 'Example Domain'"
Sleep 600ms
Enter
Sleep 1800ms

Type "vs act 4 click"
Sleep 600ms
Enter
Sleep 3000ms

Type "vs status"
Sleep 600ms
Enter
Sleep 2400ms
