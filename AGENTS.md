# AGENTS.md

This file provides guidance to AI coding assistants working with code in this repository.

## Development Commands

**Prerequisites:**

- [Rust](https://rustup.rs/) (latest stable)
- [Bun](https://bun.sh/) package manager

**Core Development:**

```bash
# Install dependencies
bun install

# Run in development mode
bun run tauri dev
# If cmake error on macOS:
CMAKE_POLICY_VERSION_MINIMUM=3.5 bun run tauri dev

# Build for production
bun run tauri build

# Frontend only development
bun run dev        # Start Vite dev server
bun run build      # Build frontend (TypeScript + Vite)
bun run preview    # Preview built frontend
```

**Linting and Formatting (run before committing):**

```bash
bun run lint              # ESLint for frontend
bun run lint:fix          # ESLint with auto-fix
bun run format            # Prettier + cargo fmt
bun run format:check      # Check formatting without changes
bun run format:frontend   # Prettier only
bun run format:backend    # cargo fmt only
```

**Model Setup (Required for Development):**

```bash
mkdir -p src-tauri/resources/models
curl -o src-tauri/resources/models/silero_vad_v4.onnx https://blob.handy.computer/silero_vad_v4.onnx
```

For detailed platform-specific build setup, see [BUILD.md](BUILD.md).

## Architecture Overview

Handy is a cross-platform desktop speech-to-text application built with Tauri 2.x (Rust backend + React/TypeScript frontend).

### Backend Structure (src-tauri/src/)

- `lib.rs` - Main entry point, Tauri setup, manager initialization
- `managers/` - Core business logic:
  - `audio.rs` - Audio recording and device management
  - `model.rs` - Model downloading and management
  - `transcription.rs` - Speech-to-text processing pipeline
  - `history.rs` - Transcription history storage
- `audio_toolkit/` - Low-level audio processing:
  - `audio/` - Device enumeration, recording, resampling
  - `vad/` - Voice Activity Detection (Silero VAD)
- `commands/` - Tauri command handlers for frontend communication
- `cli.rs` - CLI argument definitions (clap derive)
- `shortcut.rs` - Global keyboard shortcut handling
- `settings.rs` - Application settings management
- `overlay.rs` - Recording overlay window (platform-specific)
- `signal_handle.rs` - `send_transcription_input()` reusable function
- `server/` - Networked inference server (OpenAI-compatible `/v1/audio/transcriptions` + Handy streaming extensions over SSE)
- `remote_client.rs` - Client side of networked inference: offload transcription to a remote server
- `utils.rs` - Platform detection helpers

### Frontend Structure (src/)

- `App.tsx` - Main component with onboarding flow
- `components/` - React UI components:
  - `settings/` - Settings UI
  - `model-selector/` - Model management interface
  - `onboarding/` - First-run experience
  - `overlay/` - Recording overlay UI
  - `update-checker/` - App update notifications
  - `shared/`, `ui/`, `icons/`, `footer/` - Shared components
- `hooks/useSettings.ts` - Settings state management hook
- `stores/settingsStore.ts` - Zustand store for settings
- `bindings.ts` - Auto-generated Tauri type bindings (via tauri-specta)
- `overlay/` - Recording overlay window entry point
- `lib/types.ts` - Shared TypeScript type definitions

### Key Architecture Patterns

**Manager Pattern:** Core functionality organized into managers (Audio, Model, Transcription) initialized at startup and managed via Tauri state.

**Command-Event Architecture:** Frontend → Backend via Tauri commands; Backend → Frontend via events.

**Pipeline Processing:** Audio → VAD → Whisper/Parakeet → Text output → Clipboard/Paste

**State Flow:** Zustand → Tauri Command → Rust State → Persistence (tauri-plugin-store)

### Technology Stack

**Core Libraries:**

- `transcribe-cpp` - Local Whisper-family inference (GGML/GGUF) with GPU acceleration
- `transcribe-rs` - ONNX speech recognition (Parakeet, Moonshine, SenseVoice, etc.)
- `cpal` - Cross-platform audio I/O
- `vad-rs` - Voice Activity Detection
- `rdev` - Global keyboard shortcuts
- `rubato` - Audio resampling
- `rodio` - Audio playback for feedback sounds

### Application Flow

1. **Initialization:** App starts minimized to tray, loads settings, initializes managers
2. **Model Setup:** First-run downloads preferred Whisper model (Small/Medium/Turbo/Large)
3. **Recording:** Global shortcut triggers audio recording with VAD filtering
4. **Processing:** Audio sent to Whisper model for transcription
5. **Output:** Text pasted to active application via system clipboard

### Settings System

Settings are stored using Tauri's store plugin with reactive updates:

- Keyboard shortcuts (configurable, supports push-to-talk)
- Audio devices (microphone/output selection)
- Model preferences (Small/Medium/Turbo/Large Whisper variants)
- Audio feedback and translation options

### Networked Inference (Server / Client Modes)

A machine with a dedicated GPU can serve its transcription engine to machines
without one. The two halves are independent: `inference_mode` decides where _this_
install transcribes, `server_enabled` decides whether it _also_ serves.

**Server** (`server/`): an axum listener with two layers on one router.

- `POST /v1/audio/transcriptions`, `POST /v1/audio/translations`, `GET /v1/models` —
  OpenAI-compatible, so any client that speaks that API works, and Handy's client
  mode works against whisper.cpp-server / faster-whisper / OpenAI.
- `GET /handy/v1/info` and `POST /handy/v1/stream…` — Handy extensions: capability
  discovery and the streaming session.
- `GET /health` — unauthenticated liveness probe, registered outside the auth layer.

Every other route requires `Authorization: Bearer <token>`; the token is generated
on first enable and compared in constant time. Inference is serialised through a
one-permit semaphore because `TranscriptionManager` owns a single engine.

**Streaming** uses two plain-HTTP channels rather than a WebSocket: audio goes up
as chunked `POST`s of 16 kHz mono little-endian i16 PCM, and partial text comes
down over SSE. A session holds the engine permit for its whole life, so an idle
watchdog cancels abandoned sessions.

**Client** (`remote_client.rs`): `TranscriptionManager::transcribe_with` returns
early to the remote path before any local engine check, so a client machine needs
no model downloaded. Streaming is driven by `run_remote_stream_worker`, which
reuses the same command channel and guards as the local worker.

**Key design decisions:**

- Only WAV uploads are decoded — Handy links no mp3/flac decoder, so an unreadable
  container is a 415 rather than garbage text.
- Post-processing runs on both sides: the server applies its own settings, the
  client applies the dictating user's (custom words, filler removal). Both
  transforms are effectively idempotent.
- `TranscribeOverrides::force_local` terminates the remote → local fallback and
  stops a server that is itself a client from proxying requests onward.
- In client mode, streaming eligibility comes from `client_streaming`, not from a
  local model capability probe (`actions.rs`).

## Single Instance Architecture

The app enforces single instance behavior — launching when already running brings the settings window to front rather than creating a new process. Remote control flags (`--toggle-transcription`, etc.) work by launching a second instance that sends args to the running instance via `tauri_plugin_single_instance`, then exits.

## Internationalization (i18n)

All user-facing strings must use i18next translations. ESLint enforces this (no hardcoded strings in JSX).

**Adding new text:**

1. Add key to `src/i18n/locales/en/translation.json`
2. Use in component: `const { t } = useTranslation(); t('key.path')`

**File structure:**

```
src/i18n/
├── index.ts           # i18n setup
├── languages.ts       # Language metadata
└── locales/
    ├── en/translation.json  # English (source)
    ├── de/, es/, fr/, ja/, ru/, zh/, ...
    └── ...
```

For translation contribution guidelines, see [CONTRIBUTING_TRANSLATIONS.md](CONTRIBUTING_TRANSLATIONS.md).

## Code Style

**Rust:**

- Run `cargo fmt` and `cargo clippy` before committing
- Handle errors explicitly (avoid unwrap in production)
- Use descriptive names, add doc comments for public APIs

**TypeScript/React:**

- Strict TypeScript, avoid `any` types
- Functional components with hooks
- Tailwind CSS for styling
- Path aliases: `@/` → `./src/`

## CLI Parameters

Handy supports command-line parameters on all platforms for integration with scripts, window managers, and autostart configurations.

**Implementation:** `cli.rs` (definitions), `main.rs` (parsing), `lib.rs` (applying), `signal_handle.rs` (shared logic)

| Flag                     | Description                                                |
| ------------------------ | ---------------------------------------------------------- |
| `--toggle-transcription` | Toggle recording on/off on a running instance              |
| `--toggle-post-process`  | Toggle recording with post-processing on/off               |
| `--cancel`               | Cancel the current operation on a running instance         |
| `--start-hidden`         | Launch without showing the main window (tray icon visible) |
| `--no-tray`              | Launch without system tray (closing window quits the app)  |
| `--debug`                | Enable debug mode with verbose (Trace) logging             |

**Key design decisions:**

- CLI flags are runtime-only overrides — they do NOT modify persisted settings
- Remote control flags work via `tauri_plugin_single_instance`: second instance sends args, then exits
- `send_transcription_input()` in `signal_handle.rs` is shared between signal handlers and CLI

## Debug Mode

Access debug features: `Cmd+Shift+D` (macOS) or `Ctrl+Shift+D` (Windows/Linux)

## Platform Notes

- **macOS**: Metal acceleration, accessibility permissions required for keyboard shortcuts
- **Windows**: Vulkan acceleration, code signing
- **Linux**: OpenBLAS + Vulkan, limited Wayland support, overlay uses GTK layer shell (disable with `HANDY_NO_GTK_LAYER_SHELL=1`)

## Troubleshooting

See the [Troubleshooting](README.md#troubleshooting) section in README.md.

## GitHub workflow for AI coding assistants

**MANDATORY. Before opening any PR, issue, or discussion in this repo: you MUST read the relevant template file and follow it strictly.** That includes sections that look "ceremonial" — checklists, AI Assistance disclosures, "Human Written Description". A generic Summary/Test-plan layout is not acceptable.

- **Opening a PR:** Read [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md). Every section listed there is mandatory. If a section requires a human-written paragraph (e.g. "Human Written Description"), leave a clear TODO placeholder and ask the human contributor to fill it in — do not invent their voice.
- **Opening an issue:** Read [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE/). Blank issues are disabled; pick the right template (`bug_report.md` for bugs). Feature requests do not belong in issues — they go to [Discussions](https://github.com/cjpais/Handy/discussions) (see `.github/ISSUE_TEMPLATE/config.yml`).
- **Proposing a feature:** Handy is under a feature freeze. New features require community support gathered in [Discussions](https://github.com/cjpais/Handy/discussions) before any PR is opened — see the PR template's "Community Feedback" section.
- **Translations:** Follow [CONTRIBUTING_TRANSLATIONS.md](CONTRIBUTING_TRANSLATIONS.md).
- **Full contributor workflow:** [CONTRIBUTING.md](CONTRIBUTING.md).

**Commits:** Use conventional commit prefixes (`feat:`, `fix:`, `docs:`, `refactor:`, `chore:`). Focus the message on _why_, not _what_.
