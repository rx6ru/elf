# Behaviour

This document says what ELF does in each situation, and lists the configuration keys and data
formats. How it is built is in [architecture.md](architecture.md).

## 1. Features

| Feature | How it works |
|---|---|
| Dictate into any app | Paste through Hyprland: focus the target, `send_shortcut`, refocus |
| Tap or hold | One key does both: a press shorter than 250 ms is a tap (toggle), a longer one stops on release |
| Cancel | `Esc` while listening, through the `elf` submap |
| Automatic punctuation, numbers, dates | Deepgram `smart_format` |
| Spoken punctuation ("comma", "new paragraph") | Option `spoken_punctuation`; turns automatic punctuation off |
| Filler removal | Deepgram default |
| Vocabulary | `keyterm`, global and per window class |
| Snippets | Exact-phrase table; the whole transcript must match |
| Paste into the window where dictation started | `paste_into = "original"` (default) |
| Never lose text | Clipboard, history, and the WAV spool until the text is delivered |
| Paste again | `elf paste-again` pastes the last text into the focused window |
| Retry failed audio | `elf retry` sends spooled recordings again |
| Several API keys | Round robin, with failover when a key is turned away |
| Hindi and English mixed | `language = "multi"` |
| Start and stop sounds | Option `sounds` |
| Overlay | Liquid waveform HUD, configurable in `[hud]` |

## 2. Hotkeys

ELF never edits the Hyprland config. The binds are shown in the README. In short:

- `SUPER+M` runs `elf start`. While listening, `SUPER+M`, `Space` and `Enter` run `elf stop`.
- `Esc` runs `elf cancel`. It is bound only while ELF listens: the daemon enters the `elf`
  submap at start and leaves it when listening ends.
- `SUPER+CTRL+M` runs `elf paste-again`.
- `elf press` and `elf release` give push-to-talk and tap-to-toggle on one key.

## 3. Normal flow

1. `start`: read the focused window (`hyprctl -j activewindow`: address and class). This is the
   target. Pick the next API key. Enter the `elf` submap. Start capture. If `live_stream` is on,
   open the stream. Show the overlay in the listening phase.
2. Frames go to the in-memory recording, the voice analysis, the Opus encoder and, if it is up,
   the stream. Frames recorded before the stream opens are sent in one burst when it does.
3. `stop`: stop capture, leave the submap, finish the Opus encoder, start pass 2 and flush the
   stream in parallel. The overlay shows the processing phase.
4. Pass 2 uploads the whole recording with the same options as the stream. Timeout: 6 s plus
   0.3 s per second of audio.
5. Text is pass 2 if it is not empty, otherwise pass 1. Snippets and markers are applied, the
   text is trimmed, and one trailing space is added when `trailing_space` is on.
6. Deliver: copy to the clipboard and primary selection, focus the target if it is not focused,
   send the paste keys to it, wait 120 ms, focus the previous window again. About 0.3 s in total.
7. Append to `history.jsonl`, delete the spooled WAV. The overlay shows the done phase and hides.

Why the focus step: Wayland serves clipboard data only to the focused client, so a paste sent to
an unfocused window arrives empty. The target window must be written `address:0x...`; a bare
`0x...` fails with "window not found" on Hyprland 0.57.2. `Shift+Insert` works in GUI apps and
terminals; `paste_keys` overrides it per window class.

## 4. Scenarios

Every row has a test in `crates/elf/src/machine/tests.rs` (or `deliver.rs`, `control.rs`).

| # | Situation | Result |
|---|---|---|
| 1 | Tap while idle | Start (toggle mode) |
| 2 | Tap while listening | Stop and process |
| 3 | Hold 250 ms or longer, then release | Push-to-talk: stop on release |
| 4 | `start` while already listening | Ignored |
| 5 | `start` while the previous dictation is still processing | A new dictation starts. The old one finishes and pastes into its own target |
| 6 | `cancel` while listening | Stop, drop the audio, no paste, no history entry. The overlay hides |
| 7 | Recording shorter than 0.4 s | Discarded quietly. The overlay shows "nothing heard". No request is sent |
| 7b | Recording with no detected voice activity | Still sent to pass 2: a noisy microphone can hide speech from the detector, and Deepgram decides |
| 8 | Recording reaches `max_seconds` | Stops by itself and is processed |
| 9 | The stream fails to connect or drops | Recording continues. One retry after 250 ms replays the recording. If that fails, carry on without live text. Pass 2 still runs |
| 10 | The stream returns no words | Pass 2 decides |
| 11 | Pass 2 times out, returns 5xx or 429 | One retry after 500 ms. Then pass 1 text, if there is any |
| 12 | Both passes empty or failed | The WAV stays in the spool. History entry with `text: null`. Notification: "Kept your audio. Run `elf retry`" |
| 13 | Offline at start | Records anyway. At stop pass 2 fails fast: scenario 12 |
| 14 | A key is rejected (401, 403) or has no credit (402) | The request goes to the next key. When every key is out, one notification, the WAV is kept, no more retries |
| 15 | No key configured | `elfd` starts but refuses `start` with a notification that says where to put a key |
| 16 | No microphone, or `pw-record` fails | Error notification and error overlay. Nothing else changes |
| 17 | Capture ends early (suspend, device unplugged) | Treated as a stop. What was recorded is processed |
| 18 | The target window closed before delivery | No key is sent. The text stays on the clipboard and in history. Notification: "Copied. Paste with Super+Ctrl+M" |
| 19 | Focus moved during the dictation | Pasted into the original window. With `paste_into = "focused"`, into the window focused when the text is ready |
| 20 | The app ignores `Shift+Insert` | Per-class `paste_keys`, for example `ctrl+shift+v`. The text is on the clipboard anyway |
| 21 | A second `elfd` starts | It finds the live socket and exits with a message |
| 22 | `elfd` crashes or is killed | systemd restarts it. Spooled WAVs survive. `elf retry` lists them and sends them again to the focused window |
| 23 | Very long text | Delivered like any other: a clipboard paste does not depend on length |
| 24 | The whole transcript is a snippet phrase | Replaced by the snippet text |

## 5. API keys

Keys are read, merged in this order, trimmed, without empty entries or repeats:

1. The process environment, then `~/.config/elf/.env`, then `./.env`, then the repository's
   `.env` (for development). Each file can mix these lines:
   `DEEPGRAM_API_KEY=k1`, `DEEPGRAM_API_KEYS=k1,k2,k3` (comma or space separated),
   `DEEPGRAM_API_KEY_2=k2` (and `_3`, `_4`, and so on).
2. `~/.config/elf/deepgram_keys`: one key per line. `#` starts a comment.

Each dictation takes the next key in turn. Its stream, its pass 2 request and its retries all use
that key. `elf retry` of a saved recording takes the next one. When Deepgram turns a key away,
the same request goes at once to the next key that is not out:

| Status | The key is out for |
|---|---|
| 401, 403 (bad key) | Until `elfd` restarts |
| 402 (no credit) | 10 minutes |
| 429 (too many requests) | 60 seconds |

The rotation lives in memory only. `elf status` ends with a line such as
`keys: 3 (1 cooling down)`. Logs name a key as `key#2` with at most its last four characters,
and only for keys of 12 characters or more.

## 6. Configuration

File: `~/.config/elf/config.toml`. Every key is optional. A missing file means all defaults;
a file that does not parse is an error at start. [`config.example.toml`](../crates/elf/config.example.toml)
documents every key.

| Key | Default | Meaning |
|---|---|---|
| `language` | `"en"` | `"multi"` mixes Hindi and English |
| `spoken_punctuation` | `false` | Say "comma" to get a comma. Turns automatic punctuation off |
| `trailing_space` | `true` | One space after each dictation |
| `paste_into` | `"original"` | `"original"` or `"focused"` |
| `max_seconds` | `600` | A dictation stops by itself after this time |
| `sounds` | `false` | Start and stop sounds (`pw-play`) |
| `history_days` | `30` | Days of history kept |
| `mip_opt_out` | `true` | Ask Deepgram not to use the audio for model training |
| `live_stream` | `false` | Also stream while you speak. Off: only the whole recording is sent, which is more accurate and costs half |
| `[keyterms] always` | `[]` | Words Deepgram should spell correctly (100 at most in total) |
| `[keyterms.apps]` | | Extra words while a window of that class has focus |
| `[paste_keys]` | `shift+Insert` | Paste keys per window class |
| `[snippets]` | | Phrase to text. Case and punctuation do not matter |

`[hud]`:

| Key | Default | Meaning |
|---|---|---|
| `color` | `"auto"` | `"auto"`, `"white"` or `"dark"` |
| `anchor` | `"bottom"` | `"bottom"` or `"top"` |
| `x_offset` | `0.0` | Shift from the centre, as a fraction of the monitor width (-0.5 to 0.5) |
| `y_offset` | unset | Gap to the edge, as a fraction of the monitor height (0 to 0.5). Unset: 0.065, clamped to 24 to 160 px |
| `size` | `1.0` | Multiplier on the automatic size (0.5 to 2) |
| `scale_exponent` | `0.5` | How fast size grows with the screen (0 to 1) |
| `monitor` | `"focused"` | `"focused"` or a monitor name such as `eDP-1` |

`[hud]` is re-read when the file changes. Values outside their range are pulled into it and
noted in `elfd.log`. A mistake keeps the last good settings and sends one notification that
names the line.

## 7. Data

Locations follow the XDG base directories.

| Path | Content |
|---|---|
| `~/.config/elf/config.toml` | Settings |
| `~/.config/elf/.env`, `~/.config/elf/deepgram_keys` | API keys |
| `$XDG_RUNTIME_DIR/elf.sock` | Control socket, mode 0600 |
| `~/.local/state/elf/history.jsonl` | One line per dictation |
| `~/.local/state/elf/spool/<id>.wav` | A recording, only while its text is not delivered |
| `~/.local/state/elf/elfd.log` | Events and timings. Never a key, never audio |

`history.jsonl` entries are pruned after `history_days`. Each line:

```json
{"id":"1700000000000","ts":"2026-01-15T14:05:11+05:30","app":"kitty","target":"0x5588aa","ms_audio":8420,"ms_to_text":610,"pass1":null,"pass2":"Okay, so the plan is simple.","text":"Okay, so the plan is simple.","chosen":"pass2","error":null,"wav":null}
```

| Field | Meaning |
|---|---|
| `id` | Unix time in ms when the dictation began; also the spool file name |
| `ts` | The same moment, local time |
| `app`, `target` | Class and address of the target window |
| `ms_audio`, `ms_to_text` | Recording length; time from stop to text |
| `pass1`, `pass2` | What each pass returned |
| `text`, `chosen` | The text that was delivered and which pass it came from (`pass1` or `pass2`) |
| `error` | Why there is no text |
| `wav` | The spooled recording, while the text is not delivered |

### Control protocol

One line in, one reply out, then the connection closes. The reply starts with `ok` or `err`.
`status` answers `ok idle`, `ok listening` or `ok processing <n>`, then a line with the key
summary. `history [n]` answers `ok <count>` and one line per entry.
