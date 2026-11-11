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

## Claude sessions working together (the nido MCP)

Each Claude that nido launches gets nido's MCP server (`nido mcp`), with these tools:

- `sessions`: the Claude sessions on every computer in your group, with their state and `since`
  (when it started, if it is running, or when it stopped).
- `send`: a message to another session. It arrives through a Claude Code
  [channel](https://code.claude.com/docs/en/channels), and the answer comes back the same way.
- `read`: what a session has said since it was last read, its state and its pending question.
- `open`, `answer` and `close`: open a new Claude session (a child) in a folder on any computer
  in the group, answer its questions and permission requests, and close it.
- `start` and `stop`: start a stopped Claude session on any computer in the group, picking up its
  conversation, and stop one (its conversation is kept). `stop` refuses a session that is working,
  has a pending question or has subagents or commands running in the background, unless asked with
  `force`; a session can't stop itself.
- `alert`: a notification to your phone (and a 🔔 in `nido ls` and in the apps), for when you
  asked to be told about something.
- `secret`: asks you for a token or a password without it going through the conversation (see
  [Secrets, texts and files](#secrets-texts-and-files)).
- `clipboard`: a text for you to copy (a box with a Copy button in the web and the app), and a
  notification. Instead of `text`, it takes the `path` of a text file (relative to the session's
  folder, like `send_file`), so the content doesn't go through the conversation; for a command's
  output, the session runs `<command> | nido clip` (see
  [Secrets, texts and files](#secrets-texts-and-files)).
- `send_file`: a copy of a file to open or save from the web or the app, and a notification.

What nido changes in your Claude setup:

- Every time `nido serve` starts, it registers the server in Claude's user configuration
  (`claude mcp add -s user nido -- <path to nido> mcp`), in `~/.claude` and in each profile
  directory (`CLAUDE_CONFIG_DIR`) your sessions use. If it is already there it is left alone; if
  it points to another `nido`, it is replaced.
- The Claude sessions it launches get `--dangerously-load-development-channels server:nido`
  (channels are a research preview, and nido's is a development channel) and
  `--allowedTools mcp__nido__sessions mcp__nido__read mcp__nido__send mcp__nido__alert
  mcp__nido__secret mcp__nido__clipboard mcp__nido__send_file`. When it loads a development
  channel, Claude shows a warning at startup: nido accepts it for you, but only when the list is
  exactly `server:nido`.
- Only those seven tools run without asking: they look, talk, or ask and send things to you
  (`secret` is controlled by its own dialog). `open`, `answer`, `close`, `start` and `stop` act
  on other sessions, so they go through Claude's usual permission prompt (in auto mode, its
  classifier decides).
- They also get `--disallowedTools "Read(//<state>/secrets/**)" "Edit(//<state>/secrets/**)"`,
  with the absolute path of nido's secrets folder (and its real path too, if there is a link on
  the way). Claude applies the `Read` rule to its Read tool and to `cat`, `head`, `<`… in Bash,
  and the `Edit` rule to Edit and Write: that way Claude doesn't read a secret into the
  conversation by mistake.
- The channel flags are only added if your Claude Code knows them. With an older one, the
  session starts without them and `sessions` shows a `channel_error`.

**Team and Enterprise plans**: an Owner has to enable channels for the organization
(*Organization settings → Claude Code → Channels*, or `channelsEnabled` in managed settings).
Until then the tools work but messages don't arrive: nido reads Claude's warning and `sessions`
shows `channel_error: "blocked by org policy"` for that session.

### Secrets, texts and files

When a session calls `secret`, a dialog opens in the nido web and app with the secret's name and
the session's reason, and your phone gets a notification. What you type goes end-to-end encrypted
to that computer and is saved in a private file; the session gets the file's path, never the
value. You can have it remembered 2 hours, 7 days or forever, for every session on that computer
(never on other computers); otherwise it is only for that session. `secret` waits up to 15
minutes for your answer.

The session uses the secret in one of two ways:

- With its path, for programs that read it from a file (`--password-file <path>`).
- With `nido secrets run <name> --env VAR -- <command> [args…]`, for programs that want it in an
  environment variable: it runs the command with the secret in `VAR` and without showing it, and
  exits with the command's code. It takes the same one `secret` returned, so it only works inside
  a nido session (`NIDO_SESSION`). If `nido` is not in the session's `PATH`, `secret` also returns
  its full path: put it in double quotes, like the secret's path, because they may contain spaces.
  On Windows, `.cmd` and `.bat` commands (`npm`, `npx`…) go through `cmd /c`:
  `nido secrets run npm-token --env NPM_TOKEN -- cmd /c npm publish`.

If a program only takes the secret as an argument, Claude tells you so instead of using it.

To send you a text without writing it into the conversation, a session uses `clipboard` with a
file's `path`, or `nido clip`: `git diff | nido clip --title "the diff"` sends its standard input,
and `nido clip ~/.ssh/id_ed25519.pub` a file. It goes to the clipboard of the session it runs in
(`NIDO_SESSION`, so only inside a nido session), like `clipboard`: the web, the app and a
notification. It only answers how much it sent (`enviado al portapapeles (1 KB)`), never the
content. Only UTF-8 text, up to 64 KB: anything else is an error (for a binary file, use
`send_file`), and so is an empty text. Like `send_file`, it doesn't take anything from nido's
secrets folder.

Where they live and how long they last (`<state>` is `~/.local/state/nido`,
`$XDG_STATE_HOME/nido` or `$NIDO_HOME/state`; on Windows, `%USERPROFILE%\.local\state\nido`):

- Secrets, in `<state>/secrets/`, readable only by your user. A remembered one stays until it
  expires or you forget it; one that isn't remembered is deleted when its session is closed, or
  after 24 hours. A secret can be up to 16 KB.
- What `clipboard` and `send_file` send, in `<state>/outbox/`: kept 7 days or until the session
  is closed, at most the last 50 per session and 256 MB per computer (the oldest go first). A text
  can be up to 64 KB and a file up to 64 MB.
- On Windows nido doesn't change permissions: the files inherit those of your user profile, which
  are only yours. So the state folder must stay inside your profile (don't move it out with
  `XDG_STATE_HOME` or `NIDO_HOME`).

```sh
nido secrets                                      # the secrets remembered on this computer (never their value)
nido secrets forget <name>                        # forget one: the next session that needs it asks again
nido secrets run <name> --env VAR -- <command>    # run a command with a secret in VAR
<command> | nido clip [--title T]                 # send its output to the session's clipboard
nido clip <file> [--title T]                      # send a text file to the session's clipboard
```

If you set Claude Code's `MCP_TOOL_TIMEOUT` below 15 minutes, `secret` fails before you have time
to answer.

Security:

- The tools only reach the computers in your nido group, over the same end-to-end encrypted
  channel. Another computer in the group can't ask for secrets or send texts or files on behalf
  of a session: only the sessions on that computer can.
- Only a paired phone or browser (a person) can answer a secret, not the command line's token,
  which every session's `nido mcp` has. With no device paired, `secret` fails at once.
- The conversation doesn't see a secret as long as Claude doesn't read it, and nido makes Claude
  refuse to read it (see above). But a program that opens the file on its own can read it: this
  guards against accidents, not against a session that wants to read the secret.
- A session can only answer and close the sessions it opened with `open`.
- Each computer runs at most 10 sessions opened with `open` from the same computer (itself
  included), and keeps at most 50 of them registered.
- Trusting a folder is always up to you: `answer` refuses Claude's "trust this folder" question,
  and nido sends you an alert when a child is waiting on it (the session that opened it learns
  it from `read`).
- Claude is told that messages from other sessions don't come from you.

```sh
nido mcp off     # the next Claude sessions start without nido's MCP (running ones stay as they are)
nido mcp on      # back on
nido alerts      # alerts from the sessions on every computer in the group that you haven't seen
```

`nido mcp off` doesn't remove the server from Claude's configuration. To undo that too:
`claude mcp remove -s user nido` (and, for each profile,
`CLAUDE_CONFIG_DIR=<profile> claude mcp remove -s user nido`).

## The API

It only answers on this computer (`127.0.0.1`), with `Authorization: Bearer <token>`: the one in
`~/.config/nido/token` (the command line) or a paired device's (which arrives over the encrypted
channel). From outside this computer only the encrypted channel answers (`/api/v1/e2e`). Messages
come in English, or in Spanish if the request asks for it with `Accept-Language: es`.

`DELETE /api/v1/phones/me`: the paired phone or browser that calls it signs itself out of every
computer in the group (the web app's "Sign out").

Each Claude session in `GET /api/v1/sessions` has, in `claude`, `model` (the display name of the
model of Claude's last reply, e.g. `"Opus 5.5"`, or the one nido last set; `null` if unknown) and
`models` (the models that can be chosen, as Claude's `/model` menu showed them the last time nido
opened it, without `Default`; until then `["Opus", "Fable", "Sonnet", "Haiku"]`, which also work
as requests). `GET /api/v1/info` has `models` too. Each `screen` message of
`/api/v1/sessions/{id}/live` has `mode`, the permission mode Claude's footer shows: `"auto"`,
`"manual"`, `"acceptEdits"`, `"plan"`, `"bypassPermissions"` or `null` (not shown or not
understood, e.g. while a menu is open).

`POST /api/v1/sessions/{id}/mode` `{"mode": "plan"}` changes Claude's permission mode by pressing
shift+tab and reading the footer after each press, until it shows the mode asked for; it works
while Claude is working too, and on the way it may briefly go through other modes. Returns
`{"ok": true, "mode": "plan"}`; `400` for an unknown mode; `409` if it could not (the session is
not running, a menu or a question is on screen, the footer is not understood, or the mode is not
reachable with shift+tab).

`POST /api/v1/sessions/{id}/model` `{"model": "Opus 5.5"}` changes Claude's model for this
session only: with Claude idle, no menu on screen and nothing typed in its input box, it types
`/model`, moves the menu's cursor to that model with the arrow keys (never `Default`) and presses
`s` ("use this session only"); it never presses Enter in that menu, which would make it the
default for new sessions. A model, a label (`"Sonnet"`) or its beginning (`"Opus"`: the first
option that starts like that) can be asked for. Returns `{"ok": true, "model": "Opus 5.5"}`, with
the model as the menu shows it; `400` without a model; `409` if it could not (Claude working, a
menu or a draft on screen, the option not in the menu, no "session only" in the menu…), after
closing the menu with Esc if it was open.

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

The macOS binaries are signed with a Developer ID: macOS recognizes nido across updates and keeps
the privacy permissions you gave it (to Documents or Downloads, for instance), so sessions don't
get stuck waiting on that dialog again after each update. They are not notarized. Downloaded with
`curl`, as above, macOS lets it run; if you download it with a browser and macOS won't open it,
remove the download flag with `xattr -d com.apple.quarantine ~/.local/bin/nido`.

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
