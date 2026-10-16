# VerbaRelay

VerbaRelay turns the computers you already own into one transcription
service. A small **server** accepts audio over an authenticated HTTP API and
hands every job to the fastest free **peer**: a background client running on a
desktop, laptop or home server, transcribing with
[Whisper](https://github.com/openai/whisper) through
[whisper.cpp](https://github.com/ggml-org/whisper.cpp) on an NVIDIA GPU (CUDA),
any other GPU (Vulkan) or the CPU.

Audio is encrypted for exactly one peer, never written to disk on either side,
and only that peer can decrypt it.

* [How it works](#how-it-works)
* [Security](#security)
* [Server setup](#server-setup)
* [Client setup](#client-setup)
* [API](#api)
* [Configuration reference](#configuration-reference)
* [Statistics and logs](#statistics-and-logs)
* [Updates](#updates)
* [Building from source](#building-from-source)
* [Releasing](#releasing)
* [Repository layout](#repository-layout)

## How it works

1. **Enrollment.** On its first start a client asks for the server address, the
   server's master password and the compute backend. It creates an X25519 key
   pair, sends the public key and receives a personal token. The master
   password is used only for this step; every device has its own token.
2. **Scoring.** The client tells the server which hardware it has (for example
   "NVIDIA GeForce RTX 4070 via CUDA" or "AMD Ryzen 7 9700X via CPU") and
   transcribes a bundled speech sample with the server's model. The measured
   speed, in multiples of real time, becomes its **transcription score**. The
   score is refined with every real job (moving average).
3. **Dispatching.** For each request the server picks the free peer with the
   highest score, asks it whether it is available ("offer"), and only after the
   peer accepts streams the encrypted audio to it. A peer works on one job at a
   time; while it is busy, the next best peer is asked.
4. **Results.** The peer sends each finished segment back immediately (live
   streaming to the caller is optional) and the full result at the end. If a
   peer declines, times out or disconnects, the job moves to the next peer.
5. **Long recordings** (10 minutes or more by default) are cut at pauses and
   transcribed in parallel on several idle peers, then put back together with
   correct timestamps.

One client executable supports CUDA, Vulkan and the CPU. The compute backends
are separate native modules that are loaded at runtime; missing modules are
skipped instead of crashing the client. The CUDA runtime is several hundred MB,
so it is a separate download that the setup offers only on NVIDIA systems. All
native code runs in a separate worker process, so even a crashing GPU driver
cannot take the client down; after repeated GPU failures the client falls back
to the CPU on its own.

## Security

| Layer | Protection |
|---|---|
| Transport | TLS 1.3/1.2 (rustls) between all parties. Plain HTTP only with explicit opt-in for testing. |
| Audio | Per job: ephemeral X25519 key exchange with the selected peer, HKDF-SHA256, ChaCha20-Poly1305. Job id, chunk number and end marker are authenticated, so chunks cannot be replayed, reordered or truncated. Only the selected peer can decrypt. |
| Results | Encrypted the same way with a separate key from the peer back to the server. |
| At rest | Audio is decoded and held in memory only, never written to disk, and wiped after use. Transcripts are not stored on the server. |
| Peers | Personal tokens, stored on the server only as SHA-256 hashes. Tokens of peers that have not connected for 14 days are removed automatically. Peers can be revoked at any time. |
| API callers | Separate API tokens (hashed), optional expiry, per-token rate limits and a limit of concurrent jobs. |
| Master password | Can be stored as an Argon2id hash; enrollment attempts are rate-limited per address. |
| Updates | Every update and the CUDA package is verified with a minisign signature before use. |
| Logs | Tokens are redacted; audio and transcript text are never logged. |

Note: the server necessarily sees the audio it receives from API callers. It
keeps it in memory only as long as a job runs.

## Server setup

The server runs on Linux.

```sh
# 1. Install the binary (from a release archive or `cargo build --release -p verbarelay-server`)
sudo install -m 755 verbarelay-server /usr/local/bin/
sudo useradd --system --home /var/lib/verbarelay --shell /usr/sbin/nologin verbarelay

# 2. Configuration
sudo verbarelay-server --config /etc/verbarelay/server.toml init-config
sudo verbarelay-server hash-password      # put the output into master_password_hash
sudoedit /etc/verbarelay/server.toml      # TLS certificate, model, limits
sudo verbarelay-server --config /etc/verbarelay/server.toml check-config

# 3. Service
sudo cp verbarelay-server.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now verbarelay-server

# 4. A token for whoever submits audio
sudo -u verbarelay verbarelay-server --config /etc/verbarelay/server.toml tokens create my-app
```

TLS: either point `[tls]` at a certificate (for example from Let's Encrypt) or
remove the section and run the server behind a reverse proxy that terminates
TLS (Caddy, nginx). Behind a proxy, set `trust_forwarded_for = true` so rate
limits see real client addresses, and make sure the proxy forwards WebSocket
upgrades for `/api/v1/peers/connect`.

### Docker

The repository contains a `Dockerfile` for the server and an example
`compose.yaml`. The configuration directory is mounted read-only at
`/etc/verbarelay`, the database lives in a volume at `/var/lib/verbarelay`.

```sh
cp config/server.example.toml config/server.toml
$EDITOR config/server.toml   # master password; TLS files in ./config or no [tls] behind a proxy
docker compose up -d
docker compose exec server verbarelay-server tokens create my-app
```

The container runs as uid 10001, so the files in `./config` must be readable
by it. Logs go to stdout (`docker compose logs`) and to `/var/lib/verbarelay/logs`.

## Client setup

### Windows

1. Extract `verbarelay-client-<version>-windows-x86_64.zip` into a folder you
   can write to, for example `%LOCALAPPDATA%\Programs\verbarelay`.
2. Start `verbarelay-client.exe`. The setup window asks for the server
   address, the master password and the device name, detects the graphics
   hardware, measures CPU and GPU speed with a small test model, and
   preselects the recommended backend. Choosing CUDA downloads the CUDA
   package.
3. From then on the client runs in the background and starts with Windows.
   Starting the executable again opens the settings window.

The windows use native controls with labelled fields and are fully usable with
screen readers such as NVDA.

### Linux desktop

Same as Windows: extract the archive (for example to
`~/.local/share/verbarelay-client`) and run `verbarelay-client`. The
client registers itself in `~/.config/autostart`.

### Linux without a desktop

```sh
verbarelay-client setup                  # interactive, in the terminal
# or unattended:
VERBARELAY_MASTER_PASSWORD=... verbarelay-client setup \
    --server https://transcribe.example.com:8443 --backend cuda --name gpu-box

# run as a systemd user service (see packaging/systemd/verbarelay-client.service)
systemctl --user enable --now verbarelay-client
```

### Commands

| Command | Purpose |
|---|---|
| `verbarelay-client` | Run in the background (set up on first start; opens the settings if already running) |
| `verbarelay-client setup` | Set up this device again |
| `verbarelay-client settings` | Update interval and development builds |
| `verbarelay-client status` | Status of the running client |
| `verbarelay-client reload` | Reload the configuration file |
| `verbarelay-client benchmark` | Measure the transcription score |
| `verbarelay-client autostart enable/disable` | Start with the session or not |

Add `--headless` to any command to stay in the terminal.

## API

All calls need `Authorization: Bearer <api token>`.

### `POST /api/v1/transcriptions`

`multipart/form-data` fields:

| Field | Default | Meaning |
|---|---|---|
| `file` | required | Audio in any common format (WAV, FLAC, MP3, M4A/AAC, Ogg Vorbis, ...) |
| `language` | `auto` | Language code such as `de` or `en` |
| `translate` | `false` | Translate into English |
| `model` | server default | Another allowed model; see the model policy below |
| `format` | `json` | `json`, `text`, `srt` or `vtt` |
| `stream` | `false` | Server-Sent Events with live segments |
| `split` | `true` | Allow splitting long recordings across peers |

```sh
curl -H "Authorization: Bearer $TOKEN" \
     -F file=@meeting.m4a -F language=de -F format=srt \
     https://transcribe.example.com:8443/api/v1/transcriptions
```

JSON response:

```json
{
  "id": "4097b752-a71c-4511-a5b6-d07cda889ded",
  "text": "And so, my fellow Americans, ask not what your country can do for you ...",
  "language": "en",
  "model": "large-v3-turbo",
  "duration_ms": 11000,
  "words": 22,
  "parts": 1,
  "segments": [{ "start_ms": 0, "end_ms": 10560, "text": " And so, my fellow Americans, ..." }],
  "notices": []
}
```

With `stream=true` the response is an event stream: `queued`, `notice`,
`assigned` (which peer took which part), `segment` (live text with
timestamps), and finally `done` (the JSON above) or `error`. Segments of split
recordings can arrive out of order across parts; `done` is always in order.

**Model policy.** `[models] enforce = true` makes every peer use the server's
model. With `allow_request_override = true` a request may pick another model
from `allowed`; peers download it on first use. With
`allow_request_override = false` the requested model is ignored and the
response says so in `notices` (JSON and stream) or in the
`X-VerbaRelay-Notice` header (text, SRT, VTT).

Errors are JSON (`{"error": "..."}`) with status 400 (bad input), 401 (token),
429 (rate limit; see `Retry-After`) or 502 (no peer could finish the job).

### `GET /api/v1/status`

Connected peers with backend, score and state, plus the model policy.

### `GET /api/v1/health`

Unauthenticated liveness probe.

## Configuration reference

* Server: [`config/server.example.toml`](config/server.example.toml), every option commented.
* Client: [`config/client.example.toml`](config/client.example.toml). The only
  user settings are the update interval and whether to install development
  builds; everything else is written by the setup.

## Statistics and logs

```sh
verbarelay-server stats --days 7        # add --json for machine-readable output
verbarelay-server peers list
verbarelay-server peers revoke <id>
verbarelay-server tokens list
```

Statistics include jobs, failures, rejected model overrides, transcribed audio
minutes and words (total, per job, per audio minute), waiting time for a free
peer, offer response times, transfer times, processing times, time to the first
segment, speed relative to real time, and per peer: device, score, online time
in percent, finished and failed parts.

Logs are rotated daily: the server writes to `<data_dir>/logs` (or
`[logging] dir`) and to stdout, in text or JSON. Clients log to
`%LOCALAPPDATA%\verbarelay\data\logs` on Windows and
`~/.local/share/verbarelay/logs` on Linux.

## Updates

The client updates itself without any user interaction: it checks GitHub
releases on the configured interval, downloads in the background without a
progress bar, verifies the minisign signature, waits until no job is running,
swaps the files and restarts. Stable builds follow version tags; with
"development builds" enabled the client follows the rolling `dev` release
instead. If the CUDA package is installed, it is updated together with the
client.

## Building from source

Requirements: Rust (stable), CMake 3.28+, a C/C++ compiler. Optional: Vulkan
SDK, CUDA Toolkit 12. On Linux the GUI needs `libgtk-3-dev`.

```sh
# Native engine (whisper.cpp with runtime-loadable backends)
cmake -S engine -B build/engine -DCMAKE_BUILD_TYPE=Release -DVR_VULKAN=ON -DVR_CUDA=ON
cmake --build build/engine --parallel
cmake --install build/engine --prefix dist/engine

# Rust
cargo build --release -p verbarelay-server
cargo build --release -p verbarelay-client                       # with setup and settings windows
cargo build --release -p verbarelay-client --no-default-features # terminal only

# The client loads the engine from its own directory:
cp dist/engine/lib/* target/release/        # Linux
copy dist\engine\bin\*.dll target\release\  # Windows
```

Tests: `cargo test --workspace --no-default-features`.

Local builds have automatic updates disabled; they are enabled by setting
`VERBARELAY_UPDATE_REPO` and `VERBARELAY_UPDATE_PUBKEY` at build time, as
the release workflow does.

## Releasing

1. Create a minisign key pair: `minisign -G -s release.key -p release.pub`.
2. Repository secret `MINISIGN_SECRET_KEY`: contents of `release.key`.
   Repository secret `MINISIGN_PASSWORD`: its password (omit for a key created with `-W`).
   Repository variable `MINISIGN_PUBLIC_KEY`: the second line of `release.pub`.
3. Bump `version` in `Cargo.toml`, then tag: `git tag v0.2.0 && git push --tags`.

Every push to `main` publishes a development build. Release assets:

| Asset | Contents |
|---|---|
| `verbarelay-client-<v>-windows-x86_64.zip` | Client, engine, CPU and Vulkan backends |
| `verbarelay-client-<v>-linux-x86_64.zip` | Same for Linux |
| `verbarelay-cuda-<v>-<os>-x86_64.zip` | CUDA backend with CUDA runtime and cuBLAS |
| `verbarelay-server-<v>-linux-x86_64.tar.gz` | Server, example configuration, systemd unit |
| `*.minisig` | Signatures |

## Repository layout

```
crates/protocol   messages, per-job encryption, audio frames, model catalog
crates/server     API, peer WebSocket, scheduler, SQLite, statistics, admin commands
crates/client     background agent, worker process, setup and settings windows, updater
engine/           C ABI over whisper.cpp, built with runtime-loadable ggml backends
config/           commented example configurations
packaging/        systemd units
scripts/          release packaging
.github/          CI and release workflows
Dockerfile        server container image (compose.yaml: example deployment)
```

## License

MIT, see [LICENSE](LICENSE). Third-party components: [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
