# Fork hi-res mix engine: study and plan

Status: **Phases 0–3 done (Phase 3 = shared mode, not yet tried in the app); Phase 5 UI in progress; branch `fork/hires-engine`** · Written 2026-10-29 against fork `main` at upstream 1.56.0.

Goal: crossfade, AutoDJ and gapless transitions between tracks with different sample rates (44.1 / 48 / 88.2 / 96 / 176.4 / 192 kHz) and bit depths (16 / 24-bit) **without downsampling, without truncating bit depth, and without being forced to 44.1 kHz**. Outside transitions, playback should be bit-perfect when no DSP is active.

---

## 0. Summary

**What is wrong today (verified in code):**

1. With Hi-Res on and crossfade on, **every** track is resampled to the "blend rate", even 192 → 192 kHz. The blend rate is only 44.1, 88.2 or 96 kHz, and anything else falls back to 44.1 (`hi_res_blend.rs:19-37`, `commands.rs:353-358`).
2. All resampling is **linear interpolation, with sample-dropping when downsampling and no anti-alias filter**. This applies both to rodio's mixer and to the `UniformSourceIterator` that the blend path uses. A 192 → 44.1 kHz conversion aliases.
3. On Windows, cpal 0.17.3 only opens WASAPI in **shared mode with `AUTOCONVERTPCM`**, so Windows resamples everything again to the "Default Format" set in Sound settings. The comment "bit-perfect, no resampler" at `commands.rs:465` is not true on Windows.
4. A crossfade across rates can cost **≥ 650 ms of dropout**: the stream is reopened (150 ms sleep plus 500 ms prefill), and it only works if the outgoing track is cached. A gapless chain across rates **reopens mid-track** and replays the current track from cache.
5. A latent bug: in a crossfade between different rates, the outgoing fade length is computed with the **incoming** track's rate (`commands.rs:416` overwrites `current_sample_rate` before `sink_swap.rs:121` reads it). Going 192 → 48 kHz without Hi-Res, the outgoing track fades 4× too fast.

6. rodio's converter, which handles every rate mismatch today, **slips about one sample each time it restarts at a decoder packet** (every ~93 ms for FLAC), so the output gains or loses samples and steps slightly. Measured in Phase 0 (§9).
7. Each new rodio Player misreads the first ~511 samples of its first track (R12), because its empty queue reports itself as mono 48 kHz. Found in Phase 2; the fork crate fixes it with `priming_silence`.

**What we build:** a hi-res mix layer that replaces *how rates are chosen, how audio is resampled, and how it reaches the device*. It keeps upstream's decoder, source chain, Player/sink plumbing and transition logic. It has four parts:

| Part | What it does |
|---|---|
| **Rate planner** | Picks the output rate. Default policy **Seamless**: while crossfade, gapless or AutoDJ is on, the device runs at one "ceiling" rate (default 192 kHz, capped by the device). A transition therefore never needs a device switch, even into a track added to the queue late. Rule everywhere: never below a track's own rate (up to the ceiling), never above the device. |
| **HQ resampler stage** | rubato FFT resampler (≈ −200 dB artefacts) at the *end* of each track's chain, converting to the stream rate. Bit-exact passthrough when the rates already match. |
| **WASAPI exclusive backend** | Windows only. Owns a `rodio::mixer::mixer()` and pulls from it, so the DAC really runs at 192 kHz, bit-perfect when nothing is mixed. |
| **Output formatter** | f32 → S32 (24-in-32) / S24 / S16, TPDF dither only when it is actually needed, plus the bit-perfect gate (unity gain). |

**Why not a full rewrite:** upstream changes `psysonic-audio` about 3 times a week (72 commits in 6 months; `commands.rs` alone 30). rodio's mixer is **sample-exact passthrough when a source's rate already equals the mixer rate**. So if every track reaches the mixer already at the stream rate, rodio's weak converter never runs, and all of upstream's crossfade, gapless, AutoDJ, fade, gain and progress machinery keeps working. The new code goes in fork-named files, with small hooks in upstream files.

---

## 1. What "lossless" means here

| Requirement | Precise meaning |
|---|---|
| Never downsample in a transition | During any blend, the output rate is ≥ max(outgoing rate, incoming rate). |
| Never truncate bit depth | Internal samples are f32, which is exact for 16 and 24-bit PCM. The device container is ≥ the source depth. Dither only when the output is shallower than the source, or after DSP into a 16-bit container. |
| Bit-perfect when possible | Exclusive backend, stream rate = track rate, volume 100 %, no EQ, no normalization, speed 1.0. The bytes written to the device equal the decoded source samples. With the Seamless policy this applies to tracks already at the ceiling rate. With crossfade, gapless and AutoDJ all off (Native policy), it applies to every track. |
| Transparent during the overlap | A crossfade is a mix, so it *cannot* be bit-perfect by definition. Target instead: HQ resampling with artefacts ≤ −170 dB, f32 summing, and no clipping on the overlap. |
| Seamless | **No dropout on any crossfade, AutoDJ or gapless transition, including into a track added to the queue late.** Changing a DAC's clock always mutes it for about 100–500 ms, so the design never changes the device rate during blended playback (user requirement, 2026-10-29). |

---

## 2. Study

### 2.1 Signal path today

```
Symphonia decode (f32)
 → gapless trim (iTunSMPB)
 → [UniformSourceIterator, only when resample_target_hz is set: linear SRC]
 → FoldToStereo → PlaybackRateSource → EqSource (biquads at source rate)
 → EqualPowerFadeIn → TriggeredFadeOut → SpectrumTap → Notifying → CountingSource → PriorityBoost
 → rodio Player     (ReplayGain / LUFS / volume × MASTER_HEADROOM 0.891 as Player volume)
 → rodio Mixer      (wraps every Player in UniformSourceIterator → linear SRC unless rates equal)
 → MixerDeviceSink → cpal WASAPI *shared* (AUTOCONVERTPCM | SRC_DEFAULT_QUALITY)
 → Windows audio engine (resamples to the "Default Format") → DAC
```

Source: `decode/builders.rs:85-214, 245-325`; `sources.rs`; `helpers/gain.rs:277-305`; rodio 0.22.2 `mixer.rs`, `conversions/sample_rate.rs`; cpal 0.17.3 `host/wasapi/device.rs`.

### 2.2 Rate model today

| Situation | Hi-Res OFF | Hi-Res ON |
|---|---|---|
| Stream rate | Device default (probed once at startup) | Track's native rate, **or the blend rate whenever crossfade is enabled (or gapless when not a manual skip)** |
| Crossfade, same rate | Fine (mixer passthrough only if it equals the stream rate) | Both tracks resampled to the blend rate, e.g. **192 → 44.1** |
| Crossfade, different rates | Mixer's linear SRC, and the fade-out bug (R6) | Reopen at the blend rate. The outgoing track is rebuilt from cache, with a ≥ 650 ms gap. **Fails and is only logged** if the outgoing track isn't cached (`commands.rs:511-516`). |
| Gapless chain | Mixer's linear SRC | Reopen **mid-track**; the current track is replayed from cache (`hi_res_blend.rs:244-374`). The chain is abandoned if it isn't cached (`commands.rs:1032-1043`). |
| Manual skip | No reopen | Reopen at the native rate (150 ms + 500 ms prefill, `commands.rs:492-560`) |

### 2.3 Root causes

| ID | Root cause | Where |
|---|---|---|
| R1 | The blend rate is limited to a fixed list (44.1/88.2/96) and defaults to 44.1 | `hi_res_blend.rs:19-37`, `src/lib/audio/hiResCrossfadeResample.ts:2-11` |
| R2 | The blend is applied whenever crossfade is *enabled*, not only when the rates differ | `commands.rs:353-358` |
| R3 | Resampling is linear with no anti-aliasing (rodio `SampleRateConverter`). rubato is only used by `psysonic-burn`. | `builders.rs:147-175, 268-276`; rodio `sample_rate.rs` |
| R4 | Windows shared mode only: the OS resamples to the mix format, and the app can't change that format | cpal 0.17.3 WASAPI; MS docs |
| R5 | Changing the device rate needs a full stream reopen; the gap isn't hidden, and the blend depends on a cache | `output_stream.rs:267-319`, `commands.rs:446-516, 983-1045` |
| R6 | The outgoing fade is sized with the incoming track's rate | `commands.rs:416` → `sink_swap.rs:121-123` |
| R7 | `MASTER_HEADROOM` (0.891) is always multiplied into Player volume, so never bit-perfect | `helpers/gain.rs:305` |
| R8 | One sample counter and one `current_sample_rate` describe one rate at a time (the fork's `COUNT_EPOCH` works around this) | `sources.rs:358-363`, `progress_task.rs:284-308` |
| R9 | The UI shows the *source* format (`audio:format`); nothing reports the real output rate or format | `audioEventHandlers.ts:170`, `Hero.tsx:68,143` |
| R10 | Unreachable `else` branch in the chain-preload hi-res code (upstream's; leave it alone) | `commands.rs:1046-1068` |
| R11 | rodio's converter restarts at every decoder packet (span) and slips about one sample each time, gaining or losing samples (measured in §9) | rodio 0.22.2 `UniformSourceIterator::bootstrap`; `decode/decoder.rs` `current_span_len` |
| R12 | A new rodio `Player` queue reports its empty state as mono 48 kHz with a 512-sample span, so the mixer misreads the first ~511 samples of the first track after every connect (measured: L=R garbage, then a 7668-sample shift). Upstream creates a new Player for each hard start and each incoming crossfade track. | rodio 0.22.2 `queue.rs` (`threshold`), `source/uniform.rs` (`bootstrap`) |

### 2.4 External facts that shape the design

**Windows / cpal**
- cpal 0.17.3 and 0.18.x: WASAPI is **shared mode only**. Every stream is opened with `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`, so any rate "works" and Windows resamples it.
- Windows' resampler quality is unspecified. A 2015 measurement looked like linear interpolation.
- cpal 0.17.3 **I24 is broken** (wrong `wBitsPerSample`; fixed in 0.18.2), so avoid I24 through cpal.
- In shared mode the mix format is float, so **send F32**.

**`wasapi` crate** (HEnquist, 0.26.0, MIT, used by CamillaDSP)
- Event-driven **exclusive** mode, with `WAVEFORMATEXTENSIBLE` built by `WaveFormat::new(32, 24, Int, …)` for 24-in-32.
- `is_supported_exclusive_with_quirks` handles drivers that reject the extensible format, and `calculate_aligned_period_near` handles buffer alignment.
- A rate change means **stop → release → new client → initialize** (no in-place switch).
- CamillaDSP's format priority in exclusive mode is S32 > S24 > S16 > F32.

**DAC relock**
- Typically 100–500 ms of mute after a rate change; some DACs take seconds.
- Roon, JRiver and exaSound add configurable lead-in silence for this.

**rodio 0.22.2**
- `rodio::mixer::mixer(channels, rate) -> (Mixer, MixerSource)` is public, and `MixerSource: Iterator<Item = f32>`. An exclusive backend can own the mixer, and `Player::connect_new(&mixer)` works unchanged.
- The mixer sums with a plain `+=` and **does not clip**.
- It has optional `dither` (TPDF) and `64bit` features. 0.23 only bumps cpal; master adds `Source::resample` on rubato 3.

**rubato 0.16.2** (already in `Cargo.lock` via `psysonic-burn`)
- `FftFixedIn` gives fixed-ratio quality similar to sinc "Accurate" (< −200 dB), at a much lower cost.
- Sinc presets: Balanced = 192 / 512 / Quadratic (≈ −170 dB); Accurate = 256 / 256 / Cubic.
- `output_delay()` reports the latency to compensate, and `process_into_buffer` doesn't allocate.

**Other players**
- None of the players checked crossfades seamlessly across rates out of the box:
  - MPD, Roon and Squeezebox only crossfade when the formats match.
  - foobar2000 and JRiver need a fixed output rate set by the user.
  - Audirvana and DeaDBeeF have no crossfade.
  - Audacious resamples the outgoing buffer (nearest-neighbour).
- **The Seamless policy below is the one approach that works (a fixed output rate), made automatic.** The engine picks the ceiling and does HQ upsampling, instead of asking the user to configure a resampler.

### 2.5 Fork constraints

**Churn (last 6 months)**
- `psysonic-audio` has 72 commits, and `src/features/playback` has 130.
- The most-changed audio files are `commands.rs` (30), `play_input.rs` (21), `engine.rs` (19), `device_resume.rs` (16) and `lib.rs` (16).
- The fork already patches `commands.rs` and `sources.rs`.

**Coverage gate**
- The audio files in `.github/hot-path-files.txt` must stay at ≥ 70 % line coverage: `decode/*`, `preserve_worker/{seek,streaming}.rs`, `stream/icy.rs`, `progress_task.rs` and `ipc.rs`.
- **The plan doesn't touch any of them.**

**Fork patterns to follow**
- Fork Tauri commands sit in their own block at the top of `collect_commands!` and `generate_handler!` (`src-tauri/src/lib.rs:111-117, 590+`). They are regenerated to the top of `bindings.ts`.
- Fork settings sections are mounted with one import plus one JSX line (`StorageTab.tsx`, `IntegrationsTab.tsx`).
- Fork UI uses plain English plus a small persisted zustand store in the same file. No locale edits.

---

## 3. Target design

### 3.1 Principles

1. **Upstream decides *when* a transition happens; the fork decides *at what rate*, and converts with high quality.** TS transition timing, `sink_swap`, the gapless queue, fades and gain all stay as they are.
2. **Convert once, at the end of each track's chain.** The HQ resampler sits *after* `CountingSource`, so:
   - position counts native frames and is right at any output rate (R8 goes away for fork paths);
   - EQ coefficients and fade lengths are computed at the source's own rate;
   - gapless items reach the Player already at the stream rate, so there is no converter bootstrap problem.
3. **The device rate never changes while blended playback (crossfade, gapless or AutoDJ) is on**, not even for a late-added track. It changes only when the ceiling setting changes, on a device change, or at track starts when transitions are all off (Native policy).
4. **Never downsample a track below its own rate (up to the ceiling), and never exceed what the device supports.**
5. **New logic lives in a new crate plus fork-named files.** Upstream files get one-call hooks.

### 3.2 Architecture

```
(upstream chain at the track's NATIVE rate, resample_target_hz = 0)
 → [fork] HqResample → stream rate        (passthrough when the rates are equal)
 → rodio Player → rodio Mixer             (rates equal → passthrough)
 → OutputHandle
     ├─ Cpal(MixerDeviceSink)             shared mode, F32; Windows mix-format rate
     └─ [fork] Exclusive(WasapiOutput)    owns rodio::mixer::mixer(ch, rate);
                                          event-driven render thread (MMCSS "Pro Audio")
                                          pulls MixerSource → [fork] OutputFormatter
                                          (S32 / S24 / S16 / F32 + TPDF when needed)
                                          → IAudioRenderClient → DAC at the real rate
```

### 3.3 Rate planner

**Why there's a ceiling:** a DAC can't change its clock mid-stream without muting for about 100–500 ms, sometimes seconds. The only way to make *every* transition seamless, including into a track added late, is to have the device already running at a rate ≥ any track it might receive. Upsampling into that rate loses nothing; only downsampling would.

**Inputs:**
- backend (shared / exclusive) and the device's supported rates;
- policy, ceiling setting, and the current stream rate;
- the incoming track's decoded rate;
- whether blended transitions are on (crossfade, gapless or AutoDJ);
- the queue window (Adaptive only).

**Policies:**
- **Seamless (default, required).** While any blended transition is on, the stream runs at `ceiling = min(user ceiling, device max)`:
  - The user ceiling defaults to 192 kHz; "Auto" uses the highest rate in the local library index.
  - Every track below the ceiling is upsampled into it; a track at the ceiling passes through bit-perfect.
  - The stream opens at the ceiling on the first play and **never reopens for a transition**, so a track enqueued late is seamless.
- **Native.** Used automatically when crossfade, gapless and AutoDJ are all off, because every track change is then a hard start anyway. The stream follows each track's own rate, so every track is bit-perfect, with lead-in silence for the DAC relock.
- **Adaptive (optional, for bit-perfect purists).**
  - The stream runs at the max of the current track and the next N queued tracks.
  - Homogeneous queues stay bit-perfect.
  - A higher-rate track enqueued late causes a **Resync** gap: reopen, rebuild the outgoing track at the new rate, add lead-in silence.
  - Off by default because it breaks the seamless requirement.
- **Above the ceiling.** A track rate above the ceiling (e.g. a 352.8 kHz file with a 192 kHz ceiling, or a DAC that tops out at 96 kHz) has to be converted down to the ceiling. This is the only downsample in the design, it's caused by the setting or the hardware, and the output-format badge shows it.
- **Shared-mode rule.** In cpal shared mode the ceiling is forced to the Windows mix format rate, because Windows resamples anything else anyway.
  - If the mix format is below 192 kHz, the UI warns and recommends exclusive mode, or setting Windows *Default Format* to 24-bit / 192 kHz.

**Cases (Seamless, exclusive, ceiling 192 kHz):**

| Case | Stream rate | What happens | Seamless | Downsampled |
|---|---|---|---|---|
| 192 → 192 crossfade | 192 | Nothing; both pass through bit-perfect | ✓ | never |
| 192 → 48 crossfade | 192 | 48 is upsampled ×4 into 192 | ✓ | never |
| 48 → 192 crossfade | 192 | 48 was already upsampled; 192 passes through | ✓ | never |
| 48 → 192, 192 added to the queue late | 192 | Same as above; the stream was at the ceiling all along | ✓ | never |
| 44.1 → 96 gapless | 192 | Both upsampled; sample-exact boundary | ✓ | never |
| 48 → 352.8 crossfade | 192 | 352.8 is converted down to the ceiling (raise the ceiling to avoid this) | ✓ | above the ceiling only |
| Transitions all off, skip to an 88.2 track | 88.2 (Native) | Hard start at the native rate, with lead-in silence | n/a | never |

### 3.4 HQ resampler stage (`HqResample<S: rodio::Source>`)

- Uses rubato **0.16.2 `FftFixedIn`**: the ratio is fixed per track, and it's the same version `psysonic-burn` already compiles. Chunk size is 1024 frames, configurable.
- **Delay:** compensates `output_delay()` at the start (drops the leading frames) and flushes the tail with a partial final chunk, so track length is preserved to ±1 frame and gapless boundaries stay sample-exact.
- **Passthrough** when input rate = output rate: zero-cost and bit-exact.
- **Seek:** forwards `try_seek` and resets the resampler state.
- **Channels:** any count; it runs after `FoldToStereo`.
- **Varispeed:** `PlaybackRateSource` changes the *reported* rate. The resampler checks `sample_rate()` at span boundaries and rebuilds. While varispeed (not preserve-pitch) is active, the stage may fall back to `SincFixedIn` with a variable ratio (decide in Phase 2).
- **Allocations:** none in steady state; buffers are preallocated.

### 3.5 Bit depth and output format

- **Internal precision:** f32 end to end, exact for 16- and 24-bit sources. 32-bit integer sources lose bits below −144 dBFS (accepted; an optional f64 path is a non-goal).
- **Exclusive format choice:** the first supported of S32 (24 valid bits) > S24 packed > S16 > F32, and never shallower than the source when a deeper one exists.
- **f32 → S32 conversion:** multiply by 2³¹ and round. 16- and 24-bit sources survive bit-exact.
- **Dither:** TPDF, 2 LSB peak-to-peak, *only* when the container is 16-bit **and** (DSP is active **or** the source is deeper than 16-bit). Never in the bit-perfect state.
- **Overs:** the rodio mixer doesn't clip, so the formatter has a soft limiter that only engages above ≈ −0.3 dBFS (overlap peaks). Combined with the equal-power fade, it's normally inactive.
- **Bit-perfect gate:** exclusive, stream rate = track rate, volume 100 %, normalization off, EQ off or flat, speed 1.0, no blend active. When all hold, Player volume is set to exactly 1.0 (bypassing `MASTER_HEADROOM`), and the formatter reports `bitPerfect: true`.

### 3.6 Contract additions (all additive; nothing upstream renamed)

| Kind | Name | Payload |
|---|---|---|
| command | `fork_hires_set_config` | `{ enabled, backend: 'shared'\|'exclusive', policy: 'seamless'\|'adaptive', ceilingHz: number\|'auto', lookahead, resyncSilenceMs, dither: 'auto'\|'off', chunkFrames }` (Native is chosen automatically when all transitions are off) |
| command | `fork_hires_set_queue_rates` | `{ rates: number[] }`: upcoming tracks' rates, decoded when known, otherwise Subsonic `samplingRate`. **Adaptive policy only.** |
| command | `fork_hires_status` | → current backend, rate, format, bit-perfect, and the device's supported rates |
| event | `fork:audio-output-format` | `{ backend, exclusive, rateHz, bits, container, bitPerfect, resampledFromHz? }` |

The settings travel through these commands, **not** through the `audio_play` payload, so `playTrackAction.ts`, `gaplessChainPreload.ts` and the Vitest mocks of `audio_play` stay untouched.

### 3.7 Code placement

**New files (no conflict risk):**
- `src-tauri/crates/psysonic-fork-hires/`: the `crates/*` glob makes it a workspace member automatically. No Tauri dependency, so tests are fast. Modules:
  - `rate_plan.rs`
  - `hq_resample.rs`
  - `format.rs` (formatter, dither, limiter)
  - `wasapi_exclusive.rs` (`cfg(windows)`)
  - `config.rs`
- `src-tauri/crates/psysonic-audio/src/fork_hires.rs`: the glue (config state, commands, planner calls, output-format event, `OutputHandle` helpers).
- `src/features/settings/components/ForkHiResEngineSection.tsx`: the settings UI plus a persisted store (`psysonic_fork_hires`).
- `src/features/playback/forkHiResQueueSync.ts`: pushes the queue window to Rust (Adaptive policy only; can be built last).
- `src/features/nowPlaying/components/ForkOutputFormatBadge.tsx`

**Hooks in upstream files** (keep each to one call or one line where possible):

| File | Hook |
|---|---|
| `crates/psysonic-audio/Cargo.toml` | +1 dependency line |
| `crates/psysonic-audio/src/lib.rs` | +`mod fork_hires;` and re-export the commands |
| `crates/psysonic-audio/src/commands.rs` (`audio_play`) | rate selection (~L353-358, L472-480) → `fork_hires::plan(...)` when enabled; wrap the built source with `HqResample` before the Player append; skip the legacy blend reopen |
| `crates/psysonic-audio/src/commands.rs` (`audio_chain_preload`) | same, ~L947-1045 |
| `commands.rs:416` + `sink_swap.rs:121` | R6 fix: snapshot the outgoing rate and channels before they're overwritten (**offer upstream as a PR**) |
| `engine.rs`, `engine/output_stream.rs` | `stream` becomes `OutputHandle` (5 references); `connect_new_player` uses `handle.mixer()`; open dispatches by backend |
| `device_watcher.rs` / `device_resume.rs` | the reopen target comes from the planner instead of `is_hi_res=false` (L88-115, L180, L197) |
| `src-tauri/src/lib.rs` | fork command block (both macro lists) |
| `src/generated/bindings.ts` | regenerated (fork entries at the top) |
| `AudioTab.tsx` | +1 import, +1 JSX line; hide the legacy 44.1/88.2/96 picker when the fork engine is on |
| `MainApp.tsx` | +1 side-effect import for the queue sync |
| `nowPlaying/components/Hero.tsx` | +1 JSX line for the badge |

---

## 4. Decisions needed (with recommendations)

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D1 | Default rate policy | Seamless (ceiling) / Adaptive (queue max) | **Decided: Seamless.** User requirement (2026-10-29): smooth even when a higher-rate track is added late. Adaptive stays as an option. |
| D2 | Exclusive mode | Build it (Phase 4) / shared only + "set Windows to 24/192" | **Decided: build it.** It's the only way to get a real 192 kHz and bit-perfect output on Windows. Phases 1-3 still work without it. |
| D3 | Resampler | rubato 0.16.2 / rubato 5.x / libsoxr | **Decided: rubato 0.16.2.** Already compiled for `psysonic-burn`, pure Rust, MIT. Revisit 5.x (runtime SIMD) only if CPU is a problem. |
| D4 | Unexpected upward rate in a blend | Resync gap / downsample during the fade / prevent it | **Decided: prevent it.** With Seamless, the stream already runs at the ceiling, so an upward change can't happen. Resync exists only under Adaptive. |
| D5 | Legacy hi-res blend | Keep as the fallback when the fork engine is off / remove | **Decided: keep.** A one-switch rollback, and no edits to upstream logic. |
| D6 | Ceiling value | 192 kHz / "Auto" (highest rate in the local library) / device max | **Decided: 192 kHz default, with "Auto" offered.** Device max (sometimes 384–768 kHz) costs CPU for no audible benefit. |

---

## 5. Phased plan with checklist

Work on a branch (`fork/hires-engine`), and merge to `main` when each phase's exit criteria pass.

**Windows build notes (2026-10-29):** in Git Bash, cargo needs `export PATH="$HOME/.cargo/bin:$PATH"`. `cmake` is not on PATH; use the copy in Visual Studio Build Tools (`C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin`). Build and test `psysonic-audio` together with `-p psysonic-library`; on its own it fails to compile `psysonic-analysis` (a feature-unification quirk). Fork-crate work can use its own `CARGO_TARGET_DIR=target/fork-hires` so it doesn't wait on the big build.

### Phase 0: Baseline and measurement harness
- [x] Create branch `fork/hires-engine`.
- [x] Install the Rust toolchain on the dev machine (rustup via winget; stable 1.99.1, MSVC).
- [x] Scaffold `crates/psysonic-fork-hires` with its own version `0.1.0` (not the workspace version, so upstream's version bumps never touch its `Cargo.lock` entry).
- [x] Test kit (`testkit` module, also exported behind a `testkit` feature for other crates' tests): in-memory test signals (integer-Hz tones, 16/24-bit quantization) and metrics on a coherent one-second FFT window (residual, worst spur, gain, stopband level), plus tests that check the metrics themselves.
- [x] Design-assumption tests: rodio's `UniformSourceIterator` and `mixer` pass audio through **bit-exact** when the rates match (the whole design depends on this).
- [x] Baseline report (`#[ignore]`): rodio's linear converter for each rate pair: 1 kHz and near-Nyquist tone quality, and stopband leakage when downsampling (R3).
- [x] Record the baseline numbers in §9 of this document.
- Moved out of Phase 0: FLAC fixtures to Phase 3 (needed only for end-to-end decode tests), and the R6 fade-length reproduction to Phase 1 (it needs the real engine code).
- **Exit:** the design-assumption tests pass, and the baseline table is in §9.

### Phase 1: Correctness fix (ship alone, then propose upstream)
- [x] R6: snapshot the outgoing `current_sample_rate` and `current_channels` before `commands.rs:416`; `sink_swap.rs` sizes the outgoing fade with the snapshot.
- [x] Regression test `sink_swap_tests.rs::outgoing_fade_uses_outgoing_rate_not_incoming`. It failed on the old code (384 000 vs 1 536 000 samples, the 4× fade) and passes now. The same bug's channel-count variant in `hi_res_blend.rs` is fixed too.
- [ ] Open an upstream PR with the fix, so the fork stops carrying it once upstream merges it. The patch is ready; opening it needs the user's go-ahead.
- **Exit:** fade lengths are correct across rates in both Hi-Res modes.

### Phase 2: Fork crate core (pure Rust, fully tested)
- [x] Scaffold `crates/psysonic-fork-hires` (`rodio = { version = "0.22", default-features = false }`, `rubato = "=0.16.2"`).
- [x] `rate_plan.rs`: types `Backend`, `Policy`, `TransitionKind`, `DeviceCaps`, `PlanInput`, `Plan { stream_rate, reopen, resync_silence_ms }`.
- [x] `plan_output_rate(&PlanInput) -> Plan`: the policy core (**marked as the user's contribution, 5-10 lines**). Done (Sonnet, 2026-10-29).
- [x] Planner tests, a table over all rate pairs × policies × transition kinds, asserting these properties:
  - **Seamless never reopens between blended tracks, for any queue order, including late additions;**
  - no track is converted below its own rate unless it is above the ceiling;
  - the rate never exceeds the device caps;
  - shared mode is always the mix rate;
  - Native follows each track; Adaptive keeps a homogeneous queue at its native rate.
- [x] `hq_resample.rs`: a rodio `Source` built on `FftFixedIn`, with delay compensation, tail flush, seek reset, rate re-detection at span boundaries, and preallocated buffers.
- [x] Resampler tests:
  - passthrough is bit-exact;
  - a 48 → 96 sweep aliases below −150 dB;
  - 44.1 → 192 output length is within ±1 frame;
  - net delay after compensation is 0;
  - seek mid-track works;
  - 6-channel input works.
- [x] `format.rs`: f32 → S32 (24-in-32) / S24 / S16 / F32, seeded TPDF dither, soft limiter.
- [x] Formatter tests:
  - 16- and 24-bit round trips through f32 → S32 are bit-exact;
  - dither is only applied when the rules say so;
  - the limiter is inactive below its threshold.
- [x] CPU budget test (`#[ignore]`, in the style of `rows_meets_perf_budget`): 192 kHz stereo `FftFixedIn` must run ≥ 50× real time on the dev machine.
- **Exit:** `cargo test -p psysonic-fork-hires` is green, and clippy is clean with `-D warnings`.

### Phase 3: Integrate in shared mode (cpal), both policies
- [x] `psysonic-audio/src/fork_hires.rs`:
  - config state;
  - the commands `fork_hires_set_config`, `fork_hires_set_queue_rates` and `fork_hires_status`;
  - the emitter for `fork:audio-output-format`.
- [x] **Queue recipe (pinned by tests in `hq_resample_tests.rs`):** connect the Player first; before the mixer pulls any sample, append `priming_silence(stream_channels, stream_rate)` (fixes R12); queue every track as `for_output(source, stream_channels, stream_rate)`, never as a bare `HqResample` (otherwise a channel-count change at a gapless join is misread); prime again whenever the sink is rebuilt.
- [x] Varispeed: `HqResample` notices an input-rate change only at its next 1024-frame chunk (≈ 23 ms at 44.1 kHz at the old ratio) and rebuilds its FFT stage on the audio thread (one allocation). Decide whether speed changes need a tighter path.
- [x] Hook `audio_play`. When enabled:
  - build with `resample_target_hz = 0`;
  - plan the rate;
  - wrap with `HqResample`;
  - skip the legacy blend reopen.
- [x] Hook `audio_chain_preload` the same way (gapless: wrap the incoming track to the current stream rate; never reopen mid-track except for Resync).
- [x] Seamless: open the stream at the ceiling on the first play while transitions are on. Changing a transition setting or the ceiling takes effect at the next hard start, never mid-song.
- [ ] Native (all transitions off): follow each track's rate with lead-in silence.
- [ ] Resync path (Adaptive only, can come last): reuse `capture_outgoing_blend_snapshot` / `spawn_outgoing_blend_resample` with the new target rate, plus lead-in silence.
- [x] Shared-mode planner input: use `device_default_rate` (the mix format) as the fixed rate. Probe the device's mix format and bit depth for the status command.
- [x] Reopen paths use the planner's target instead of hard-coded defaults:
  - `device_watcher`
  - `device_resume`
  - `power_resume`
  - idle release / reopen (`stream_idle.rs`, `ensure_output_stream_open_locked`)
- [x] Check the spectrum tap: it runs before the resampler at the native rate, so confirm `SOURCE_RATE` is right.
- [ ] Wrap the preview player with `HqResample` too (otherwise rodio's linear SRC still applies to previews).
- [x] Register the commands in the fork block of `src-tauri/src/lib.rs`, regenerate bindings (`cargo test -p psysonic regenerate_committed_bindings -- --ignored`), and make sure `typeable_commands_are_all_collected` passes.
- [x] Harness integration tests:
  - crossfades 192 → 48, 48 → 192, **48 → 192 with the 192 track enqueued during the 48 track (must have no gap)**, 44.1 → 96;
  - gapless 44.1 → 96, sample-exact;
  - position accuracy through each transition;
  - fade durations.
- **Exit:**
  - With Windows Default Format at 24/192, every mixed-rate crossfade, gapless and AutoDJ transition is seamless and uses HQ SRC, including into late-added tracks.
  - Nothing is forced to 44.1.
  - The legacy path is unchanged when the fork engine is off.

#### Phase 3 implementation notes (2026-10-29)
- **Where it hooks:** `fork_hires.rs` (glue) plus `// Fork patch` hooks in `commands.rs` (`audio_play`, `audio_chain_preload`), `device_resume.rs`, `device_watcher.rs`, `device_commands.rs`, `psysonic-audio/src/lib.rs` and `Cargo.toml`, and the command registration in `src-tauri/src/lib.rs`. No coverage-gated file is touched; `engine.rs`, `output_stream.rs`, `progress_task.rs` are unchanged.
- **Legacy hi-res off:** one shadow line, `hi_res_enabled && !fork_hires::enabled()`, makes `blend_rate_hz` return `None`, so the forced-44.1 blend, the outgoing rebuild and the mid-track gapless reopen are all unreachable while the fork engine is on.
- **Stream protection kept:** fork streams still get the max-priority audio thread and the 150 ms settle (`fork_hi_res_stream`), and the 500 ms prefill now keys on the real stream rate (> 48 kHz), not the track's rate.
- **Queueing:** the first track on a new Player uses `primed_for_output` (priming silence plus seek-safe; fixes R12 and the paused-seek hazard); gapless tracks use `for_output`. Position stays right because each track's counter runs at its native rate before the resampler.
- **Varispeed:** a track that starts while varispeed is active (speed ≠ 1.0) uses the upstream path; toggling it mid-track isn't handled yet.
- **Mix rate:** probed lazily on the first play and refreshed on every device switch (`refresh_mix_rate`); the idle-release reopen still uses the last requested rate (unchanged upstream code), which equals the mix rate.
- **Measured through the real decoder:** exact lengths (22.05 kHz MP3 and 44.1 kHz WAV to 48 kHz; gapless 44.1 → 32 kHz and mono → stereo), residual at the 16-bit file's floor (−92 dB).
- **Try it before the UI exists:** quit the app, create `%APPDATA%\dev.psysonic.player\fork-hires.json` containing `{"enabled":true,"policy":"seamless","ceilingHz":null,"resyncSilenceMs":250,"chunkFrames":1024}`, set Windows Sound settings → your device → Advanced → Default Format to 24-bit, 192000 Hz, then start the app. In shared mode the output rate is the Windows mix rate.

### Phase 4: WASAPI exclusive backend (Windows)
- [ ] Add `wasapi = "0.26"` under `[target.'cfg(windows)'.dependencies]` in the fork crate. Check for `windows` crate duplication and the compile-time impact.
- [ ] Map the device the user selected (existing `selected_device` keys, `dev_io` identity) to the WASAPI endpoint ID.
- [ ] Format negotiation (a pure function, unit-tested with mocked caps): S32/24 > S24 > S16 > F32, ≥ the source depth, via `is_supported_exclusive_with_quirks`.
- [ ] Ceiling fallback when the device lacks the ceiling rate: the highest supported rate ≤ the ceiling. For Native, the nearest **higher** supported rate, else the highest supported.
- [ ] Render thread:
  - event-driven;
  - MMCSS "Pro Audio";
  - period from `calculate_aligned_period_near` (≈ 10 ms default, configurable);
  - pulls `MixerSource` → formatter → render buffer;
  - counts underruns.
- [ ] `OutputHandle` enum in `engine.rs` / `output_stream.rs`. `connect_new_player` and preview use `handle.mixer()`.
- [ ] Rate switch: stop → release → re-initialize. Retry once on `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`. Add lead-in silence of `resyncSilenceMs` (default 250 ms).
- [ ] Failures:
  - `AUDCLNT_E_DEVICE_IN_USE` or `UNSUPPORTED_FORMAT` → fall back to shared mode, emit status, and show a toast.
  - Device unplugged or the default device changed → reuse the `device_watcher` flow.
- [ ] The idle release (60 s) frees the exclusive device for other apps.
- [ ] Bit-perfect gate wired up (Player volume exactly 1.0 when the gate holds), and the `bitPerfect` flag reported.
- [ ] Debug-only CRC tap: compare the formatter's input against the decoded source during gate-on playback, and log mismatches.
- **Exit:**
  - Exclusive playback at the 192 kHz ceiling (Seamless) and at native rates (Native).
  - The CRC tap shows bit-perfect output for at-ceiling tracks, and for every track under Native, with no DSP.
  - Every mixed-rate transition is seamless, including into late-added tracks.

### Phase 5: Frontend (fork-only files)
- [ ] `ForkHiResEngineSection.tsx` in Settings → Audio, with these controls:
  - enable (fork engine / legacy);
  - backend (shared / exclusive);
  - policy (Seamless / Adaptive);
  - ceiling (192 kHz / Auto / list of device rates);
  - lookahead (Adaptive only);
  - DAC resync silence;
  - dither.
- [ ] The section also shows live status from `fork_hires_status`: device rates, current format, bit-perfect.
- [ ] Persisted store `psysonic_fork_hires`. Push the config on change and at startup.
- [ ] `forkHiResQueueSync.ts` (Adaptive policy only; build last):
  - subscribe to the playerStore queue and index;
  - the next N tracks' rates (decoded rate when known; `samplingRate` otherwise; skip transcoded streams whose rate is unknown);
  - debounced `fork_hires_set_queue_rates`.
- [ ] `ForkOutputFormatBadge.tsx`: shows e.g. "192 kHz · 24-bit → 192 kHz · S32 exclusive · bit-perfect" from `fork:audio-output-format`.
- [ ] When the fork engine is on, hide the legacy blend-rate picker and replace it with a note.
- [ ] Vitest:
  - store and queue-rate computation;
  - the section renders and pushes its config;
  - mocks for the new commands and event;
  - `npm run lint`, `npm run dep:check`, `npx tsc --noEmit`.
- **Exit:** all frontend checks are green, and the only failures are the 10 known fork failures.

### Phase 6: Verification
- [ ] Offline metrics compared with the Phase 0 baseline (aliasing, THD+N, null test, fades, drift over 10 minutes, gapless boundary).
- [ ] Transition matrix on real hardware (§6), in shared and exclusive mode.
- [ ] Device matrix:
  - onboard Realtek;
  - a USB UAC2 DAC (≥ 192 kHz);
  - HDMI;
  - Bluetooth (shared only, which must fall back cleanly).
- [ ] Soak: 2 hours of a mixed-rate queue with crossfade. Zero underruns, steady memory, CPU budget met.
- [ ] Full CI locally:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace --all-targets`
  - `npm test`, `npm run lint`, `npm run dep:check`
  - `npx tsc --noEmit`
- **Exit:** every row of the matrix passes, or has a documented exception.

### Phase 7: Ship and maintain
- [ ] Add a `.github/README.md` bullet describing the patch and its files.
- [ ] Merge to `main`, run "Fork sync and release" by hand with **force**, and install through the updater.
- [ ] Write a conflict playbook for this feature: the hook list in §3.7, plus `cargo test -p psysonic-fork-hires` after every upstream merge.
- [ ] Lockfile rule for this feature: when a sync conflict is resolved by taking upstream's `Cargo.lock`, run any cargo command in `src-tauri` (e.g. `cargo metadata --format-version 1`) so cargo re-adds the fork crate's entries.
- [ ] Track upstream's rodio bump (0.23 / master `Source::resample`) and adjust the fork crate when upstream moves.

---

## 6. Test matrix

**Rates** (from → to): 44.1→48, 48→44.1, 44.1→96, 96→44.1, 48→192, 192→48, 88.2→176.4, 192→192, 44.1→44.1.
**Bit depths:** 16→24, 24→16, 24→24.
**Transitions:**
- crossfade (2 s, 8 s);
- AutoDJ blend;
- gapless;
- manual skip with blend (fork patch `autodjManualBlend.ts`);
- manual skip without blend;
- seek during a fade;
- pause during a fade;
- skip during a fade;
- queue edited during the lookahead;
- **higher-rate track added to the queue while a lower-rate track is playing (must be seamless).**

| Check | How |
|---|---|
| No downsampling | `fork:audio-output-format.rateHz` ≥ each track's rate (up to the ceiling) |
| No device switch in blended playback | The stream-reopen counter stays 0 across the whole Seamless session |
| Seamless | No gap > 1 ms in the offline render; listening test on hardware |
| Bit-perfect (gate on) | Debug CRC tap matches; badge says bit-perfect |
| Position and lyrics | Position error < 20 ms after each transition (fork lyrics patch) |
| Fade length | Within ±1 % of the setting at any rate pair |
| No clipping | Formatter limiter counter stays 0 at normal levels |

---

## 7. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Upstream churn in `commands.rs` / `engine.rs` | Hooks are single calls into `fork_hires`; all logic lives in fork files. Run the fork crate tests after every merge. |
| Upstream rewrites the audio path or bumps rodio (master adds `Source::resample` on rubato 3) | Keep the fork crate's rodio version equal to `psysonic-audio`'s. The hook surface is small enough to re-apply. |
| Exclusive mode mutes other apps (Discord, notifications) | Expected; documented in the UI. The idle release frees the device. |
| The DAC relocks slower than the lead-in silence | `resyncSilenceMs` is configurable (0–2000 ms). |
| FFT chunks cause CPU spikes in the render callback | Measure; tune the chunk size; if needed, move the resampler to a producer thread feeding a `ringbuf` (already a dependency). |
| Wrong `samplingRate` metadata (transcoding) | Decoded rates drive the resampler; the queue hints only steer planning. A mismatch triggers Resync, never a downsample. |
| cpal I24 bug | Never use I24 through cpal; exclusive output goes through the `wasapi` crate. |
| Coverage gate | No gated file is touched (`decode/*`, `progress_task.rs`, …). New code ships with its own tests. |
| Linux / macOS builds | The fork crate compiles everywhere; exclusive mode is `cfg(windows)`. Shared mode plus the planner work on every platform. |

## 8. Sources

- cpal 0.17.3 WASAPI: https://github.com/RustAudio/cpal/blob/v0.17.3/src/host/wasapi/device.rs · changelog: https://docs.rs/crate/cpal/latest/source/CHANGELOG.md
- Windows audio:
  - https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-formats
  - https://learn.microsoft.com/en-us/windows/win32/coreaudio/exclusive-mode-streams
  - https://learn.microsoft.com/en-us/windows/win32/api/mmreg/ns-mmreg-waveformatextensible
- Windows 10 mixer measurements: https://archimago.blogspot.com/2015/11/measurements-windows-10-audio-stack.html
- `wasapi` crate: https://docs.rs/wasapi/0.26.0/wasapi/ · CamillaDSP WASAPI notes: https://github.com/HEnquist/camilladsp/blob/master/backend_wasapi.md
- rodio 0.22.2:
  - https://docs.rs/crate/rodio/0.22.2/source/src/mixer.rs
  - https://docs.rs/crate/rodio/0.22.2/source/src/conversions/sample_rate.rs
- rubato 0.16.2: https://docs.rs/rubato/0.16.2/rubato/ · CamillaDSP resampler profiles: https://github.com/HEnquist/camilladsp/blob/master/README.md
- Player behaviour:
  - MPD: https://mpd.readthedocs.io/en/latest/user.html
  - Roon: https://help.roonlabs.com/portal/en/kb/articles/faq-where-can-i-set-a-crossfade
  - JRiver: https://wiki.jriver.com/index.php/Output_Format
  - Audacious crossfade: https://github.com/audacious-media-player/audacious-plugins/blob/master/src/crossfade/crossfade.cc
- DAC relock: https://community.roonlabs.com/t/having-roon-add-a-delay-until-dac-is-ready/116922

## 9. Baseline measurements

Measured 2026-10-29 with the `psysonic-fork-hires` test kit (`cargo test -p psysonic-fork-hires -- --ignored --nocapture`, from `src-tauri`). Each tone is measured on a coherent one-second window, so the floor is f32 precision (about −155 dB at half scale).

How to read the columns:
- **1k / HF resid**: everything except the tone (noise, distortion, aliases, images) relative to the tone. Lower is better; above 0 dB means the error is louder than the music.
- **spur**: the strongest single unwanted component.
- **HF gain**: level of a tone at 0.42 × the lower rate (near the shared Nyquist); 0 dB is ideal.
- **stopband leak**: downsampling only. A tone above the new Nyquist, which an ideal converter removes completely; 0 dB means it folds back into the audible band at full level.
- **len err**: output samples vs the exact expected length, for 2 s of input.
- **first dev**: first output sample that differs from an ideal linear interpolator by more than 1e-4.

Rows per rate pair:
- **rodio 1 span**: rodio's `UniformSourceIterator` fed one long span.
- **rodio 4096 / 1152 spans**: the same converter fed packet-sized spans, the way the real decoder (`SizedDecoder`) does it: 4096 frames for FLAC, 1152 for MP3. **This is what live playback goes through** whenever a track's rate differs from the stream rate.
- **ideal linear**: a reference linear interpolator, showing what linear interpolation itself can achieve.

```text
     from -> to | converter        |  1k resid |   1k spur |  HF resid |   HF spur |  HF gain | stopband leak | len err | first dev
 44100 -> 48000  | rodio 1 span     |  -41.0 dB |  -47.7 dB |   -4.9 dB |   -5.7 dB |  -5.5 dB |             — |       1 |     35665
 44100 -> 48000  | rodio 4096 spans |   -9.8 dB |  -13.6 dB |   27.5 dB |   24.5 dB | -31.7 dB |             — |      17 |      4458
 44100 -> 48000  | rodio 1152 spans |  -14.9 dB |  -19.3 dB |   21.2 dB |   19.5 dB | -25.4 dB |             — |      10 |      1253
 44100 -> 48000  | ideal linear     |  -62.4 dB |  -65.4 dB |   -5.4 dB |   -5.6 dB |  -5.4 dB |             — |       0 |      none
 48000 -> 44100  | rodio 1 span     |  -29.9 dB |  -37.8 dB |   -1.8 dB |  -10.0 dB |  -6.0 dB |       -4.7 dB |       1 |     30105
 48000 -> 44100  | rodio 4096 spans |   -7.8 dB |  -11.4 dB |   34.8 dB |   32.4 dB | -38.6 dB |       -4.7 dB |      19 |      3763
 48000 -> 44100  | rodio 1152 spans |    3.6 dB |    2.3 dB |   32.4 dB |   26.9 dB | -36.2 dB |       -4.7 dB |      50 |      1058
 48000 -> 44100  | ideal linear     |  -63.9 dB |  -66.9 dB |   -7.8 dB |   -8.1 dB |  -4.5 dB |       -4.7 dB |       0 |      none
 44100 -> 96000  | rodio 1 span     |  -33.7 dB |  -40.3 dB |   -3.1 dB |   -6.2 dB |  -6.0 dB |             — |      -2 |     71330
 44100 -> 96000  | rodio 4096 spans |  -20.5 dB |  -25.2 dB |   29.0 dB |   27.8 dB | -33.3 dB |             — |     -10 |      8915
 44100 -> 96000  | rodio 1152 spans |   -4.4 dB |   -7.3 dB |   26.6 dB |   22.5 dB | -30.9 dB |             — |     -58 |      2506
 44100 -> 96000  | ideal linear     |  -62.4 dB |  -65.4 dB |   -5.4 dB |   -5.6 dB |  -5.4 dB |             — |       0 |      none
 96000 -> 44100  | rodio 1 span     |  -31.5 dB |  -37.4 dB |   -5.4 dB |   -9.3 dB |  -2.2 dB |       -3.5 dB |       1 |     15053
 96000 -> 44100  | rodio 4096 spans |   -7.9 dB |  -11.4 dB |   35.5 dB |   35.0 dB | -36.6 dB |       -3.5 dB |      19 |      1882
 96000 -> 44100  | rodio 1152 spans |   13.3 dB |    9.8 dB |   55.3 dB |   53.6 dB | -56.4 dB |       -3.5 dB |     133 |       529
 96000 -> 44100  | ideal linear     |  -75.9 dB |  -79.1 dB |  -23.8 dB |  -24.9 dB |  -1.1 dB |       -3.5 dB |       0 |      none
 48000 -> 96000  | rodio 1 span     |  -28.7 dB |  -36.6 dB |    1.3 dB |   -6.4 dB |  -6.4 dB |             — |      -3 |     65535
 48000 -> 96000  | rodio 4096 spans |  -12.9 dB |  -17.1 dB |   19.9 dB |   14.7 dB | -22.7 dB |             — |     -24 |      8191
 48000 -> 96000  | rodio 1152 spans |   -0.2 dB |   -2.3 dB |   33.9 dB |   31.1 dB | -36.7 dB |             — |     -84 |      2303
 48000 -> 96000  | ideal linear     |  -59.4 dB |  -59.4 dB |   -4.4 dB |   -4.4 dB |  -4.1 dB |             — |       0 |      none
 96000 -> 48000  | rodio 1 span     | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
 96000 -> 48000  | rodio 4096 spans | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
 96000 -> 48000  | rodio 1152 spans | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
 96000 -> 48000  | ideal linear     | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
 48000 -> 192000 | rodio 1 span     |  -25.2 dB |  -33.0 dB |    5.1 dB |   -0.9 dB | -10.1 dB |             — |      -9 |    131069
 48000 -> 192000 | rodio 4096 spans |   -9.2 dB |  -13.0 dB |   25.8 dB |   21.7 dB | -29.6 dB |             — |     -72 |     16381
 48000 -> 192000 | rodio 1152 spans |    6.3 dB |    5.4 dB |   43.3 dB |   40.4 dB | -47.2 dB |             — |    -252 |      4605
 48000 -> 192000 | ideal linear     |  -61.9 dB |  -65.1 dB |   -4.9 dB |   -5.3 dB |  -5.1 dB |             — |       0 |      none
192000 -> 48000  | rodio 1 span     | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
192000 -> 48000  | rodio 4096 spans | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
192000 -> 48000  | rodio 1152 spans | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
192000 -> 48000  | ideal linear     | -154.9 dB | -159.7 dB | -153.8 dB | -158.9 dB |  -0.0 dB |       -0.0 dB |       0 |      none
192000 -> 44100  | rodio 1 span     |  -16.7 dB |  -21.3 dB |   13.4 dB |    9.3 dB | -13.9 dB |       -2.6 dB |       7 |      7527
192000 -> 44100  | rodio 4096 spans |   -7.9 dB |  -11.4 dB |   36.1 dB |   35.9 dB | -36.3 dB |       -2.6 dB |      19 |       941
192000 -> 44100  | rodio 1152 spans |   13.3 dB |    9.8 dB |   58.0 dB |   57.6 dB | -58.3 dB |       -2.6 dB |     134 |       265
192000 -> 44100  | ideal linear     |  -88.0 dB |  -91.2 dB |  -36.9 dB |  -38.9 dB |  -0.3 dB |       -2.6 dB |       0 |      none
 88200 -> 176400 | rodio 1 span     |  -30.9 dB |  -36.1 dB |    5.9 dB |    1.8 dB |  -9.6 dB |             — |      -6 |     65535
 88200 -> 176400 | rodio 4096 spans |  -13.0 dB |  -17.1 dB |   25.3 dB |   19.8 dB | -28.0 dB |             — |     -44 |      8191
 88200 -> 176400 | rodio 1152 spans |   -0.3 dB |   -2.3 dB |   46.4 dB |   44.3 dB | -49.1 dB |             — |    -154 |      2303
 88200 -> 176400 | ideal linear     |  -70.0 dB |  -70.0 dB |   -4.4 dB |   -4.4 dB |  -4.1 dB |             — |       0 |      none
```

**Findings**
1. **The design assumption holds.** rodio's `UniformSourceIterator` and `mixer` pass audio through bit-exact when rates and channel counts match. The only change is `-0.0` becoming `+0.0`, which converts to the same integer on every output format. Audio that is already at the stream rate therefore reaches the device untouched.
2. **R11: rodio's converter slips at every span restart.** It builds a new `SampleRateConverter` at each decoder packet. Each restart shifts the output by about one sample and gains or loses samples (`len err` up to −252 samples over 2 s at 48 → 192 kHz with 1152-frame spans). The first deviation is exactly at the first span boundary. In live playback this is a small step about every 93 ms with FLAC packets. The coherent metric reads the accumulated time shift as noise, so the "spans" rows overstate how loud it sounds, but the defect is real, and so is the drift in sample count, which affects position accuracy and gapless boundaries.
3. **Linear interpolation itself is not transparent**, even without the slip: 1 kHz residual −59 to −88 dB, and 1 to 8 dB of treble loss near Nyquist (the "ideal linear" rows).
4. **Downsampling has no anti-alias filter.** Integer ratios (96 → 48, 192 → 48) are exact in-band but fold content above the new Nyquist back at full level (stopband leak −0.0 dB). Other ratios leak at −2.6 to −4.7 dB.
5. **Phase 2 exit target for the fork's HQ resampler**, measured with the same kit: in-band residual and stopband leak ≤ −120 dB, HF gain within ±0.1 dB, `len err` 0, and no slips at span boundaries.

### Phase 2 result: the fork's HQ resampler

rubato 0.16.2 `FftFixedIn` (1024-frame chunks, `sub_chunks = 1`), measured with the same test kit. Every Phase 2 target is met with about 15 dB of margin:

| from → to | 1k resid | HF resid | HF gain | stopband leak | len err |
|---|---|---|---|---|---|
| 44.1 → 48 | −134.8 dB | −134.3 dB | 0.0 dB | — | 0 |
| 48 → 44.1 | −134.5 dB | −135.5 dB | 0.0 dB | −139.2 dB | 0 |
| 44.1 → 96 | −134.8 dB | −134.6 dB | 0.0 dB | — | 0 |
| 96 → 44.1 | −135.8 dB | −136.7 dB | 0.0 dB | −140.1 dB | 0 |
| 48 → 96 | −135.5 dB | −135.8 dB | 0.0 dB | — | 0 |
| 96 → 48 | −137.0 dB | −137.0 dB | 0.0 dB | −150.5 dB | 0 |
| 48 → 192 | −134.7 dB | −135.4 dB | 0.0 dB | — | 0 |
| 192 → 48 | −138.3 dB | −137.9 dB | 0.0 dB | −166.7 dB | 0 |
| 192 → 44.1 | −136.8 dB | −137.0 dB | 0.0 dB | −143.8 dB | 0 |
| 88.2 → 176.4 | −135.4 dB | −135.8 dB | 0.0 dB | — | 0 |

- Output is bit-identical whether the input arrives as one span or as 4096- or 1152-frame packets: no slips (R11 gone).
- Passthrough at equal rates is bit-exact; 720 edge-case length combinations (empty input, inputs shorter than the filter delay, chunk boundaries) are exact; no allocation on the audio thread in steady state.
- Speed (release build, 30 s stereo): 377× real time for 44.1 → 192 kHz, 364× for 48 → 192 kHz.
