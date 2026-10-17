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
- The first bytes at 0x78000 include the disc header / game ID (RMGE01), consistent with the beginning of a Wii data-partition sector; this is not proof that later payload bytes are plaintext.
- The test has only read metadata and a 64-byte prefix. It has not decrypted partition payload, located the decrypted FST, or booted the game.

## Updated next technical gate

Before integrating streaming into Dvd, implement and validate a small, isolated Wii-sector reader against this selected-file layout. Wii partition sectors are physically 0x8000 bytes, with 0x400 bytes of hash/metadata area and 0x7C00 bytes of user data per sector. The reader must map logical user-data offsets to physical sector offsets and correctly handle the encrypted payload and its required title-key/IV handling. It must also distinguish NKit-specific layout/repair behavior from a standard raw Wii ISO. Only after a test can read and validate the decrypted partition header/apploader and locate the FST should the core Dvd abstraction be changed.

This is a more specific implementation gate than the original plan; it does not imply that the sector reader or game boot is implemented.