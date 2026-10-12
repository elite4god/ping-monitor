# Ping Monitor

A cross-platform desktop application, written in Rust, for monitoring many hosts
with ICMP echo requests ("pings"). Add your hosts, leave the window open, and see at a glance
what is up, what is down, and how healthy each link is.

The GUI is built with [egui](https://github.com/emilk/egui)/eframe and ICMP is
handled asynchronously with [surge-ping](https://crates.io/crates/surge-ping) on
top of [tokio](https://tokio.rs), so a slow or dead host never freezes the UI.

## Features

- Monitor any number of hosts by IPv4 address, IPv6 address or hostname
- Per-host ping interval, timeout, and "mark DOWN after N failures" threshold
- Live table showing status, last / average / min / max latency, packets sent and loss %
- Live latency graph for the selected host, with timeouts marked
- Event log recording every UP / DOWN transition with a timestamp
- Pause all monitoring, or enable / disable individual hosts
- Host list is saved automatically and restored on the next launch
- Runs on Linux, Windows and macOS from a single code base

## Usage

1. Enter a name (optional), a host or IP, an interval and a timeout in the top bar, then click **Add**.
2. Click a host in the table to see its graph and edit its settings.
3. Watch the event log at the bottom for UP / DOWN changes.

Hosts are saved to a `hosts.json` file in your user configuration folder:

| OS      | Location                                              |
|---------|-------------------------------------------------------|
| Linux   | `~/.config/pingmonitor/hosts.json`                    |
| macOS   | `~/Library/Application Support/PingMonitor/hosts.json`|
| Windows | `%APPDATA%\PingMonitor\config\hosts.json`             |

## Project layout

```
ping-monitor/
├── Cargo.toml                 Crate manifest (+ .deb packaging metadata)
├── build.rs                   Embeds the icon / version info in the Windows .exe
├── src/main.rs                The application
├── assets/                    Linux desktop entry and icon
├── debian/postinst            Grants ICMP permission when installing the .deb
├── wix/                       Windows installer definition, license and icon
└── scripts/
    ├── build-deb.sh           Build a Debian/Ubuntu package
    └── build-msi.ps1          Build a Windows MSI installer
```

## Prerequisites (all platforms)

Install the Rust toolchain from <https://rustup.rs> (stable, 2021 edition or newer).
Verify with:

```bash
cargo --version
```

## Linux

### Build dependencies

On Debian / Ubuntu:

```bash
sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev \
    libx11-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libgl1-mesa-dev
```

Other distributions need the equivalent development packages (X11/Wayland,
xkbcommon, OpenGL).

### Build and run

```bash
cargo run --release
```

### Build a .deb package

```bash
chmod +x scripts/build-deb.sh debian/postinst
./scripts/build-deb.sh
```

This installs [`cargo-deb`](https://crates.io/crates/cargo-deb) if needed and
writes the package to `target/debian/`, for example
`ping-monitor_0.1.0-1_amd64.deb`.

### Install

```bash
sudo apt install ./target/debian/ping-monitor_0.1.0-1_amd64.deb
```

The package installs the program to `/usr/bin/ping-monitor`, adds an
application-menu entry, and runs `setcap cap_net_raw+ep` on the binary so ICMP
works without running as root. Remove it with `sudo apt remove ping-monitor`.

### Notes

- A `.deb` only works on the CPU architecture it was built on. Build on an ARM
  machine to get an `arm64` package. For the widest compatibility, build on the
  oldest distribution release you want to support.
- If you run the binary directly (not via the package) and every host reports
  `Cannot open ICMP socket`, allow unprivileged ICMP once:
  ```bash
  sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"
  ```
  (add the setting to `/etc/sysctl.conf` to make it permanent), or run
  `sudo setcap cap_net_raw+ep target/release/ping-monitor`.

## Windows

### Build dependencies

- Rust with the MSVC toolchain (the default on Windows)
- Visual Studio Build Tools with the **Desktop development with C++** workload,
  which includes the Windows SDK needed to embed the exe icon

### Build and run

```powershell
cargo run --release
```

### Build an MSI installer

1. Install **WiX Toolset 3.x** from <https://wixtoolset.org> (or
   `winget install WiXToolset.WiXToolset`), then open a new terminal.
2. From the project folder run:
   ```powershell
   .\scripts\build-msi.ps1
   ```

The script installs [`cargo-wix`](https://crates.io/crates/cargo-wix) if needed
and writes the installer to `target\wix\`, for example
`ping-monitor-0.1.0-x86_64.msi`.

### Install

Double-click the `.msi` and follow the wizard, or install silently from an
elevated prompt:

```powershell
msiexec /i ping-monitor-0.1.0-x86_64.msi /qn
```

The installer places the program in `Program Files\Ping Monitor`, adds a Start
Menu shortcut, and registers an uninstall entry. Newer versions upgrade older
ones in place.

### Notes

- ICMP on Windows may require administrator rights. If hosts show a socket
  error, right-click the shortcut and choose **Run as administrator**.
- The MSI is not code-signed, so Windows SmartScreen may warn on first run.
- Do not change the `UpgradeCode` GUID in `wix/main.wxs`; it is what lets new
  versions replace old ones.

## macOS

### Build dependencies

- Xcode Command Line Tools: `xcode-select --install`
- Rust via rustup (see above)

### Build and run

```bash
cargo run --release
```

ICMP works without administrator rights on macOS.

### Install

There is no installer package for macOS yet. To install the binary for your user:

```bash
cargo install --path .
```

This places `ping-monitor` in `~/.cargo/bin` (make sure that folder is on your
`PATH`). Alternatively, copy `target/release/ping-monitor` anywhere you like,
for example `/usr/local/bin`.

To build for both Intel and Apple Silicon, add the other target
(`rustup target add x86_64-apple-darwin` or `aarch64-apple-darwin`) and build
with `cargo build --release --target <target>`.

## Ideas for future work

- Sound or desktop notifications when a host goes down
- Export the event log and statistics to CSV
- TCP port and HTTP checks alongside ICMP
- A macOS `.app` / `.dmg` bundle

## License

MIT — see `LICENSE`. 