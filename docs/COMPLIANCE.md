# Compliance statement

**Org HUD is non-invasive. It does not touch Star Citizen in any way.**

This is the whole point of the HUD being open source: you can read this repository and
confirm every claim below for yourself.

## What the Org HUD does NOT do

- **No process injection.** It never attaches to, hooks, or loads code into the game process.
- **No memory reading.** It never reads or writes the game's memory.
- **No input automation.** It never sends keystrokes, mouse movements, or controller input to
  the game. It does not "play" for you.
- **No network interception.** It never reads, proxies, or modifies the game's network traffic.

## What it actually is

A separate window that draws **your org's own information** (events, status, recognition)
in a transparent, click-through overlay you can position over your screen. It gets that
information by talking to the org platform's public API — the same API any member's browser
talks to — authenticating as you via Discord, with no special access.

If a feature could not be built within these constraints, it will not be built. If you ever
find this code doing any of the above, that is a bug and a broken promise — please open an
issue.
