# cadence

A terminal dashboard for Bluetooth LE cycling speed/cadence sensors, written in Rust.
Built and tested around the **Saris Speed/Cadence** sensor, but it works with any sensor
that exposes the standard **Cycling Speed and Cadence Service** (`0x1816`).

- Live cadence (rpm), colour-coded by zone
- Rolling 3-minute graph with a 90 rpm target line
- Session stats: average, max, pedalling time, elapsed time
- Speed, max speed and distance when the sensor is mounted on the wheel hub
- Auto-reconnects when the sensor sleeps and wakes

<p align="center">
  <img width="675" height="433" alt="57_027c7e99-8ca7-4f42-9891-b02656a8beef" src="https://github.com/user-attachments/assets/d7605f8c-a67a-4085-bad1-ff615fb1fb60" />
</p>

## Requirements

- Linux with BlueZ running (`systemctl status bluetooth`) and a Bluetooth LE adapter
- **Rust 1.85 or newer** (install with [rustup](https://rustup.rs); distro packages are often too old)
- D-Bus development headers:

```bash
# Debian / Ubuntu
sudo apt install libdbus-1-dev pkg-config
```

## Build and run

```bash
cargo run --release
cargo run --release -- --wheel-mm 2136   # set your wheel circumference
cargo test                               # unit tests, no hardware needed
```

Shake or spin the sensor first: it sleeps after 5 minutes and wakes on motion.
Close any phone app (Strava, Zwift...) that's connected to it, because most BLE sensors
accept only one connection.

| Option | Meaning |
| --- | --- |
| `--wheel-mm <mm>` | Wheel circumference for speed and distance. Default `2105` (700x25c). |
| `-h`, `--help` | Show usage |

| Key | Action |
| --- | --- |
| `q` / `Esc` / `Ctrl-C` | Quit |
| `r` | Reset session stats and graph |

## Cadence or speed?

The Saris sensor configures itself from how it moves. Mount it on the **crank arm** and you get
cadence; mount it on the **wheel hub** and you get speed. The dashboard shows whichever panels
it receives data for. You need a second sensor to see both.

## How it works

```
 BLE task (tokio) --Msg--> main loop (select!) --draw--> terminal
 input thread   --KeyEvent-->      ^
```

1. `btleplug` scans for a device advertising service `0x1816` (or named "Saris"), connects, and subscribes to the CSC Measurement characteristic (`0x2A5B`).
2. Each notification holds cumulative revolution counts and the time of the last event in 1/1024 s units. The counts are kept by the sensor itself, not counted from when you connected, so the program only ever uses the *difference* between two packets. `parse_csc` decodes them.
3. `Rate` turns two consecutive samples into revolutions per second, handling counter wraparound (16-bit crank, 32-bit wheel), 16-bit timestamp wrap, repeated packets (see below), and glitch spikes.
4. The UI (`ratatui`) redraws every 100 ms and records one graph/stat sample per second.

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

## Project layout

```
Cargo.toml
src/main.rs    parsing, rate maths, BLE session, UI, tests
```
