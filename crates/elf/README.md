# elf

The dictation daemon `elfd` and its client `elf`. Setup, key binds and configuration are in the
[repository README](../../README.md); the design is in [docs/architecture.md](../../docs/architecture.md)
and the behaviour in [docs/behaviour.md](../../docs/behaviour.md).

```sh
cargo build -p elf --release     # target/release/elfd and target/release/elf
cargo test -p elf
```

## Modules

| Module | Job |
|---|---|
| `machine` | The state machine. Owns all dictation state; the outside world is reached through port traits. |
| `control` | Unix socket `$XDG_RUNTIME_DIR/elf.sock`, command parsing, the client request. |
| `audio` | `capture` (`pw-record`, 20 ms frames), `encode` (Ogg/Opus while recording, through ffmpeg), `wav` (44-byte header). |
| `vad` | Voice activity, level, bands and onsets, from `elf-hud`. `Vad` is a trait, so the detector can be replaced. |
| `stt` | Deepgram: `stream` (pass 1, WebSocket), `batch` (pass 2, HTTPS), request parameters and JSON parsing. |
| `text` | Snippets, dictation markers, trim, trailing space. |
| `deliver` | `wl-copy`, then focus the target, send the paste keys, refocus the previous window. |
| `hypr` | `hyprctl` calls and output parsing. One place for the window selector format. |
| `desktop` | Focused window, the `elf` submap, `notify-send`, start and stop sounds. |
| `history` | `history.jsonl` and `spool/*.wav`. |
| `keys` | Key sources, round robin and per-key health. |
| `hud` | `Hud` trait. `OverlayHud` drives the `elf-hud` overlay; `LogHud` only logs. |
| `daemon`, `simulate` | Wiring for `elfd` and for `elfd --simulate`. |

The main thread runs the overlay's event loop. The state machine, control socket, capture, stream,
batch and delivery run on their own threads and talk to the machine through one event channel.

## Testing

The state machine is tested with fake ports and a controllable clock; each row of the scenario
table in [docs/behaviour.md](../../docs/behaviour.md) has a test. `deliver`, `keys`, `config`,
`text` and `history` have unit tests of their own. The microphone, `pw-play` and the real paste
path need a desktop session and are not exercised by `cargo test`.

## Test hooks

- `ELF_MIC_FILE=<file>`: capture decodes that file with ffmpeg in real time (once, no loop)
  instead of using `pw-record`. The dictation ends when the file ends.
- `elfd --simulate <file.wav> --dry-run`: runs one dictation from a file at real-time speed with
  the real Deepgram stream and pass 2, prints what it would paste, and pastes nothing.
- `elfd --no-hud`: runs without the overlay.
