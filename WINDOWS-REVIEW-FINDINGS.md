# Windows Code Review — Findings (2026-07-03)

Four parallel review passes over the Windows-specific code (screen capture, MediaFoundation
encoding, decode/render, desktop app + camera). All HIGH findings below were verified
against source by a second pass; the decoder root-cause was additionally reproduced
empirically against a real recording (`demo.cap`, 204 frames) on this machine.

Cross-corroboration note: the capture and encoding reviews independently found the same
two defects (P0-4 texture race, P1-5 panic), raising confidence.

---

## The export warnings we observed, explained

During a GIF export we saw:

```
WARN cap_rendering::decoder::media_foundation: MediaFoundation decoder unhealthy, performance may degrade name="screen" consecutive_errors=0 texture_failures=0 total_decoded=0
WARN cap_rendering::decoder::media_foundation: MediaFoundation read_sample error: Stream error consecutive_errors=1
```

Neither warning meant what it said. MF decoded all 204 frames fine; the export was healthy.

1. **"unhealthy"** is a false positive: `record_request()` refreshes `last_request_time`
   immediately before `should_warn_unhealthy()` is evaluated
   (`crates/rendering/src/decoder/media_foundation.rs:296-306`), so the idle-grace check can
   never apply; the first request >5s after decoder spawn (routine during export startup)
   always warns with all-zero counters.
2. **"Stream error"** is a normal end-of-stream: the hand-rolled flag constants in
   `crates/video-decode/src/media_foundation.rs:771-772` are **swapped** relative to the
   Windows SDK (`MF_SOURCE_READERF_ERROR = 0x1`, `ENDOFSTREAM = 0x2`; the code has them
   reversed). EOS is reported as "Stream error"; worse, a **real stream error is silently
   treated as EOS** — see P0-1.

---

## P0 — fix first (silent data corruption / loss)

### P0-1. Swapped MF_SOURCE_READERF constants → real decode errors are invisible
`crates/video-decode/src/media_foundation.rs:771-772`
Real errors (flags=0x1) are classified as EOS → `Ok(None)` → decode loop exits cleanly,
health monitor never increments, stale/black frames served, export "succeeds".
**Fix**: use `windows::Win32::Media::MediaFoundation::{MF_SOURCE_READERF_ERROR, MF_SOURCE_READERF_ENDOFSTREAM}`
instead of hand-rolled constants. Also handle `CURRENTMEDIATYPECHANGED` (0x20, observed on
first sample) and `STREAMTICK` (0x100) explicitly instead of falling into the EOS path.

### P0-2. No mid-stream decoder fallback; black frames count as success
`crates/rendering/src/decoder/media_foundation.rs:462-499`
Once MF init succeeds there is no runtime switch to FFmpeg. A decoder failing every read
serves opaque-cache/black frames forever and the export completes "successfully". Combined
with P0-1, a genuinely broken MF session = silent black export. The desktop retry
(`apps/desktop/src-tauri/src/export.rs:479-514`) only triggers on export *error*.
**Fix**: propagate "0 frames decoded after N requests" as an error (CLI already fails on
rendered==0, `apps/cli/src/export.rs:385-393`); or fall back to FFmpeg after
MAX_CONSECUTIVE_ERRORS at runtime.

### P0-3. GIF/MOV exports cannot avoid the MF decoder; per-recording marker is dead code
`crates/export/src/settings.rs:28-33` — `force_ffmpeg_decoder()` hard-returns `false` for
Gif/Mov (the toggle only exists on Mp4). Meanwhile every fragmented remux writes a
`.force-ffmpeg-export` marker (`apps/desktop/src-tauri/src/recording.rs:3826-3838`) that
nothing reads (a test asserts it is ignored). The editor forces FFmpeg on Windows
(`crates/editor/src/editor_instance.rs:249`) precisely because MF was not trusted; exports
lost that protection.
**Fix**: add `force_ffmpeg_decoder` to Gif/Mov settings and/or honor the marker in
`should_force_ffmpeg_export`.

### P0-4. WGC pool texture released before the encoder consumes it → torn/wrong frames
`crates/recording/src/output_pipeline/win.rs:336-377` + `crates/scap-direct3d/src/lib.rs:590-633`
`get_frame` clones only the `ID3D11Texture2D` and drops the `ScreenFrame`, returning the
buffer to the 2-4 deep WGC pool, which overwrites it with subsequent captures before/while
the encoder copies (`video_processor.rs:231`). The frame-pacing branch holds `last_texture`
across many intervals, guaranteeing re-encoding of "whatever lands in that buffer next".
Crop path is worse: ONE shared `cropped_texture` is aliased by every in-flight frame.
Found independently by two reviewers.
**Fix**: keep the `ScreenFrame` alive until after `process_texture` submits the copy, or
`CopyResource` into an encoder-owned texture at receive time; round-robin pool for crop
output textures.

### P0-5. Window/area recording: crop box never clamped to display size → whole-recording black output
`crates/recording/src/sources/screen_capture/windows.rs:360-377`
Crop clamps to >=0 only. A window hanging off the right/bottom display edge (or spanning
monitors) produces an out-of-bounds `D3D11_BOX`; `CopySubresourceRegion` silently no-ops
and the pre-allocated output texture is never written — black/garbage for the entire
recording with zero errors. The screenshot path already clamps correctly
(`crates/recording/src/screenshot.rs:507-508`); the recording path never got the fix.
**Fix**: clamp right/bottom to `item.Size()` like screenshot.rs; derive `video_info` from
the clamped size.

### P0-6. Encoder never drained at stop → last ~100-250ms of every MF recording lost
`crates/enc-mediafoundation/src/video/h264.rs:643-648` (same in `hevc.rs:435-440`)
On stop: `NOTIFY_END_OF_STREAM` → `NOTIFY_END_STREAMING` → `COMMAND_FLUSH` with no
`MFT_MESSAGE_COMMAND_DRAIN` and no post-EOS `ProcessOutput` pumping. Frames still inside
the HW pipeline (2-8 typ.) are discarded; queued `HaveOutput` events never serviced. Audio
IS flushed properly → A/V end misalignment on every recording.
**Fix**: send `COMMAND_DRAIN`, pump `ProcessOutput` until `DrainComplete` /
`MF_E_TRANSFORM_NEED_MORE_INPUT`, then END_STREAMING/FLUSH.

---

## P1 — reliability (crashes, wedges, missing devices)

### P1-1. Camera MF/DS dedup replaces the MF entry with a clone of itself, dropping the DirectShow fallback
`crates/camera-windows/src/lib.rs:598-606`
`devices.push(mf_device.clone()); devices.swap_remove(i);` — the intended replacement with
`dshow_device` never happens; format-less MF devices stay and can never open
(`InvalidFormat`). Legacy capture cards / some virtual cams silently unusable.
**Fix**: `devices[i] = dshow_device;` (one line). Add dedup unit test.

### P1-2. DirectShow `TypeEnumerator::Next` reads an uninitialized/possibly-NULL out param — UB/crash
`crates/camera-directshow/src/lib.rs:1084`
Gates on `pcfetched.read()` (uninitialized *out* param; may legitimately be NULL when
`cMediaTypes==1`) instead of `cmediatypes`. Also writes `*typ` before its null-check and
assumes `KS_VIDEOINFOHEADER` unconditionally.
**Fix**: guard on `cmediatypes > 0`, null-check `pcfetched` before read/write.

### P1-3. `panic!` on unknown MF media event kills the encoder thread
`crates/enc-mediafoundation/src/video/h264.rs:637-639`
`MEError`, `METransformDrainComplete`, `METransformMarker`, or vendor events panic the
`windows-encoder` thread mid-recording. `hevc.rs:429-431` handles this correctly with
`warn!` — h264 is a regression. Found independently by two reviewers.
**Fix**: copy the HEVC behavior; treat `MEError` as an error return.

### P1-4. First failed pipeline task triggers `muxer.finish()` (trailer write) while other tasks still write packets
`crates/recording/src/output_pipeline/core.rs:1812-1830`
`finish_build` returns on FIRST task failure; `.then` closure calls `finish()` →
`write_trailer()` while mux-audio keeps calling `av_interleaved_write_frame` on the same
context — UB inside ffmpeg movenc (crash or corrupt file).
**Fix**: cancel/drain remaining tasks before `finish`; make finish a barrier.

### P1-5. Blocking `GetEvent` with no timeout defeats the 5s health monitor and can leave the MP4 without a moov
`crates/enc-mediafoundation/src/video/h264.rs:557` (hevc.rs:379)
Hung MFT (TDR/device removed) blocks forever; only the 10s `PIPELINE_STOP_TIMEOUT` saves
the app, thread leaked, trailer never written → whole recording unplayable.
**Fix**: `MF_EVENT_FLAG_NO_WAIT` loop with health/stop checks, or `BeginGetEvent`.

### P1-6. Camera-start wedge leaks blocked threads that keep the camera claimed
`crates/camera-mediafoundation/src/lib.rs:639-655`
`wait_for_event` blocks forever if the engine never fires (wedged driver, hot-unplug); the
4s init timeout only cancels the caller's future — capture thread + reaper thread leak per
retry, device stays busy until app exit. `stop_capturing` also never waits for
`PreviewStopped` nor calls `media_source.Shutdown()`.
**Fix**: `recv_timeout` + engine teardown on timeout; Shutdown on stop.

### P1-7. Device-lost only detected via `Item.Closed`; silent TDR = endless frozen recording
`crates/recording/src/sources/screen_capture/windows.rs:444-453`
FrameArrived errors are swallowed by the WinRT dispatcher; if frames stop arriving the
pacing loop duplicates the last frame indefinitely.
**Fix**: watchdog — no frame progress for N seconds while unpaused → check
`GetDeviceRemovedReason()` → feed the existing restart machinery.

### P1-8. Camera device poll (every 5s) leaks COM objects and re-activates every camera's media source
`crates/camera-mediafoundation/src/lib.rs:32-157` + poll at `apps/desktop/src-tauri/src/lib.rs:1597-1657`
No `Drop` on `DeviceSourcesIterator` (activates + CoTaskMem array leak), `GetAllocatedString`
results never freed, and `ActivateObject` per device per poll with no `Shutdown()` — makes
cameras appear busy to other apps, destabilizes flaky drivers, steady multi-hour leak.
Related: one broken/busy camera early in the list hides all later cameras
(`next()` returns `None` on activation error instead of `continue`, lib.rs:96-101).
**Fix**: Drop impl; read names off `IMFActivate` attributes without activating; `continue`
on per-device failure.

### P1-9. `todo!()` panic on MF-format/DS-device mismatch when opening a camera
`crates/camera-windows/src/lib.rs:387`
Format and device come from two separate enumerations that can disagree (esp. with P1-1) →
panic on the camera setup thread → opaque "BuildStreamCrashed".
**Fix**: return a proper error; carry the resolved device from format enumeration.

### P1-10. Timestamp unit mismatch: 100-ns values divided by QPF — only correct when QPF == 10 MHz
`crates/timestamp/src/win.rs:30-78` + `screen_capture/windows.rs:500-503`
WGC/WASAPI timestamps are 100-ns units; the anchor is raw QPC ticks; all math divides by
QPF. Works iff QPF==10MHz (common but not guaranteed: HPET via bcdedit, ACPI PM timer
3.579545 MHz, some VMs) — elsewhere the timeline is scaled wrongly or recording aborts on
"timestamp anomalies".
**Fix**: normalize `now()` to 100-ns units (`ticks * 10_000_000 / QPF`); log when
QPF != 10 MHz.

---

## P2 — quality (visible but not data-destroying)

- **BT.601 shaders on BT.709 content (both directions)** —
  `crates/rendering/src/shaders/nv12_to_rgba.wgsl:21-27`, `yuv420p_to_rgba.wgsl:25-31`,
  `rgba_to_nv12.wgsl:12-23` hardcode 601 coefficients; Windows recordings are tagged bt709
  (verified with ffprobe). Visible hue/saturation shift on every Windows preview/export,
  doubled on MP4 roundtrip. Select matrix from stream colorspace tag.
- **Encoder-side color config also wrong** — `enc-mediafoundation/src/video/video_processor.rs:98-105`
  builds the D3D11 video processor color-space bitfield with wrong bit positions (nominal
  range lands in the RGB_Range bit); YCbCr matrix never set → BT.601 conversion for HD, and
  the muxed stream carries no colorimetry tags.
- **MF frame numbering floors where FFmpeg rounds** — `rendering/decoder/media_foundation.rs:512-514`
  → one-frame temporal offset + duplicated first frame at 30fps (60fps works by luck). Round.
- **Health-monitor false positive** — `record_request()` ordering, see top section. Treat
  zero-attempts as healthy; evaluate idle grace against the *previous* request time.
- **Zero-copy decode path is dead code; every frame does GPU→CPU→GPU** —
  `video-decode/media_foundation.rs:478-515` never creates shared handles, so
  `has_valid_zero_copy_handles` is always false; plus an unconsumed compute dispatch and an
  uncalled `FramePool::recycle` (fresh textures+views per frame). Big export perf headroom.
- **DTS=PTS with B-frames never disabled** — `mediafoundation-ffmpeg/src/h264.rs:124-128` +
  no `ICodecAPI` config; vendor MFTs defaulting to B-frames produce non-monotonic DTS,
  errors swallowed (below) → stutter/missing frames. Set B-picture count 0.
- **Muxer write errors swallowed** — `output_pipeline/win.rs:395-397, 935-938`: write_sample
  failures logged and dropped; disk-full = "successful" empty recording. Fail after N
  consecutive errors. Related: `send_video_frame` treats a dead encoder channel as success
  (`win.rs:549-551`) — capture continues, every frame discarded.
- **Per-frame COM ref leak in video processor** — `video_processor.rs:238`
  (`ManuallyDrop` clone never released; refcount +1 per encoded frame; pins device forever).
- **Frame pool never `Recreate`d on ContentSize change** — `scap-direct3d/src/lib.rs:590-633,
  771-808`: resolution change mid-recording → mismatched CopyResource (undefined) exactly
  when the frame scaler engages.
- **Tray click stops recording on ANY mouse button** — `apps/desktop/src-tauri/src/tray.rs:984-994`:
  right-clicking the tray menu mid-recording stops the recording. Match Left+Up only. Also:
  no recording indicator in the tray on Windows (icon update early-returns).
- **Custom recordings folder ignored by the writer** — `recording.rs:1530` + `tray.rs:174-184`
  hard-code `app_data_dir()/recordings` while settings/recovery honor `recordings_path` →
  recordings land on the wrong drive, disk-space gate checks the wrong volume, recovery scans
  the wrong folder. Also multi-GB video under roaming %APPDATA% (should be local).
- **Mixed-DPI coordinate-space mixing (systemic)** — `windows.rs:592-624`,
  `scap-targets/platform/win.rs:57-60,154-174`, `fake_window.rs:244-285`: physical px compared
  against per-monitor "logical" rects; overlays/controls placed wrong on multi-monitor
  mixed-DPI setups; WebView rasterization scale locked to the primary monitor's DPI before
  the overlay is moved (`windows.rs:2824-2861` called at :1630, before placement at :1641).
- **Monochrome cursors (I-beam) capture fully transparent** — `recording/src/cursor.rs:560-866`:
  DrawIconEx of AND/XOR cursors leaves alpha=0 → cursor invisible in studio re-render during
  text editing. Derive alpha from the mask. Also: cursor captured even when hidden
  (`CURSOR_SHOWING` unchecked); PNG+SHA256 every ~16ms even when unchanged.
- **HMONITOR used as persistent DisplayId** — stale after the very topology changes that
  trigger restarts (`scap-targets/win.rs:128-135`); restart machinery then fails 3x and aborts.
  Persist the device name and re-resolve.
- **Seeks past EOF unclamped + keyframe-only seek cost** — `rendering/decoder/media_foundation.rs:321-328`;
  long-GOP recordings pay decode-from-keyframe on mid-recording GIF export start; can exceed
  frame timeout → duplicated frames.
- **RGB24 camera frames uploaded as RGBA textures** — `output_pipeline/win.rs:614, 1978-2009`:
  wrong stride math → skewed garbage + OOB read on the last row, for any DS webcam
  delivering RGB24 on the HW path.
- **FFmpeg directory fallback loads whole recording into RAM** — `video-decode/ffmpeg.rs:219-231`
  (multi-GB Vec for crashed-recording recovery). Stream-copy instead.

Plus assorted LOW items (dead `IGNORED_EXES` filter matching, MF software-encoder
constructors that can never work, missing sample durations, empty avcC in the unused
fragmented mode, PROPVARIANT leaks, capture adapter chosen by VRAM instead of the monitor's
adapter, `ModelID` deserialize unwrap, identical-webcam disambiguation, zero-fps division,
infinite loop potential in MF format matching) — see the per-area reports for detail.

## Clean areas (verified sound)

- QPC math in `crates/timestamp` (aside from P1-10 unit mismatch), disk-space Windows impl,
  cli-install shim logic, out-of-process muxer lifecycle (no orphan risk; parent-death →
  graceful finish), single-instance/deep-link, fake_window hit-testing, stride handling in
  scap-ffmpeg, cursor position math (negative coords, mixed DPI), A/V start alignment
  (VideoStartGate), segmented_stream finalization (with tests), hotkeys register/unregister
  balance, the ffmpeg software encoder fallback (defensive: even-dims, GOP, 709 tagging,
  PTS fixup).

## Suggested fix order for the GIF-to-disk workflow specifically

1. P0-1 (swap constants — two lines) + health-monitor ordering (small) → kills both spurious
   warnings AND unmasks real errors
2. P0-3 (let GIF exports force the FFmpeg decoder) → user-controllable escape hatch
3. P0-5 (clamp crop box) → prevents silent black window recordings
4. P0-4 (frame lifetime) + P0-6 (encoder drain) → recording correctness
5. P2 color matrix (601→709) → visible quality win on every export
