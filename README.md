# Kosmik Layouts

Per-game OBS layouts without per-game scenes. Keep **one** live scene; when a game starts, Kosmik Layouts moves, resizes, shows and hides that scene's sources into the layout you saved for that game. When the game closes, it goes back to your default layout.

**Proof of concept.** It works end to end, but it is small on purpose: no visual editor yet. You arrange the scene in OBS itself and the app remembers it.

## How it works

- **Layouts are snapshots of OBS.** Arrange your live scene in OBS for a game (camera to a corner, chat smaller, hide the music widget…), then press **Save layout from OBS**. The app stores where every source is, its size, crop and whether it is visible.
- **Capture targets.** For *Game Capture*, *Window Capture* and *Application Audio Capture* sources, a layout also remembers what they capture: the mode (specific window or any fullscreen app) and which game. So one layout can use Game Capture on PoE and another Window Capture on a game where Game Capture doesn't work. Targets are matched by program, not just window title, so another game built on the same engine (two Unity games, say) is never grabbed by mistake. A capture is only re-pointed when its target really changes, and only if that layout shows it, so nothing restarts needlessly.
- **Game detection.** Every 3 seconds the app reads the Windows process list. If several programs with a layout are running, the one **started last** wins, so an idle game left open in the background never takes over from the game you just launched, and alt-tabbing changes nothing. No game running means the **default layout**.
- **Switching.** It only moves things when the wanted layout changes: a game starts or closes, you lock or unlock, or OBS reconnects. Anything you tweak by hand in OBS stays until the next switch. Moves are animated (400 ms by default, 0 = instant).
- **Only what you choose.** Each layout lists the sources it controls; untick one (for example *Alerts*) and that layout never touches it. Sources that are not in the scene are skipped and reported in *Activity*.
- **Manual control.** *Apply now* shows a layout once. *Lock* keeps a layout on screen whatever game runs, until *Unlock*.

It never creates, deletes or renames anything in OBS. It only changes position, size, crop and visibility of the sources a layout lists, plus the capture target of capture sources.

## Requirements

- Windows, OBS 28 or newer (the WebSocket server is built in), Node.js 22 or newer.
- In OBS: **Tools › WebSocket Server Settings › Enable WebSocket server**.

## Run it

```powershell
npm install
npm start
```

Open <http://127.0.0.1:7430/>. The page is in Spanish or English, with three looks (Studio, Daylight, Pixel) shared with Kosmik Voice; both are picked from the round button at the top right. If OBS can't be reached, the page says why and what to check.

The page has two tabs:

- **Layouts**: what's happening now (game → layout → scene), your layouts with a drawing of where each source sits, a live picture of what the scene shows (*On stream now*, a small screenshot every 2 seconds; *Pause* stops it), the form to save the current scene, and the activity log. A *First steps* checklist shows until OBS is connected, a no-game layout exists and a game layout exists.
- **OBS connection**: how the app reaches OBS, its status (address, OBS version, canvas size) and the behaviour settings (scene to control, default layout, animation length).

## Connecting to OBS

- **Automatic** (default): the WebSocket port and password are read from OBS's own settings on this PC (`%APPDATA%\obs-studio\plugin_config\obs-websocket\config.json`), so there is nothing to type. If you change the port or password in OBS, the app picks it up by itself.
- **Manual**: for OBS on another PC (two-PC setups), or to type the port and password yourself. Enter the server IP or name, the port and the password from OBS's *Tools › WebSocket Server Settings › Show Connect Info*. *Save and connect* reconnects right away and tells you whether it worked (connected, wrong password, or nobody answering). Game detection always looks at the PC this app runs on, so in a two-PC setup run it on the gaming PC.

Manual settings are saved in `data/obs-connection.json`. The password never reaches the browser: the page only shows whether one is saved. Leaving the password field empty keeps the saved one.

`KL_OBS_URL=ws://127.0.0.1:4455` and `KL_OBS_PASSWORD=...` still work and override the page (which then shows the connection as fixed).

## First layouts

1. Arrange your live scene in OBS for when you're not playing. Under **Save the current scene**, name it (say, `Just Chatting`), leave *Game program* empty, tick **Use it when no game is open**, and press **Save from OBS**.
2. Start a game and arrange the scene for it. If it uses a Game Capture or Window Capture, point that at the game too. Press **Pick from open apps** to choose its program (or type it, e.g. `eldenring.exe`), name it and save.
3. Close and reopen the game: the layout follows.

By default the app controls whichever scene is live; pick a fixed one under **Settings › Scene to control**.

Layouts are stored in `data/profiles.json`. Sources are matched by name, so if you rename a source in OBS, press **Update from OBS** on the layouts that use it. The **⋯** menu on each layout can *Duplicate* it (a copy with no game, to build a variant), make it the no-game default, or delete it.

The drawing on each layout needs the size of each source. Layouts saved from now on keep it; older layouts use the sizes OBS reports while connected (press **Update from OBS** once to store them).

Environment variables: `KL_PORT` (default `7430`), `KL_DATA_DIR` (default `./data`), `KL_OBS_URL`, `KL_OBS_PASSWORD`.

## Security

- The control page listens on `127.0.0.1` only, refuses foreign `Host` headers (DNS rebinding) and refuses changes without a same-origin `Origin` header, so other websites can neither read nor drive it.
- The only connection it makes is to OBS: on your own PC, or at the address you typed under *OBS connection*.

## Not yet (ideas for after the proof of concept)

- A visual editor: drag sources over the live picture instead of arranging them in OBS.
- Twitch category as a trigger (and updating title/category when a game starts).
- Vertical canvas layouts (Aitum Vertical) and nested scenes/groups.
- Tray launcher instead of a console.

## Tests

`npm test` runs everything against an in-process fake OBS that speaks the real obs-websocket v5 protocol, password authentication included.

## License

MIT
