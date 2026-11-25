# Kosmik Layouts

**Your OBS scene rearranges itself for each game you play.**

Keep one live scene in OBS. When you open a game, Kosmik Layouts moves your camera, chat, alerts and the rest to where you placed them for that game, and points your game capture at it. Close the game and it goes back to your "just chatting" layout. No scene switching, no hotkeys, and no copying the same scene for every game.

![The Kosmik Layouts control page](docs/control-page.png)

- **You arrange things in OBS, as usual.** The app simply remembers each arrangement as a layout.
- **Each game can capture its own way.** One game on Game Capture, another on Window Capture: the right one turns on and points at the right game.
- **Background games don't get in the way.** If an idle game is open, the game you started last wins, and alt-tabbing changes nothing.
- **Smooth moves.** Sources slide into place (or snap, if you prefer).
- **Manual control when you want it.** Show a layout once, or lock one on screen whatever you're playing, from the page, the tray icon or a hotkey.
- **Safe.** It never creates, deletes or renames anything in OBS.

The page is in Spanish or English and comes in three looks (Studio, Daylight, Pixel), shared with [Kosmik Voice](https://github.com/kosmik-code/kosmik-voice).

## What you need

- Windows 10 or 11.
- OBS 28 or newer.

## Install

1. Download **`Kosmik-Layouts_…_x64-setup.exe`** from the [latest release](https://github.com/kosmik-code/kosmik-layouts/releases/latest) and run it. No admin rights needed.
2. Windows may say *"Windows protected your PC"*, because the installer isn't signed yet. Press **More info → Run anyway**.
3. The control page opens in your browser. Kosmik Layouts now lives in the tray, next to the clock: click its icon to open the page again, right-click it to see what's on screen, pick a layout, quit, or stop it starting with Windows.

It's tiny: about a 2 MB download, and a few MB of memory while it waits in the tray. Prefer running it from the source code with Node.js? See [Run from source](#run-from-source).

## Set it up

1. **Let it talk to OBS.** In OBS, open **Tools › WebSocket Server Settings** and tick **Enable WebSocket server**. That's all: the app finds the port and password by itself. The pill at the top turns green when it's connected; if it doesn't, the page tells you why.
2. **Your no-game layout.** Arrange your scene in OBS for when you're not playing. On the page, under **Save the current scene**, give it a name (say, *Just Chatting*), tick **Use it when no game is open** and press **Save from OBS**.
3. **A layout per game.** Open the game and arrange the scene for it. If you use a Game Capture or Window Capture, point it at the game. Press **Pick from open apps** to choose the game, then **Save from OBS**.

Now close the game and open it again: the scene follows. The **First steps** button at the top shows these steps whenever you want them.

## Good to know

- **Changing a layout:** arrange it again in OBS and press **Update from OBS** on that layout.
- **Leave a source alone:** open *Sources it controls* on a layout and untick it (your alerts, say), and that layout won't touch it.
- **Renamed a source in OBS?** Layouts find sources by name, so press **Update from OBS** on the layouts that use it.
- **Anything you move by hand stays** until the next game opens or closes.
- **Hotkeys:** **Ctrl+Alt+Shift+L** locks the layout on screen (press again to go back to automatic), **Ctrl+Alt+Shift+N** steps through your layouts and then back to automatic. Change them (or turn them off) in the **Hotkeys** card on the page; if another program already uses a combination, the card says so. A Stream Deck's *Hotkey* action can press them for you. The tray icon's *Choose layout* menu does the same with the mouse.
- **Kosmik Voice too:** set its overlay's *Placement* to **Free box** and it becomes a normal source you can drag and resize, so each layout can put your voice overlay somewhere else.
- **Which scene it controls:** whichever one is live, unless you pick one under **OBS connection › Behaviour**. There you can also set how long the slide animation lasts (0 = instant).
- **Two-PC setups:** in **OBS connection**, choose *Manual* and type the streaming PC's address and the password from OBS's *Show Connect Info*. Run Kosmik Layouts on the gaming PC, since that's where it looks for games.
- **It starts with Windows** and waits in the tray, so your layouts follow your games as soon as OBS is open. Turn that off from the tray icon.
- **Updates:** when there's a new version, the tray menu shows *Install update* and the page shows a banner with **Install now**. One click installs it and Kosmik Layouts restarts by itself; your layouts are kept.
- **Your layouts** live in `%APPDATA%\Kosmik Layouts`.

## Status

This is an early version that does one thing well. There's no drag-and-drop editor yet: you arrange things in OBS itself. Ideas for later: dragging sources on the live picture, Twitch category as a trigger, vertical canvases (Aitum Vertical), and nested scenes and groups.

---

## Technical details

### How it works

- **Layouts are snapshots.** Saving stores, for every source in the scene, its position, size, rotation, crop, bounds and whether it's visible. A layout only touches the sources it lists; unticked ones are skipped, and sources that are no longer in the scene are reported in *Activity*.
- **Capture targets.** For Game Capture, Window Capture and Application Audio Capture sources, a layout also stores what they capture: the mode (a specific window or any fullscreen app) and the window. Targets are matched by program (`window_priority` = executable), not just by window title or class, so another game built on the same engine (two Unity games, say) is never grabbed by mistake. A capture is only re-pointed when its target really differs and only if that layout shows it, because Window Capture restarts on every settings change. Captures are re-pointed before anything slides in, so a source never appears showing the previous game.
- **Game detection.** Every second the app reads the Windows process list (about a millisecond of work; the Node.js version checks every 3 seconds). If several programs with a layout are running, the one started last wins; programs already open when the app starts count as equally old, and then the layout listed first wins. No game means the default layout.
- **Switching.** It only acts when the wanted layout changes: a game starts or closes, you lock or unlock, or OBS reconnects. Moves are animated (400 ms by default) and a newer switch cancels an older animation.
- **OBS restarts.** While OBS is starting it answers "not ready"; the app waits and retries instead of failing, and re-applies once OBS is back. The app also notices OBS's program starting and connects right away instead of waiting for its next retry.

### Connecting to OBS

- **Automatic** (default): the WebSocket port and password are read from OBS's own settings on this PC (`%APPDATA%\obs-studio\plugin_config\obs-websocket\config.json`), and re-read every few seconds, so a changed port or password (or the server just being switched on) is picked up by itself.
- **Manual**: host, port and password typed under *OBS connection*. *Save and connect* reconnects right away and says whether it worked (connected, wrong password, or nobody answering). Saved in `obs-connection.json` in the data folder. The password never reaches the browser: the page only shows whether one is saved, and leaving the field empty keeps it.
- `KL_OBS_URL=ws://127.0.0.1:4455` and `KL_OBS_PASSWORD=...` override both (the page then shows the connection as fixed).

### The app

The installed app is a small Rust program ([Tauri](https://tauri.app/) for the tray, the installer and starting with Windows). It has no window of its own: it runs the local server, and the control page opens in your normal browser, so nothing heavy stays loaded while it waits in the tray. The page is built into the program. Data goes to `%APPDATA%\Kosmik Layouts` (`profiles.json`, `obs-connection.json`, `hotkeys.json`, and `kosmik-layouts.log`, replaced on each start). Opening it a second time just opens the page. Running programs are read straight from Windows, without `tasklist` or PowerShell. The Node.js version in this repository does exactly the same job and stays here for development; both use the same port, so run only one of them at a time.

To move from the Node.js version to the app, quit the Node.js version and copy `profiles.json` (and `obs-connection.json`, if you set the connection by hand) from its `data` folder into `%APPDATA%\Kosmik Layouts`.

### Updates

The app checks the [latest GitHub release](https://github.com/kosmik-code/kosmik-layouts/releases/latest) shortly after it starts and then every 6 hours, with Tauri's updater: it reads the release's `latest.json`, and installs only an installer signed with the project's update key (the public half is in `app/tauri.conf.json`). Installing runs the installer with a progress bar only, then starts the app again, and the open page reloads itself. The Node.js version doesn't check for updates.

### Data and options

Layouts are stored in `profiles.json` in the data folder (`%APPDATA%\Kosmik Layouts` for the app, `data/` from source), validated before every write and written atomically. A file that fails validation is moved aside as `profiles.broken-<time>.json`. Each layout keeps the size of every source, which the drawing on its card uses; layouts saved by older versions use the sizes OBS reports while connected (press **Update from OBS** once to store them).

Environment variables (app and source): `KL_PORT` (default `7430`), `KL_DATA_DIR` (default `%APPDATA%\Kosmik Layouts` for the app, `./data` from source), `KL_OBS_URL`, `KL_OBS_PASSWORD`. The app also takes `KL_UPDATE_CHECK=0` (no update check) and `KL_PUBLIC_DIR=<folder>` (serve the page from disk while developing). The page accepts `?look=studio|daylight|pixel`.

### Security

- The page listens on `127.0.0.1` only, refuses foreign `Host` headers (DNS rebinding) and refuses changes without a same-origin `Origin` header, so other websites can neither read it nor drive it.
- The only connections it makes are to OBS (on your own PC, or at the address you typed) and, for the app, the update check: it downloads the release's `latest.json` from GitHub every few hours, sending nothing about you. `KL_UPDATE_CHECK=0` turns it off.

### Run from source

With [Node.js](https://nodejs.org/) 22 or newer: download the [latest release](https://github.com/kosmik-code/kosmik-layouts/releases/latest) (**Source code (zip)** under *Assets*) and unzip it, or `git clone https://github.com/kosmik-code/kosmik-layouts`. Open a terminal in its folder and run:

```powershell
npm install
npm start
```

Then open **<http://127.0.0.1:7430/>**. Keep that terminal open while you stream. Updates: `git pull` (or a new zip) and start it again; the `data` folder is kept.

### Development

```powershell
npm test                # the Node.js backend
cd app
cargo test              # the Rust backend (same cases, against a fake OBS)
$env:TAURI_SIGNING_PRIVATE_KEY = "$HOME\.tauri\kosmik-voice.key"   # signs the update (same key as Kosmik Voice)
npx @tauri-apps/cli@2 build   # installer in app/target/release/bundle/nsis/
```

The tests run against an in-process fake OBS that speaks the real obs-websocket v5 protocol, password authentication included.

Building the app needs [Rust](https://rustup.rs/) and the Visual Studio C++ Build Tools. Pushing a `v*` tag that matches the version in `app/Cargo.toml` builds and signs the installer on GitHub and publishes it with `latest.json` as a release (`.github/workflows/release.yml`); installed apps offer it from then on. The signing key lives outside the repository (`~/.tauri/kosmik-voice.key`, shared with Kosmik Voice, also the repository secret `TAURI_SIGNING_PRIVATE_KEY`). Keep a backup: without it, installed copies can't be updated automatically.

| Path | Role |
|---|---|
| `app/src/` | The app: `obs.rs`, `connection.rs`, `games.rs`, `layout.rs`, `controller.rs`, `store.rs`, `server.rs` (same roles as the files below), `main.rs` (tray) and `updates.rs` (the app's updater) |
| `app/tauri.conf.json`, `app/windows/hooks.nsh` | Installer settings; uninstall removes the autostart entry |
| `src/obs.js` | obs-websocket v5 client: auth, requests, reconnects; reads OBS's own settings |
| `src/connection.js` | Where OBS is: automatic, manual or environment |
| `src/games.js` | Process list, start order and which layout a game wants |
| `src/layout.js` | Capturing and applying layouts, capture targets, animation |
| `src/controller.js` | Decides what to apply and when; activity log |
| `src/store.js` | `data/profiles.json` schema, validation and atomic writes |
| `src/server.js` | Control page and JSON API with its guards |
| `public/` | Control page (`kosmik-ui.*` is shared with Kosmik Voice; keep both copies identical) |

## License

[MIT](LICENSE)
