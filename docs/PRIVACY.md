# Privacy

What the StarPlatform HUD sends, and what it stores.

## What it sends

- **To the org platform only:** your Discord login (OAuth) to identify you as a member, and
  liveness/heartbeat messages to keep the live connection open. Nothing else leaves your
  machine.
- It talks to **one** server: your org's platform. It does not phone home to any third party
  and contains no analytics or telemetry.

## What it stores locally

- Your session token (so you do not log in every launch) and your overlay preferences
  (position, hotkeys, which panels are shown), in a config file on your machine.
- Nothing about other members is persisted; live data is held in memory and discarded on exit.

## What it never collects

- No game data, no screenshots of the game, no system information beyond what the OS needs to
  draw a window.

You can delete all local state by removing the StarPlatform HUD config directory; the path is shown in
the app's About screen.
