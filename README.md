# nido

**English** · [Español](README.es.md)

A terminal session manager built for Claude Code. Each session is a terminal running its program
(usually Claude) that keeps running when you close the terminal, and comes back when the computer
restarts: Claude picks up its conversation. You drive them from the command line, the browser or
your phone, and if you have several computers, all from one place.

**Works on Windows, macOS and Linux** (WSL 2 too).

This repository is the tool you install on each computer: the sessions, the API and the command
line (`nido …`). From a browser or a phone you use the nido web app or mobile app, paired with
your computer; everything between them is **end-to-end encrypted**.

| System | How it runs |
|---|---|
| Linux with systemd | systemd user service |
| WSL 2 (with systemd) | Same as Linux |
| macOS | launchd agent (no per-session memory limit) |
| Windows 10/11 | Task at sign-in (no `nido attach`: sessions are shown in the web and mobile apps) |

## Use it for free with nido.utebo.info

Nothing to set up: **[nido.utebo.info](https://nido.utebo.info)** is a free public hub serving
the nido web app. Your computer connects to it and you open it from any browser (or from the
mobile app), without opening ports or touching your router.

And it is **private**: the hub only relays encrypted bytes between your computer and your
devices, which are the only ones holding the keys. It cannot read your sessions, what you type
or what Claude answers. What it does see: which computers are connected (by an identifier derived
from their key, not by name), how many devices and how much traffic, and the token used to send
notifications to your phone (their content is encrypted too). The web app is served by the hub
itself: your browser keeps the version you used and warns you if it ever gets a different one.

```sh
# With nido installed (see "Install" below):
nido hub set wss://nido.utebo.info
nido pair          # prints a nido://pair?… code (valid for 10 minutes, once)
```

Open [nido.utebo.info](https://nido.utebo.info), paste the code and you're done (on Windows, the
same with `nido.exe`). To add another computer to the same place, see
[Several computers](#several-computers).

nido's messages follow your language: the web and mobile apps are in English, or in Spanish when
your browser or phone is set to Spanish. The command line is in Spanish for now.

## Usage

```sh
nido ls                          # sessions and their state
nido new api                     # a new Claude in the current folder
nido new web -- npm run dev      # any other command
nido attach api                  # attach from the terminal (Ctrl+\ to detach)
nido attach                      # pick from a list
nido stop|start|restart|rm <name>
nido new api --folder work       # logical folders, for grouping
nido mv api personal             # move to a folder (no folder: `nido mv api`)
nido import-tmux [--dry-run]     # register the Claudes running in tmux
nido accounts                    # your Claude accounts and how much each one has used
nido accounts use 3              # switch Claude (every session) to account 3
nido accounts login [3]          # sign in to a new account, or sign in to account 3 again
```

nido manages your Claude accounts: to sign in it gives you a link to Claude's page (it can be
opened in any browser, including your phone's) and you paste the code it shows at the end. If you
had cswap (claude-swap) accounts, they are imported with their numbers the first time; after that,
better not to use cswap at the same time: each would renew the sessions on its own and invalidate
the other's. With `NIDO_ACCOUNTS=off`, a nido leaves accounts alone (for a test or demo nido next
to the real one).

### From Windows Terminal

One profile per session, or one that asks which. In *Settings → Add a new profile*, command line:

```
wsl.exe -d Debian -- nido attach api
wsl.exe -d Debian -- nido attach
```

`Ctrl+\` detaches from the session (it keeps running); closing the tab does the same.

### Phone and browser

```sh
nido pair                        # QR code to pair a device (valid for 10 minutes, once)
nido pair --days 30              # reusable code for 30 days, only for demo machines (`--stop` revokes it)
nido phones                      # paired devices; `nido phones rm <id>` removes one
nido hub                         # status of the connection to the hub
nido hub set wss://hub.example   # use a hub; `nido hub off` removes it
nido hub test                    # test notification to your devices
```

How it works:

- **No open ports.** nido opens an outgoing connection to a *hub*, and the phone or browser joins
  it. The hub only relays encrypted bytes and sends the notifications (whose content is encrypted
  too).
- **End-to-end encryption** with [Noise](https://noiseprotocol.org/)
  (`Noise_IK_25519_ChaChaPoly_SHA256`, and `IKpsk1` with the QR code's secret for pairing).
  Neither the hub nor anyone in between can read the sessions, the conversation or what you type.
- **Identity.** Your computer's identity is the key in `~/.config/nido/hub.key`: don't share it,
  and if it is lost you need to pair your devices again. Each device has its own key and its own
  token, and `nido phones rm` locks it out immediately.
- **At home**, if nido listens on the LAN, the phone can connect directly, over the same encrypted
  channel.

### Several computers

```sh
nido group invite                # on a computer in the group: a code (10 minutes, once)
nido group join '<code>'         # on the new computer, connected to the same hub
nido group                       # the computers in the group; `nido group rm <id>` removes one
```

The computers in a group share which devices are authorized: a phone or browser paired with any
of them works with all of them, and removing it on one removes it on all. All computers are equal.
They sync with each other through the hub, end-to-end encrypted; the hub does not see the list.
Each computer's name is the system's, or `NIDO_NAME`.

## The API

It only answers on this computer (`127.0.0.1`), with `Authorization: Bearer <token>`: the one in
`~/.config/nido/token` (the command line) or a paired device's (which arrives over the encrypted
channel). From outside this computer only the encrypted channel answers (`/api/v1/e2e`). Messages
come in English, or in Spanish if the request asks for it with `Accept-Language: es`.

## Install

There are binaries for Linux (x86_64 and ARM64), macOS (Apple Silicon and Intel) and Windows
(x86_64) in [Releases](https://github.com/jleal52/nido/releases). Each release includes
`SHA256SUMS` to verify them.

**Linux** (and WSL 2), with a systemd user service. Replace `x86_64` with `aarch64` on ARM:

```sh
curl -fsSL https://github.com/jleal52/nido/releases/latest/download/nido-x86_64-unknown-linux-gnu.tar.gz | tar -xz
cd nido-x86_64-unknown-linux-gnu
install -Dm755 nido ~/.local/bin/nido
install -Dm644 contrib/nido.service ~/.config/systemd/user/nido.service
systemctl --user daemon-reload && systemctl --user enable --now nido
```

**macOS**, with a launchd agent. Replace `aarch64` with `x86_64` on an Intel Mac:

```sh
curl -fsSL https://github.com/jleal52/nido/releases/latest/download/nido-aarch64-apple-darwin.tar.gz | tar -xz
cd nido-aarch64-apple-darwin
install -d ~/.local/bin ~/Library/LaunchAgents
install -m755 nido ~/.local/bin/nido
sed "s/USUARIO/$(whoami)/g" contrib/nido.plist > ~/Library/LaunchAgents/io.github.jleal52.nido.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.jleal52.nido.plist
```

The binary is not signed by Apple. Downloaded with `curl`, as above, macOS lets it run; if you
download it with a browser and macOS won't open it, remove the download flag with
`xattr -d com.apple.quarantine ~/.local/bin/nido`.

**Windows 10/11** (PowerShell), with a scheduled task that starts nido when you sign in, as your
user and with no window (`nidow.exe` launches it without a console):

```powershell
$zip = "$env:TEMP\nido.zip"
Invoke-WebRequest https://github.com/jleal52/nido/releases/latest/download/nido-x86_64-pc-windows-msvc.zip -OutFile $zip
Expand-Archive $zip -DestinationPath "$env:TEMP\nido" -Force
New-Item -ItemType Directory -Force ~\.local\bin | Out-Null
Copy-Item "$env:TEMP\nido\nido*.exe" ~\.local\bin\
~\.local\bin\nido.exe service install          # --name other-name: as shown in the web and mobile apps
```

For `nido` to work in any terminal, add `~/.local/bin` (on Windows, `%USERPROFILE%\.local\bin`)
to your `PATH` if it isn't there.

### Build it yourself

With Rust 1.88 or newer ([rustup](https://rustup.rs)):

```sh
git clone https://github.com/jleal52/nido && cd nido
cargo build --release      # produces target/release/nido (and on Windows, nidow.exe too)
```

and then the same steps as above with `target/release/nido` instead of the downloaded binary.

On Windows it listens on `127.0.0.1:7691` (7690 may belong to WSL on the same computer), the log
is in `~\.local\state\nido\serve.log` and `nido service uninstall` removes the task. Signing out
of Windows closes nido's sessions; when you sign in again, the autostart ones open.

To update it: Windows won't let you replace a running `.exe`, but it will let you rename it.
Rename `nido.exe` and `nidow.exe` (e.g. to `nido.old.exe`), copy the new ones and restart the task
(`schtasks /End /TN nido` and `schtasks /Run /TN nido`): the manager restarts and the sessions
keep running.

By default it listens on `127.0.0.1:7690`. For the phone to connect directly at home, add the LAN
address with `NIDO_BIND` in a unit *drop-in* (`~/.config/systemd/user/nido.service.d/bind.conf`):
only the encrypted channel is served on it.

Restarting the manager doesn't touch the sessions: each one is its own unit (`nido-s-<id>`, with
`MemoryMax=4G`).
