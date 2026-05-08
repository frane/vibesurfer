# vibesurfer demo tape — an agent driving `vs` against
# Hacker News. The wrapper (`docs/demo/render.sh`) substitutes
# __BIN_DIR__ and __PAGE__ before vhs runs.
#
# Output: `docs/demo.gif`, referenced from the README.

Output docs/demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1080
Set Height 720
Set Padding 18
Set TypingSpeed 50ms
Set Theme "Dracula"
Env PATH "__BIN_DIR__:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
Env PS1 "> "

# Agent: poll Hacker News and surface what's hot.
Type "# Agent: what's on Hacker News right now?"
Sleep 400ms
Enter
Sleep 600ms

Type "vs status"
Sleep 500ms
Enter
Sleep 1800ms

Type "# Agent: let me read the front page"
Sleep 400ms
Enter
Sleep 600ms

Type "vs view __PAGE__ | head -30"
Sleep 600ms
Enter
Sleep 4000ms

Type "# Agent: any AI stories on the front page?"
Sleep 400ms
Enter
Sleep 600ms

Type "vs find 'AI'"
Sleep 500ms
Enter
Sleep 2500ms


Type "# every call audited in SQLite"
Sleep 400ms
Enter
Sleep 600ms

Type "vs status"
Sleep 500ms
Enter
Sleep 2400ms
