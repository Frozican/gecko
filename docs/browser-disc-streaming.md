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
