# cadence

<p align="center">
  <img width="800" alt="cadence terminal dashboard" src="https://github.com/user-attachments/assets/a0bf23e0-f892-4ade-832c-6f8795d63532" />
</p>

A dashboard for Bluetooth LE cycling speed/cadence sensors, written in Rust. Use it in the
terminal, or open the **3D browser view** and ride a 9-mile loop of English countryside where
your cadence is your speed.
Built and tested around the **Saris Speed/Cadence** sensor, but it works with any sensor
that exposes the standard **Cycling Speed and Cadence Service** (`0x1816`).

- Live cadence (rpm), colour-coded by zone
- Rolling 3-minute graph with a 90 rpm target line
- Session stats: average, max, pedalling time, elapsed time
- Speed, max speed and distance when the sensor is mounted on the wheel hub
- Every ride is saved, with a history screen to browse and delete them
- `--gui`: first-person 3D ride in your browser, driven by your cadence
- Auto-reconnects when the sensor sleeps and wakes

  <span style="display:inline-block; width: 40px;"></span>

<p align="center">
  <img width="675" height="433" alt="57_027c7e99-8ca7-4f42-9891-b02656a8beef" src="https://github.com/user-attachments/assets/d7605f8c-a67a-4085-bad1-ff615fb1fb60" />
</p>
<span style="display:inline-block; width: 40px;"></span>

## Requirements

- Linux with BlueZ running (`systemctl status bluetooth`) and a Bluetooth LE adapter
- **Rust 1.85 or newer** (install with [rustup](https://rustup.rs); distro packages are often too old)
- D-Bus development headers:

```bash
# Debian / Ubuntu
sudo apt install libdbus-1-dev pkg-config
```

- For `--gui`: a browser with WebGL (Firefox, Chrome, Brave...), and a one-off download of
  three.js (see [3D browser view](#3d-browser-view))

## Build and run

```bash
cargo run --release                       # terminal dashboard
cargo run --release -- --gui              # 3D browser view
cargo run --release -- --wheel-mm 2136    # set your wheel circumference
cargo test                                # unit tests, no hardware needed
```

Shake or spin the sensor first: it sleeps after 5 minutes and wakes on motion.
Close any phone app (Strava, Zwift...) that's connected to it, because most BLE sensors
accept only one connection.

| Option | Meaning |
| --- | --- |
| `--wheel-mm <mm>` | Wheel circumference for speed and distance. Default `2105` (700x25c). |
| `--db <path>` | Ride database. Default `~/.local/share/cadence/rides.db`. |
| `--gui` | Open the 3D browser view instead of the terminal UI. |
| `--port <n>` | Port for `--gui`. Default `8787`. |
| `--no-open` | With `--gui`, don't launch a browser automatically. |
| `-h`, `--help` | Show usage |

### Terminal keys

| Key | Action |
| --- | --- |
| `q` / `Esc` / `Ctrl-C` | Quit (saves the current ride) |
| `r` | Save this ride and start a new one from zero |
| `h` / `Tab` | Open ride history |

On the history screen:

| Key | Action |
| --- | --- |
| `↑` / `↓` or `k` / `j` | Select a ride |
| `x` / `Delete` | Delete the selected ride (press twice; any other key cancels) |
| `h` / `Tab` / `Esc` | Back to the live view |
| `q` | Quit |

## Ride history

Each ride is recorded to a small SQLite database as you pedal: one cadence sample per second,
plus totals (pedalling time, average and max cadence) when the ride ends. A ride ends when you
press `r`, quit, or stop the GUI with `Ctrl-C`. Rides with only a few seconds of pedalling are
discarded, so fiddling with the sensor doesn't clutter the list. If the program crashes
mid-ride, the next start recovers it.

If the database can't be opened, cadence keeps working and shows a warning; rides just
aren't saved. Rides recorded in the browser view show up in the terminal history, and the
other way round.

## 3D browser view

```bash
cargo run --release -- --gui
```

This serves a page on `http://127.0.0.1:8787/` (this machine only) and opens it in your
browser. The page is built into the binary; the sensor data reaches it live over
Server-Sent Events.

- You ride in first person, on the left of the lane, round a flat **9-mile closed loop**
- **More rpm means more speed.** Speed is cadence times a gear (default 6.2 m per revolution,
  so 90 rpm is about 21 mph), eased so you accelerate and coast like a real bike
- Procedural tarmac, branching trees (including broad oaks), leafy hedges, fields of
  sunflowers and tulips, and three villages with houses and shops
- Slower cyclists share the road. Pull up behind one and you swing out to overtake;
  the "passed" counter on the right keeps score
- The display shows an rpm gauge with the 90 rpm target marked, ride time, pedalling time,
  mph, miles round the loop, and the lap number

The speed in the 3D view is simulated from cadence. It doesn't use a wheel sensor, and your
real speed and distance still appear in the terminal view if the sensor is on the wheel hub.

### One-off setup

The page imports three.js from a file bundled into the binary, so the GUI works offline.
Download it before the first build:

```bash
mkdir -p web
curl -L https://cdn.jsdelivr.net/npm/three@0.160.0/build/three.module.js -o web/three.module.js
cargo build --release
```

`cargo build` fails with a "couldn't read" error if `web/three.module.js` is missing.

### Browser keys

| Key | Action |
| --- | --- |
| `↑` / `↓` | Pedal without a sensor (+/- 4 rpm per press; slowly coasts back down) |
| `[` / `]` | Lower / raise the gear (3 to 10 m per revolution) |
| `f` | Fullscreen |

### Font

The page is set in **Caskaydia Cove** (Nerd Font). Install it on the machine running the
browser; if it isn't found the page falls back to Cascadia Code, then any monospace font.

### Previewing without a bike

Serve the `web/` folder with any static server and use the keyboard to pedal:

```bash
cd web
python3 -m http.server 8787
```

Open `http://127.0.0.1:8787/`. Edits to `web/index.html` show up on refresh, with no rebuild.
Remember to `cargo build` again before using `--gui`, as the page is compiled into the binary.

Tip: set the gear to 10 and hold `↑` to reach the first village (about 2.3 km) in a couple
of minutes.

### Tweaking the scenery

The settings sit near the top of the scenery section in `web/index.html`:

- `VIL`: village positions, in metres along the loop
- `FIELDS`: flower field positions and sides (even entries are sunflowers, odd are tulips)
- `SPECIES`: tree shapes
- the tree count (`n<1300`) and the number of cyclists (`k<7`)

If the frame rate drops on an older machine, reduce the tree count first, then make the
tulip spacing (`da`, `db` in the field code) coarser.

## Cadence or speed?

The Saris sensor configures itself from how it moves. Mount it on the **crank arm** and you get
cadence; mount it on the **wheel hub** and you get speed. The dashboard shows whichever panels
it receives data for. You need a second sensor to see both. The 3D view needs the **cadence**
data (crank mount).

## How it works

```
 BLE task (tokio) --Msg--> App (select! loop) --draw--> terminal      (default)
 input thread   --KeyEvent-->      |
                                   +--telemetry (SSE, 10 Hz)--> browser   (--gui)
                                   +--samples, totals--> SQLite ride database
```

1. `btleplug` scans for a device advertising service `0x1816` (or named "Saris"), connects, and subscribes to the CSC Measurement characteristic (`0x2A5B`).
2. Each notification holds cumulative revolution counts and the time of the last event in 1/1024 s units. The counts are kept by the sensor itself, not counted from when you connected, so the program only ever uses the *difference* between two packets. `parse_csc` decodes them.
3. `Rate` turns two consecutive samples into revolutions per second, handling counter wraparound (16-bit crank, 32-bit wheel), 16-bit timestamp wrap, repeated packets (see below), and glitch spikes.
4. The `App` records one graph/stat sample per second and writes it to the ride database.
5. In terminal mode, `ratatui` redraws every 100 ms. With `--gui`, a tiny built-in HTTP server (`web.rs`, plain tokio, no extra crates) serves the page and streams the current cadence and ride stats to it ten times a second. The page does all the 3D work with three.js.

### Repeated packets

The sensor sends packets on its own schedule (roughly once a second), but the counters only change when a revolution happens. So many packets carry the same totals as the one before, and while you're stopped every packet does.

- **Repeats are ignored.** If the revolution count hasn't changed, `Rate::update` returns without touching its previous sample. The next packet with a new revolution is compared against the last *real* event, so the time difference covers the whole gap and the cadence stays accurate however many duplicates arrive in between.
- **Sensor timestamps, not arrival times.** The maths uses the event time reported by the sensor (1/1024 s units). Bluetooth delivery jitter never affects the result. The local clock is only used to decide "no new revolution for 3 seconds, so show 0 rpm".
- **Several revolutions per packet are fine.** At high cadence one packet can cover 2 or 3 revolutions, and the time difference spans the same interval.

## Troubleshooting

| Symptom | Try |
| --- | --- |
| `could not talk to BlueZ` | `sudo systemctl enable --now bluetooth` |
| `no Bluetooth adapter found` | `rfkill list`, `bluetoothctl show`, check the BIOS wireless toggle, or use a USB BLE dongle |
| Stuck on "Scanning..." | Shake the sensor, close phone apps, check it shows up in `bluetoothctl scan on`. If its name has no "Saris" and it doesn't advertise `0x1816`, edit `is_candidate` |
| Cadence reads 0 while pedalling | The sensor may be configured as a speed sensor. Move it to the crank and spin it |
| `⚠ rides won't be saved` | The database couldn't be opened. Check permissions on `~/.local/share/cadence/` or pass `--db <path>` |
| Build error mentioning `AsyncWriteExt` or `tokio::signal` | Enable the tokio features: `cargo add tokio --features net,io-util,signal` |
| Build error "couldn't read ../web/..." | `web/index.html` or `web/three.module.js` is missing; see [One-off setup](#one-off-setup) |
| `--gui` page shows only the dials, no scenery | Hard-refresh (`Ctrl+Shift+R`), then check the browser console (`F12`) for the first error. A `404` on `/three.module.js` means the file wasn't bundled |
| `can't listen on 127.0.0.1:8787` | Something else is using the port; try `--port 8800` |
| 3D view is slow | Reduce the tree count and tulip density, or close other GPU-heavy tabs |

## Project layout

```
Cargo.toml
src/main.rs         parsing, rate maths, BLE session, terminal UI, CLI, tests
src/db.rs           ride database (SQLite)
src/web.rs          --gui: tiny HTTP + Server-Sent Events server
web/index.html      the 3D view (three.js scene, HUD)
web/three.module.js three.js r160, bundled into the binary (download it yourself)
```
