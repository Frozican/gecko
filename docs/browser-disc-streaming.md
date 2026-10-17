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
