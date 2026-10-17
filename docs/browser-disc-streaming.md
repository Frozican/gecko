# Browser disc streaming: implementation plan

## Goal

Boot large, user-selected Wii disc images in the browser without copying the entire image into JavaScript/WASM memory or into OPFS. The initial target is a 3.39 GB Super Mario Galaxy ISO on Android Chrome.

## Current constraints confirmed in source

- `image::load_dvd(data: Vec<u8>)` accepts an entire image in memory.
- `Iso::parse(data: Vec<u8>)` reads the disc header and FST from the in-memory slice and retains the whole `Vec<u8>`.
- `Dvd::read_disc_into` and `Dvd::read_raw_disc` are synchronous.
- The web crate currently exports `start_emulator(rom_data: &[u8], ...)`, also requiring the entire image in memory.
- Browser `File.slice(...).arrayBuffer()` is asynchronous. Passing a File to a Worker and reading a few ranges proves the browser primitive works, but does not make synchronous Rust disc reads able to await those ranges.
- Copying the whole ISO to OPFS exceeded the browser's storage quota in the Android test, so full-file OPFS staging is not an acceptable baseline.

## Architecture decision

Do not add a superficial range-reader wrapper around `Iso`: it would still be impossible for synchronous `Dvd` reads to fetch a missing range from a browser File. Do not use full-file OPFS staging as the default.

The next implementation decision must account for both execution and rendering:

1. Prototype whether the web emulator can run in a worker while retaining a rendering path compatible with browsers that support the required WebGPU/offscreen APIs.
2. If worker execution is viable, evaluate an OPFS `SyncAccessHandle` backed by bounded/chunked storage; the selected browser File cannot be read synchronously directly, and whole-image copying must remain optional because of quota limits.
3. If worker rendering is not viable, refactor disc reads into a resumable/continuation-based design so the emulator can pause for asynchronous range reads without blocking the browser main thread. This is a much larger core change and must be scoped before implementation.
4. Add a bounded cache and explicit range/EOF/error handling only after the execution model is proven.
5. Keep ISO and RVZ support distinct: RVZ additionally requires random access to compressed groups and decompression of requested chunks.

## Acceptance criteria

- No allocation proportional to the entire 3.39 GB image in JS/WASM memory.
- No mandatory full-image OPFS copy.
- Reads spanning range boundaries and reads near EOF are correct.
- Invalid/truncated input reports an error rather than panicking on unchecked slices.
- CI builds the WASM crate.
- On-device test records browser/device, image format, memory/storage behavior, and whether the game reaches its first rendered frame.

## Status

This commit documents the first source-level investigation and the architecture gate. It does not claim that streaming or Wii game boot is implemented. The web front end and deployment workflow should not switch to this branch until a working prototype passes the criteria above.

## On-device NKit ISO findings (2026-10-17)

The browser-only range test was run on the target 3,392,176,128-byte Super Mario Galaxy NKit ISO in Android Chrome. It read the disc header and partition metadata without uploading or copying the complete file.

Observed values:
- Wii magic at disc-header offset 0x18 is recognized.
- Header FST offset/size are zero, so the GameCube-style header FST path is not applicable.
- Partition group 0 has one entry; partition table at 0x40020 points to a type-0 partition at 0x58000.
- Partition metadata reports the data-area offset 0x20000 relative to the partition, giving absolute file offset 0x78000.
- The reported data-area length is 3,391,684,608 bytes, ending exactly at the selected file's size.
- The first bytes at 0x78000 include the disc header / game ID (RMGE01). This alone does not establish how the remainder of the NKit payload is laid out or whether it is plaintext.
- The test has only read metadata and small ranges. It has not decrypted partition payload, located the decrypted FST, or booted the game.

## Follow-up sector-mapping test (2026-10-17)

A targeted browser Worker test tried the common Wii physical-sector mapping: each 0x8000-byte physical sector contributes 0x7C00 bytes of logical user data after skipping its first 0x400 bytes.

Observed on the selected NKit file:
- Raw bytes at 0x78000 begin with `RMGE01` and the Wii magic.
- Applying the assumed mapping to logical offset 0 returns 32 zero bytes, not a plausible boot header.
- The raw boot-header candidate at 0x78000 has FST fields `0x18EF00`, `0x4F67`, `0x4F67`; shifting these header values left by two gives `0x63BC00`, `0x13D9C`, `0x13D9C`.
- Reading the candidate FST offset through the assumed mapping yields bytes whose root entry does not validate.
- The test did not decrypt AES-encrypted partition data and did not validate NKit repair semantics.

Conclusion: the simple 0x400-byte-skip mapping is **not validated for this NKit file** and must not be used as the production reader. The result does not prove that the image is invalid. It shows that NKit layout and/or partition decryption/sector interpretation must be resolved first.

## Updated next technical gate

Inspect and reuse Gecko's existing RVZ partition-reading logic as a reference for the required transformations. In `crates/image/src/rvz.rs`, Wii RVZ reading uses partition metadata to select chunks, strips per-chunk hash-exception metadata, unpacks stored data, and exposes decrypted logical user-data bytes through `read_partition_decrypted`. That path is specific to RVZ chunk structures and cannot simply be applied to an NKit ISO, but it is the best in-repository reference for the semantics that a streaming reader must preserve.

Next, establish the exact NKit ISO layout and whether the file can be read using an existing NKit-aware implementation or must first be normalized to a supported format. Validate a decrypted partition header, apploader, and FST before changing the synchronous `Dvd` abstraction. Do not switch the public Pages build to an experimental branch until a game boot succeeds on-device.

This remains a design and diagnostic checkpoint; streaming and game boot are not implemented.


## Multi-file NKit range probe (2026-10-17)

A follow-up browser test on the same Super Mario Galaxy NKit ISO scanned the FST and sampled the beginning, middle, and end of seven known audio-stream files.

Observed:
- 21 of 21 requested 16-byte ranges returned the expected byte count.
- No target files were missing.
- Total sampled payload was 336 bytes, in addition to small disc/FST metadata reads.
- Sampled file starts included plausible `STRM` headers.
- The test used `File.slice()` ranges and did not copy whole files or the complete ISO.

This increases confidence in the tested FST offset arithmetic and these specific file-to-ISO mappings. It does **not** validate all file ranges, NKit gap/repair semantics, every partition structure, or bootability.

## Next implementation gate

The browser-side range primitive and several NKit file mappings have now been demonstrated. The blocker is no longer whether Chrome can read selected bytes; it is how to provide correct synchronous reads to Gecko without retaining the whole ISO. Before changing the core reader, inspect the complete boot path and decide whether a worker plus bounded synchronous cache can be supported with the available browser APIs. The target Android browser has no SharedArrayBuffer/cross-origin isolation, so do not assume a shared-memory worker bridge is available. Keep the public Pages build unchanged until the prototype can validate metadata, apploader, FST, and reach a rendered game frame.


## Bounded browser range-cache prototype (2026-10-17)

The website repository now includes an experimental browser test at `nkit-bounded-cache-test.html` with worker `nkit-bounded-cache-worker.js`. It uses 256 KiB physical ISO chunks and a 4 MiB LRU payload cap, compares cached reads against direct `File.slice()` reads, and probes start/middle/end plus a physical chunk-boundary range for four known files. It reports hit/miss counts and peak cached payload.

This prototype is deliberately only a JavaScript I/O experiment. It does not solve the key integration mismatch: Gecko's `Dvd::read_disc_into` and `read_raw_disc` are synchronous Rust methods, whereas browser File reads are asynchronous. Without SharedArrayBuffer/cross-origin isolation, a synchronous JS bridge that blocks waiting for async reads is not a viable assumption. The Android browser test also found worker WebGPU unavailable, so moving the existing WebGPU frontend wholly into a worker is not presently validated.

## Core integration decision

Do not wire the JS LRU cache into Gecko by pretending asynchronous reads are synchronous. A production solution requires one of these explicit architectural changes:

1. **Resumable/asynchronous emulation:** propagate pending disc reads through the CPU/system scheduler and resume emulation after the browser read completes. This is a broad core refactor, but is compatible with the observed browser constraints.
2. **Supported synchronous backing store:** first prove a browser API and storage arrangement that synchronously serves requested ranges without copying the full 3.39 GB image. The current Android test has not demonstrated such a backing store.

The immediate next code investigation is to map every call site that consumes `Dvd::read_disc_into`/ `read_raw_disc` during boot and determine whether emulation can yield and resume around disc I/O. Until that is designed and tested, the range-cache prototype should remain a diagnostic tool, not be described as a working streaming emulator.


## Bounded range-cache on-device result (2026-10-17)

The Android Chrome test of `nkit-bounded-cache-test.html` completed successfully against the selected RMGE01 NKit ISO:

- 16 cached-vs-direct range comparisons passed; 0 failed.
- The test covered start, middle, end, and physical cache-chunk-crossing ranges in four audio-stream files.
- Cache chunk size: 262,144 bytes (256 KiB); configured payload cap: 4,194,304 bytes (4 MiB).
- Peak cached payload: 3,932,160 bytes, below the cap.
- Cache counters: 5 hits and 15 misses.
- The tested payload was fetched in bounded chunks; no whole-ISO copy was made.

This confirms the JavaScript LRU cache's bounded behavior and sampled byte-for-byte equivalence for these tested ranges. It does not validate every logical file range, decode all NKit structures, connect the cache to Gecko, or demonstrate a game boot.

## Immediate next step: trace the synchronous read call graph

Before implementing a yield/resume design, map the concrete call sites and boot-time dependencies of `Dvd::read_disc_into` and `Dvd::read_raw_disc`, including apploader loading, FST/file access, DVD command handling, and any reads performed while the CPU/system scheduler is running. For each call site, record whether it can return a pending-read result without exposing partially filled buffers or violating device timing.

Do not change the trait to async in isolation: this is a broad interface used by the core, and an async read must be propagated to the emulation scheduler and DVD command state machine. Keep the public Pages workflow unchanged until this path can be built and tested end-to-end.


## Synchronous read call-graph investigation (2026-10-17)

Source inspection identified two separate boot/runtime read paths. This is the concrete scope that must be addressed before a browser File range source can replace the in-memory ISO.

### 1. Wii HLE boot path

- `crates/gecko/src/wii.rs::apploader_hle_run` reads the TMD through `Dvd::tmd_ios_title_id` and `Dvd::tmd_title_id` during low-memory setup. Those default trait methods call synchronous `read_raw_disc`.
- The same function reads the apploader image synchronously into emulated RAM with `read_disc_into`.
- `Wii::run_apploader` repeatedly calls `emu.run_until(...)` to execute apploader functions. After each `Apploader_Main` return, it reads destination address, byte length, and a word-based disc offset from emulated memory, then immediately copies that range with `dvd.read_disc_into`.
- This is a boot-time control loop, not merely a background data fetch. It must become a resumable boot state machine: preserve emulated CPU state and the requested read, await the range, copy it into RAM, then continue with the next apploader phase. A direct async replacement cannot be awaited inside the current synchronous loop.

### 2. Runtime DVD Interface (DI)

- `crates/gecko/src/dvd.rs::start_transfer` resolves a DI command and calls `process_dvd_command` synchronously before scheduling `DiTransferDone`.
- `process_dvd_command` handles `ReadSectorData` and `ReadDiskId` by obtaining an emulated RAM buffer and calling `dvd.read_disc_into` immediately.
- The existing scheduler already models a delay before the transfer-complete interrupt, but the data has been copied before that delay. Browser streaming needs a pending-transfer state: capture command/source/destination/length, request the bytes asynchronously, copy the completed bytes into emulated RAM, and only then signal transfer completion according to device timing. The CPU must not observe a completed transfer before the data is available.
- Other synchronous disc calls may be reached through the `Dvd` trait defaults and must be audited as part of the same change.

### Implementation implication

The viable direction under the observed Android constraints is **not** a synchronous Rust-to-JavaScript bridge and not a worker that blocks waiting for `File.slice().arrayBuffer()`. Instead, split the work into two explicit resumable state machines:

1. **Boot/apploader state machine:** stages for metadata reads, apploader-code load, entry/init/main calls, pending file read, and final entrypoint. Each stage advances only after its required data is available.
2. **Runtime DI transfer state machine:** retain the command and transfer parameters while a range is pending; on completion, write the returned bytes into emulated RAM and resume the normal scheduler/interrupt flow.

The JavaScript side should own the selected File and bounded LRU range cache, and return each requested range as a message to Rust/WASM. The Rust side must never retain the full ISO. Before coding, define message IDs, cancellation/error behavior, EOF handling, and how an in-flight transfer pauses CPU execution while keeping rendering/input responsive.

This investigation maps the principal call sites but is not yet an implementation. No Rust source or public Pages workflow has been changed by this section.


## Web frontend integration blocker (2026-10-17)

A further source audit found that `crates/web/src/lib.rs` currently exposes
`start_emulator(rom_data, filename, dsp_irom)`, but its non-IPL path parses the
input as a GameCube DOL (`Dol::parse` / `GameCube::with_image`). The web
`App` also stores a `GameCube`, not a Wii system. Therefore, adding a range
reader alone cannot boot a Wii disc in the current web frontend: the frontend
needs a Wii-disc launch path and must construct `Wii::apploader_hle(...)`
using a disc implementation that supports the required metadata and range reads.

This is a distinct prerequisite from asynchronous I/O. Keep the implementation
split into testable layers:

1. Add a Wii-disc launch/API path without changing the existing GameCube DOL
   path.
2. Introduce a disc-range request broker with request IDs, bounded-cache reads,
   error/EOF reporting, and cancellation.
3. Make Wii apploader boot resumable across pending reads; preserve the CPU
   state and pending destination/length/offset.
4. Make runtime DI reads resumable and only signal transfer completion after
   bytes have been copied to emulated RAM.
5. Build and run automated checks before connecting the new launch path to the
   public Pages site.

No frontend or emulator runtime code has been changed by this audit. In
particular, no Wii boot or game-play success is claimed.


## First browser I/O primitive (2026-10-17)

Added `crates/web/src/disc_stream.rs` with a browser `FileRangeReader`:

- uses asynchronous `File.slice(...).array_buffer()` reads, so it does not block the JS event loop or require SharedArrayBuffer;
- reads in 256 KiB chunks and maintains a 16-chunk LRU cache (4 MiB maximum cached payload);
- validates overflow and end-of-file bounds and returns explicit JavaScript errors for invalid/incomplete reads;
- keeps the selected browser `File` on the web side and does not copy the entire ISO into WASM memory.

The web crate now declares `js-sys` and includes the module. This is intentionally only the I/O primitive: it is not yet called by the frontend, does not implement `image::Dvd`, and cannot boot a Wii game by itself. The boot/apploader and runtime DI state machines described above remain required.

**Validation status:** source was committed to `feat/browser-disc-streaming`, but a Rust/WASM build and device test have not yet been run. The public Pages workflow remains unchanged.


## WASM-exposed range-reader API (2026-10-17)

The browser crate now exports `BrowserDiscReader` to JavaScript. Construct it
with a browser `File`, read its `size` property, and call
`await reader.read_range(offset, length)` to receive only that range as a
`Uint8Array`. The reader still uses 256 KiB chunks and a 4 MiB LRU cache and
rejects invalid offsets, overflow, and reads beyond EOF.

This makes the Rust/WASM range-reader itself callable from a browser harness,
so the next validation can compare its bytes with direct `File.slice()`
reads on the actual NKit file. It is still not connected to `image::Dvd`,
the Wii boot path, or the public Pages frontend. The existing GameCube launch
path remains unchanged.
