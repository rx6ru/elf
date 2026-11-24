# ELF

Voice dictation for Linux and Hyprland. Press a key, speak, press a key, and the text is pasted
into the window where you started.

Platform: Linux, Wayland (Hyprland). License: MIT.

## What it does

`elfd` is a daemon. It records the microphone through PipeWire, sends the recording to Deepgram
for transcription, and pastes the result into the window that had focus when the dictation began.
`elf` is the command-line client that Hyprland key binds call. A small overlay shows what ELF is
doing: a waveform that follows your voice while you speak, a sloshing shape while the text is
processed, and a drop that shrinks away when the text is delivered. The overlay is drawn on the
GPU with the [Rune](https://github.com/anomalyco/rune) UI framework and a WGSL shader.

Also in the box: several API keys with round robin and failover, history, retry of audio that
could not be transcribed, paste-again, snippets, per-application vocabulary, and an option for
spoken punctuation.

## How it works

```
  Hyprland bind ──▶ elf (CLI) ──▶ elfd (daemon)
                                    │
   microphone ── pw-record ─────────┤  20 ms frames
                                    ├──▶ ffmpeg: Ogg/Opus, encoded while you speak
                                    ├──▶ Deepgram stream (optional, live text)
                                    └──▶ overlay: phase + voice level
   stop
                                    ├──▶ Deepgram request with the whole recording
                                    ├──▶ snippets, markers, trailing space
                                    └──▶ wl-copy ─▶ focus target ─▶ paste ─▶ refocus
                                         history.jsonl, spool/ (until delivered)
```

The text that gets pasted comes from the request that carries the whole recording, because it has
the full context. The live stream is off by default; with `live_stream = true` it supplies a
fallback text and live results. Design details are in [docs/architecture.md](docs/architecture.md).

## Requirements

- Linux with Hyprland 0.57 or newer, using the Lua configuration
- PipeWire (`pw-record`; `pw-play` for sounds)
- `wl-clipboard` (`wl-copy`)
- `ffmpeg` with libopus
- A [Deepgram](https://deepgram.com) API key
- A Rust toolchain (edition 2024) to build
- Optional: `grim`, for automatic overlay colour; `libnotify` (`notify-send`), for notifications

## Build and install

```sh
git clone --recurse-submodules https://github.com/rx6ru/elf.git
cd elf
cargo build --release
install -Dm755 target/release/elf target/release/elfd -t ~/.local/bin
```

The `third_party/rune` submodule holds the UI framework. If you cloned without
`--recurse-submodules`, run `git submodule update --init`.

Install and start the user service:

```sh
install -Dm644 crates/elf/elfd.service ~/.config/systemd/user/elfd.service
systemctl --user daemon-reload
systemctl --user enable --now elfd
```

The service needs `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE` in the user manager
environment. Import them when the session starts, for example from your Hyprland config:

```sh
systemctl --user import-environment WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE
```

### API keys

One key:

```sh
mkdir -p ~/.config/elf
echo 'DEEPGRAM_API_KEY=your-key' > ~/.config/elf/.env
chmod 600 ~/.config/elf/.env
```

Several keys. ELF uses them in turn and moves on when one is rejected or out of credit:

```sh
# ~/.config/elf/.env
DEEPGRAM_API_KEYS=key-one,key-two,key-three

# or one key per line, with # comments, in ~/.config/elf/deepgram_keys
```

Without a key `elfd` starts but refuses `start`. Restart `elfd` after changing keys.

## Hyprland binds

Add this to your Lua configuration:

```lua
local elf = os.getenv("HOME") .. "/.local/bin/elf"
hl.bind("SUPER + M", hl.dsp.exec_cmd(elf .. " start"))
hl.bind("SUPER + CTRL + M", hl.dsp.exec_cmd(elf .. " paste-again"))

-- elfd enters this submap while it listens and leaves it when it is done.
-- Only these binds work inside it.
hl.define_submap("elf", function()
    hl.bind("space", hl.dsp.exec_cmd(elf .. " stop"))
    hl.bind("Return", hl.dsp.exec_cmd(elf .. " stop"))
    hl.bind("escape", hl.dsp.exec_cmd(elf .. " cancel"))
    hl.bind("SUPER + M", hl.dsp.exec_cmd(elf .. " stop"))
end)
```

| Keys | Action |
|---|---|
| `SUPER+M` | Start dictation |
| `Space` or `Enter` while listening | Stop and paste |
| `Esc` while listening | Cancel; nothing is pasted or saved |
| `SUPER+M` while listening | Stop and paste |
| `SUPER+CTRL+M` | Paste the last text again |

Without the submap block ELF still starts, but only `elf stop` can end a dictation, because
Hyprland refuses to enter a submap that does not exist.

For push-to-talk, bind the start key to `elf press` and, with `{ release = true }`, to
`elf release`. A press shorter than 250 ms is a tap and leaves the dictation open; a longer press
stops on release.

## Usage

| Command | Effect |
|---|---|
| `elf start` | Start dictating into the focused window |
| `elf stop` | Stop, transcribe and paste |
| `elf toggle` | Start, or stop if listening |
| `elf press`, `elf release` | One key for tap and hold |
| `elf cancel` | Drop the recording |
| `elf paste-again` | Paste the last text into the focused window |
| `elf retry` | Send recordings that were kept after a failure again |
| `elf history [n]` | Show the last `n` dictations (10 by default) |
| `elf status` | Show the state and the key pool |
| `elf quit` | Stop the daemon |

`elfd --no-hud` runs without the overlay. `elfd --simulate file.wav --dry-run` runs one
dictation from a file at real-time speed against the real Deepgram API, prints what it would
paste, and pastes nothing.

## Configuration

`~/.config/elf/config.toml`. Every key is optional. The file is documented in
[`crates/elf/config.example.toml`](crates/elf/config.example.toml); copy it as a starting point.

| Key | Default | Meaning |
|---|---|---|
| `language` | `"en"` | `"multi"` mixes Hindi and English |
| `spoken_punctuation` | `false` | Say "comma" to get a comma. Turns automatic punctuation off |
| `paste_into` | `"original"` | Paste into the window you started in, or `"focused"` |
| `live_stream` | `false` | Also stream while you speak |
| `trailing_space` | `true` | One space after each dictation |
| `max_seconds` | `600` | A dictation stops by itself after this time |
| `mip_opt_out` | `true` | Ask Deepgram not to train on your audio |
| `[keyterms]` | | Words Deepgram should spell correctly, global and per window class |
| `[paste_keys]` | `shift+Insert` | Paste keys per window class |
| `[snippets]` | | A phrase that is replaced by a fixed text |

The `[hud]` section sets the overlay and is re-read when the file changes:

```toml
[hud]
color = "auto"        # "auto", "white" or "dark"
anchor = "bottom"     # or "top"
x_offset = 0.0        # fraction of the monitor width, -0.5 to 0.5
size = 1.0            # multiplier on the automatic size, 0.5 to 2
monitor = "focused"   # or a name such as "eDP-1"
```

With `color = "auto"` the overlay samples the screen next to it (needs `grim`) and switches
between white and near-black ink. The full list of keys and their ranges is in
[docs/behaviour.md](docs/behaviour.md).

## How text gets pasted

Wayland gives clipboard data only to the focused client. A paste key sent to a window that does
not have focus arrives empty. ELF therefore does this:

1. Copy the text to the clipboard and the primary selection with `wl-copy`.
2. If the target window is not focused, focus it.
3. Send the paste keys to it with Hyprland's `send_shortcut`, addressing the window as
   `address:0x...`.
4. Wait 120 ms for the application to read the clipboard.
5. Focus the window you were in.

The default paste keys are `Shift+Insert`, which GUI applications and terminals both accept. Set
`[paste_keys]` for a class that needs something else. The text stays on the clipboard after the
paste, so if it lands nowhere you can still paste it by hand. If the target window has closed,
ELF sends no key and tells you the text is copied.

## Privacy

- ELF sends audio to Deepgram: the whole recording over HTTPS after you stop, and a live
  WebSocket while you speak if `live_stream` is on. Nothing else leaves the machine.
- Vocabulary from `[keyterms]` is sent with each request.
- `mip_opt_out = true` (the default) asks Deepgram to exclude your audio from model training.
- On disk, under `~/.local/state/elf/`: `history.jsonl` with the transcripts of the last
  `history_days` days, `spool/` with a recording only until its text is delivered (or kept when
  transcription fails), and `elfd.log`.
- Keys are never written to the log or the history. The log names a key as `key#2`, with at most
  its last four characters.
- The control socket is in `$XDG_RUNTIME_DIR` with mode 0600.

## Troubleshooting

The daemon log is `~/.local/state/elf/elfd.log`.

**Nothing is pasted.** Run `elf status`. If it says `cannot reach elfd`, the service is not
running: `systemctl --user status elfd`. If the service lacks `HYPRLAND_INSTANCE_SIGNATURE`,
import it as shown above. The text is on the clipboard either way, and `elf history` shows it.

**The text goes in, but as the wrong characters or not at all.** The application does not accept
`Shift+Insert`. Find its class with `hyprctl -j activewindow` and set `[paste_keys]`, for example
`"org.wezfurlong.wezterm" = "ctrl+shift+v"`.

**There is no overlay.** Start `elfd` in a terminal and read the output. Without a Wayland
display or a GPU the overlay fails to start; that is logged and dictation still works. Run
`elf-hud --hold listening` (build the workspace and run `target/release/elf-hud`) to test the
overlay alone.

**Nothing is recorded, or the overlay stays flat.** Check that PipeWire has an input device and
that `pw-record` works. `target/release/elf-hud --voice-debug` prints what the voice analysis
hears; if it stays at zero, the default input is wrong or muted.

**"No Deepgram key" or "key rejected".** Check `~/.config/elf/.env` and `elf status`, which shows
how many keys are loaded and how many are cooling down. Restart `elfd` after changing keys. A
rejected key is skipped until restart.

**"Kept your audio".** Transcription failed on every attempt. The recording is in
`~/.local/state/elf/spool/`. Run `elf retry` when the network or key works again.

## Project layout

| Path | Content |
|---|---|
| `crates/elf` | The daemon `elfd`, the client `elf`, the state machine, capture, Deepgram clients, delivery, history |
| `crates/elf-hud` | The overlay: Rune compositor, WGSL shader, layout, ink colour, `[hud]` settings, and the standalone `elf-hud` binary |
| `docs/architecture.md` | Components, threads, state machine and sequences |
| `docs/behaviour.md` | Scenario table, configuration keys, data formats |
| `third_party/rune` | The Rune UI framework (git submodule) |

Run the tests with `cargo test`. The state machine is tested through fake ports, so no
microphone, network or display is needed.

## License

MIT. See [LICENSE](LICENSE).
