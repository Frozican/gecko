//! Bounded asynchronous reads from a browser-selected disc image.
//!
//! This is the browser-side I/O primitive for future resumable Wii boot/runtime
//! work. It deliberately does not implement image::Dvd: Gecko's current Dvd
//! interface is synchronous, so callers must await reads and resume emulation
//! rather than trying to block the browser event loop.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::File;

const CHUNK_SIZE: usize = 256 * 1024;
const MAX_CACHED_CHUNKS: usize = 16;
// Prevent a JavaScript caller from requesting a multi-gigabyte allocation in one read.
// Streaming consumers should request bounded chunks and await each read.
const MAX_READ_LENGTH: usize = 16 * 1024 * 1024;

fn validate_read_length(length: usize) -> Result<(), &'static str> {
    if length > MAX_READ_LENGTH {
        Err("disc read exceeds the 16 MiB per-request limit")
    } else {
        Ok(())
    }
}
const MAX_NKIT_PARTITIONS: u32 = 10_000;
const MAX_FST_SIZE: usize = 64 * 1024 * 1024;

struct Cache {
    chunks: HashMap<u64, Vec<u8>>,
    lru: VecDeque<u64>,
}

impl Cache {
    fn new() -> Self {
        Self { chunks: HashMap::new(), lru: VecDeque::new() }
    }

    fn copy_range(&mut self, index: u64, start: usize, end: usize, output: &mut Vec<u8>) -> bool {
        let Some(bytes) = self.chunks.get(&index) else {
            return false;
        };
        if start > end || end > bytes.len() {
            return false;
        }

        // Copy only the requested bytes. Cloning the full 256 KiB cache entry
        // on every hit needlessly multiplied temporary memory during FST scans.
        output.extend_from_slice(&bytes[start..end]);
        self.lru.retain(|&item| item != index);
        self.lru.push_back(index);
        true
    }

    fn insert(&mut self, index: u64, bytes: Vec<u8>) {
        if self.chunks.contains_key(&index) {
            self.lru.retain(|&item| item != index);
        } else {
            while self.chunks.len() >= MAX_CACHED_CHUNKS {
                if let Some(oldest) = self.lru.pop_front() {
                    self.chunks.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        self.chunks.insert(index, bytes);
        self.lru.push_back(index);
    }
}

/// Reads only requested ranges of a browser File and keeps at most 4 MiB of
/// 256 KiB chunks in memory. All I/O is asynchronous; no SharedArrayBuffer or
/// cross-origin isolation is required.
pub(crate) struct FileRangeReader {
    file: File,
    cache: RefCell<Cache>,
}

impl FileRangeReader {
    pub(crate) fn new(file: File) -> Self {
        Self { file, cache: RefCell::new(Cache::new()) }
    }

    pub(crate) fn len(&self) -> u64 {
        self.file.size() as u64
    }

    pub(crate) async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, JsValue> {
        validate_read_length(length).map_err(JsValue::from_str)?;
        let end = offset.checked_add(length as u64)
            .ok_or_else(|| JsValue::from_str("disc range overflow"))?;
        if end > self.len() {
            return Err(JsValue::from_str("disc range extends beyond end of file"));
        }
        if length == 0 { return Ok(Vec::new()); }

        let mut output = Vec::with_capacity(length);
        let mut cursor = offset;
        while cursor < end {
            let chunk_index = cursor / CHUNK_SIZE as u64;
            let chunk_start = chunk_index * CHUNK_SIZE as u64;
            let within = (cursor - chunk_start) as usize;
            let expected_chunk_len = ((self.len() - chunk_start).min(CHUNK_SIZE as u64)) as usize;
            let take = ((end - cursor) as usize).min(expected_chunk_len.saturating_sub(within));
            if take == 0 {
                return Err(JsValue::from_str("browser returned an incomplete disc chunk"));
            }

            let cache_hit = {
                self.cache.borrow_mut().copy_range(
                    chunk_index,
                    within,
                    within + take,
                    &mut output,
                )
            };
            if !cache_hit {
                let slice_end = (chunk_start + CHUNK_SIZE as u64).min(self.len());
                let blob = self.file.slice_with_f64_and_f64(chunk_start as f64, slice_end as f64)?;
                let array_buffer = JsFuture::from(blob.array_buffer()).await?;
                let bytes = js_sys::Uint8Array::new(&array_buffer).to_vec();
                if bytes.len() != expected_chunk_len {
                    return Err(JsValue::from_str("browser returned an incomplete disc chunk"));
                }
                self.cache.borrow_mut().insert(chunk_index, bytes);
                let copied = {
                    self.cache.borrow_mut().copy_range(
                        chunk_index,
                        within,
                        within + take,
                        &mut output,
                    )
                };
                if !copied {
                    return Err(JsValue::from_str("failed to cache browser disc chunk"));
                }
            }
            cursor += take as u64;
        }
        Ok(output)
    }
}

fn be_u32(bytes: &[u8], offset: usize) -> Result<u32, JsValue> {
    let field = bytes.get(offset..offset + 4)
        .ok_or_else(|| JsValue::from_str("truncated NKit metadata"))?;
    Ok(u32::from_be_bytes(field.try_into().unwrap()))
}

/// JavaScript-facing range reader for RVZ files. Metadata and compressed data
/// blocks are fetched independently; the complete RVZ file is never copied into WASM.
#[wasm_bindgen::prelude::wasm_bindgen]
pub struct BrowserRvzReader {
    inner: FileRangeReader,
    index: RefCell<Option<image::rvz::RvzRangeIndex>>,
}

#[wasm_bindgen::prelude::wasm_bindgen]
impl BrowserRvzReader {
    #[wasm_bindgen::prelude::wasm_bindgen(constructor)]
    pub fn new(file: File) -> BrowserRvzReader {
        BrowserRvzReader { inner: FileRangeReader::new(file), index: RefCell::new(None) }
    }

    #[wasm_bindgen::prelude::wasm_bindgen(getter)]
    pub fn size(&self) -> f64 { self.inner.len() as f64 }

    /// Fetch only the RVZ header and compressed raw/group tables.
    pub async fn open(&self) -> Result<String, JsValue> {
        const HEADER_LEN: usize = 0x11c;
        const H1: usize = 0x48;
        let header = self.inner.read_range(0, HEADER_LEN).await?;
        if header.get(0..4) != Some(b"RVZ\x01") {
            return Err(JsValue::from_str("not an RVZ file (bad magic)"));
        }
        let raw_offset = be_u64(&header, H1 + 0xb8)?;
        let raw_size = be_u32(&header, H1 + 0xc0)? as usize;
        let groups_offset = be_u64(&header, H1 + 0xc8)?;
        let groups_size = be_u32(&header, H1 + 0xd0)? as usize;
        if raw_size > MAX_READ_LENGTH || groups_size > MAX_READ_LENGTH {
            return Err(JsValue::from_str("RVZ metadata table exceeds 16 MiB safety limit"));
        }
        let raw_payload = self.inner.read_range(raw_offset, raw_size).await?;
        let groups_payload = self.inner.read_range(groups_offset, groups_size).await?;
        let index = image::rvz::RvzRangeIndex::from_ranges(&header, &raw_payload, &groups_payload)
            .map_err(|error| JsValue::from_str(&error))?;
        let summary = format!(
            "RVZ range index ready\nFile size: {} bytes\nLogical disc size: {} bytes\nChunk size: {} bytes\nCompression type: {}\nRaw-data entries and group table parsed without loading the whole file. This is the block I/O layer; Wii partition selection/decryption and emulator DI integration are still separate steps.",
            self.inner.len(), index.iso_size, index.chunk_size, index.compression_type
        );
        *self.index.borrow_mut() = Some(index);
        Ok(summary)
    }

    /// Fetch and decode exactly the compressed RVZ block containing a logical disc offset.
    pub async fn read_block(&self, disc_offset: f64) -> Result<js_sys::Uint8Array, JsValue> {
        if !disc_offset.is_finite() || disc_offset < 0.0 || disc_offset.fract() != 0.0 || disc_offset > u64::MAX as f64 {
            return Err(JsValue::from_str("disc offset must be a non-negative integer"));
        }
        let (descriptor, group_offset, output_size, compression_type) = {
            let index = self.index.borrow();
            let index = index.as_ref().ok_or_else(|| JsValue::from_str("call open() before reading RVZ blocks"))?;
            let (descriptor, group_offset, output_size) = index.block_for_disc_offset(disc_offset as u64)
                .map_err(|error| JsValue::from_str(&error))?;
            (descriptor, group_offset, output_size, index.compression_type)
        };
        let payload = self.inner.read_range(descriptor.file_offset, descriptor.stored_size as usize).await?;
        let decoded = image::rvz::decode_block_payload(
            &payload, descriptor, compression_type, output_size, group_offset
        ).map_err(|error| JsValue::from_str(&error))?;
        Ok(js_sys::Uint8Array::from(decoded.as_slice()))
    }
}

fn be_u64(bytes: &[u8], offset: usize) -> Result<u64, JsValue> {
    let field = bytes.get(offset..offset + 8)
        .ok_or_else(|| JsValue::from_str("truncated RVZ header"))?;
    Ok(u64::from_be_bytes(field.try_into().unwrap()))
}

/// JavaScript-facing handle for bounded disc reads and metadata probes.
#[wasm_bindgen::prelude::wasm_bindgen]
pub struct BrowserDiscReader {
    inner: FileRangeReader,
}

#[wasm_bindgen::prelude::wasm_bindgen]
impl BrowserDiscReader {
    #[wasm_bindgen::prelude::wasm_bindgen(constructor)]
    pub fn new(file: File) -> BrowserDiscReader {
        BrowserDiscReader { inner: FileRangeReader::new(file) }
    }

    #[wasm_bindgen::prelude::wasm_bindgen(getter)]
    pub fn size(&self) -> f64 {
        self.inner.len() as f64
    }

    pub async fn read_range(
        &self,
        offset: f64,
        length: u32,
    ) -> Result<js_sys::Uint8Array, JsValue> {
        validate_read_length(length as usize).map_err(JsValue::from_str)?;
        if !offset.is_finite() || offset < 0.0 || offset.fract() != 0.0 {
            return Err(JsValue::from_str("disc offset must be a non-negative integer"));
        }
        if offset > u64::MAX as f64 {
            return Err(JsValue::from_str("disc offset is too large"));
        }
        let bytes = self.inner.read_range(offset as u64, length as usize).await?;
        Ok(js_sys::Uint8Array::from(bytes.as_slice()))
    }

    /// Inspect NKit/Wii boot metadata without loading the disc image into memory.
    /// This is diagnostic only; it does not decode NKit gap/hash records or boot a game.
    pub async fn probe_nkit(&self) -> Result<String, JsValue> {
        let marker = self.inner.read_range(0x200, 4).await?;
        if marker.as_slice() != b"NKIT" {
            return Err(JsValue::from_str("NKit marker not found at offset 0x200"));
        }

        let table = self.inner.read_range(0x40000, 0x40).await?;
        let mut partition_offset = None;
        for slot in 0..4usize {
            let count = be_u32(&table, slot * 8)?;
            let table_words = be_u32(&table, slot * 8 + 4)?;
            if count == 0 || count > MAX_NKIT_PARTITIONS || table_words == 0 {
                continue;
            }
            let entries_offset = (table_words as u64).checked_mul(4)
                .ok_or_else(|| JsValue::from_str("NKit partition table offset overflow"))?;
            let entries_len = (count as usize).checked_mul(8)
                .ok_or_else(|| JsValue::from_str("NKit partition table size overflow"))?;
            if entries_len > 0x10000 {
                continue;
            }
            let entries = self.inner.read_range(entries_offset, entries_len).await?;
            for index in 0..count as usize {
                if be_u32(&entries, index * 8 + 4)? == 0 {
                    partition_offset = Some((be_u32(&entries, index * 8)? as u64) * 4);
                    break;
                }
            }
            if partition_offset.is_some() { break; }
        }
        let partition_offset = partition_offset
            .ok_or_else(|| JsValue::from_str("type-0 NKit data partition not found"))?;

        let partition_header = self.inner.read_range(partition_offset, 0x300).await?;
        let data_start_words = be_u32(&partition_header, 0x2b8)?;
        let data_start = partition_offset.checked_add((data_start_words as u64) * 4)
            .ok_or_else(|| JsValue::from_str("NKit data start overflow"))?;
        let boot = self.inner.read_range(data_start, 0x440).await?;
        let game_id = std::str::from_utf8(&boot[0..6])
            .map_err(|_| JsValue::from_str("Wii boot header has invalid game ID"))?;
        if !game_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(JsValue::from_str("Wii boot header game ID contains invalid characters"));
        }
        if boot.get(0x18..0x1c) != Some(&[0x5D, 0x1C, 0x9E, 0xA3][..]) {
            return Err(JsValue::from_str("Wii magic does not match at boot-header offset 0x18"));
        }
        // Probe the fixed apploader header and main DOL header too. This verifies
        // that the browser can fetch the next boot-critical structures without
        // loading the complete image; it does not execute them or restore NKit gaps.
        let apploader_offset = data_start.checked_add(0x2440)
            .ok_or_else(|| JsValue::from_str("apploader offset overflow"))?;
        let apploader = self.inner.read_range(apploader_offset, 0x20).await?;
        let apploader_date = String::from_utf8_lossy(&apploader[0..10]).trim_end_matches('\0').to_string();
        let apploader_code_size = be_u32(&apploader, 0x14)? as u64;
        let apploader_trailer_size = be_u32(&apploader, 0x18)? as u64;
        let apploader_total = 0x20u64.checked_add(apploader_code_size)
            .and_then(|n| n.checked_add(apploader_trailer_size))
            .ok_or_else(|| JsValue::from_str("apploader size overflow"))?;
        if apploader_total > 32 * 1024 * 1024 {
            return Err(JsValue::from_str("apploader metadata exceeds diagnostic safety limit"));
        }
        let apploader_end = apploader_offset.checked_add(apploader_total)
            .ok_or_else(|| JsValue::from_str("apploader range overflow"))?;
        if apploader_end > self.inner.len() {
            return Err(JsValue::from_str("apploader range extends beyond end of disc image"));
        }

        let dol_offset = (be_u32(&boot, 0x420)? as u64).checked_mul(4)
            .ok_or_else(|| JsValue::from_str("DOL offset overflow"))?;
        let dol_absolute_offset = data_start.checked_add(dol_offset)
            .ok_or_else(|| JsValue::from_str("absolute DOL offset overflow"))?;
        let dol_header = self.inner.read_range(dol_absolute_offset, 0x100).await?;
        let mut dol_end = 0x100u64;
        for section in 0..18usize {
            let offset_field = if section < 7 { section * 4 } else { 0x1c + (section - 7) * 4 };
            let size_field = if section < 7 { 0x90 + section * 4 } else { 0xac + (section - 7) * 4 };
            let file_offset = be_u32(&dol_header, offset_field)? as u64;
            let file_size = be_u32(&dol_header, size_field)? as u64;
            if file_size == 0 { continue; }
            let end = file_offset.checked_add(file_size)
                .ok_or_else(|| JsValue::from_str("DOL section range overflow"))?;
            if end > 256 * 1024 * 1024 {
                return Err(JsValue::from_str("DOL section exceeds diagnostic safety limit"));
            }
            dol_end = dol_end.max(end);
        }
        if dol_absolute_offset.checked_add(dol_end).is_none_or(|end| end > self.inner.len()) {
            return Err(JsValue::from_str("main DOL section ranges extend beyond end of disc image"));
        }

        let fst_offset = (be_u32(&boot, 0x424)? as u64) * 4;
        let fst_size = (be_u32(&boot, 0x428)? as usize).checked_mul(4)
            .ok_or_else(|| JsValue::from_str("FST size overflow"))?;
        if fst_size < 12 || fst_size > MAX_FST_SIZE {
            return Err(JsValue::from_str("FST size is outside the diagnostic probe limit"));
        }
        let fst_absolute_offset = data_start.checked_add(fst_offset)
            .ok_or_else(|| JsValue::from_str("absolute FST offset overflow"))?;
        let fst_end = fst_absolute_offset.checked_add(fst_size as u64)
            .ok_or_else(|| JsValue::from_str("FST range overflow"))?;
        if fst_end > self.inner.len() {
            return Err(JsValue::from_str("FST range extends beyond end of disc image"));
        }
        let fst = self.inner.read_range(fst_absolute_offset, fst_size).await?;
        let entry_count = be_u32(&fst, 8)? as usize;
        let string_table_offset = entry_count.checked_mul(12)
            .ok_or_else(|| JsValue::from_str("FST entry count overflow"))?;
        if entry_count == 0 || string_table_offset > fst.len() {
            return Err(JsValue::from_str("invalid FST root entry or string table"));
        }

        let mut leaf_files = 0usize;
        let mut matching_stream_names = Vec::new();
        for index in 1..entry_count {
            let entry_offset = index * 12;
            let word = be_u32(&fst, entry_offset)?;
            if word >> 24 == 1 { continue; }
            leaf_files += 1;
            let name_offset = (word & 0x00ff_ffff) as usize;
            let name_start = string_table_offset.checked_add(name_offset)
                .ok_or_else(|| JsValue::from_str("FST filename offset overflow"))?;
            if name_start >= fst.len() { continue; }
            let name_end = fst[name_start..].iter().position(|&b| b == 0)
                .map(|n| name_start + n).unwrap_or(fst.len());
            let name = String::from_utf8_lossy(&fst[name_start..name_end]);
            if name.to_ascii_lowercase().contains("strm") && matching_stream_names.len() < 8 {
                matching_stream_names.push(name.into_owned());
            }
        }

        Ok(format!(
            "NKit metadata probe: PASS\nDisc ID: {game_id}\nISO size: {} bytes\nPartition offset: 0x{partition_offset:X}\nPartition data start: 0x{data_start:X}\nApploader header: PASS (date: {apploader_date}, code: {apploader_code_size} bytes, trailer: {apploader_trailer_size} bytes, range in file)\nMain DOL header: PASS (relative offset: 0x{dol_offset:X}, declared sections end at +0x{dol_end:X})\nFST offset (partition-relative): 0x{fst_offset:X}\nFST size: {fst_size} bytes\nFST entries: {entry_count}\nLeaf file entries scanned: {leaf_files}\nFilenames containing 'strm' (up to 8): {}\n\nDiagnostic only: this confirms metadata/ranges can be read, but does not restore NKit gaps/hash data and does not boot the game.",
            self.inner.len(),
            if matching_stream_names.is_empty() { "(none in this scan)".to_string() } else { matching_stream_names.join(", ") }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_read_length, Cache, MAX_READ_LENGTH};

    #[test]
    fn read_length_limit_accepts_bounded_reads_and_rejects_large_allocations() {
        assert!(validate_read_length(0).is_ok());
        assert!(validate_read_length(MAX_READ_LENGTH).is_ok());
        assert!(validate_read_length(MAX_READ_LENGTH + 1).is_err());
    }

    #[test]
    fn cache_copies_only_requested_range_and_updates_lru() {
        let mut cache = Cache::new();
        cache.insert(3, vec![10, 11, 12, 13, 14]);
        let mut out = Vec::new();
        assert!(cache.copy_range(3, 1, 4, &mut out));
        assert_eq!(out, vec![11, 12, 13]);
        assert_eq!(cache.lru.back(), Some(&3));
    }

    #[test]
    fn cache_miss_and_invalid_ranges_do_not_copy() {
        let mut cache = Cache::new();
        cache.insert(1, vec![1, 2, 3]);
        let mut out = Vec::new();
        assert!(!cache.copy_range(9, 0, 1, &mut out));
        assert!(!cache.copy_range(1, 0, 4, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn cache_evicts_least_recently_used_chunk() {
        let mut cache = Cache::new();
        for index in 0..16 {
            cache.insert(index, vec![index as u8]);
        }
        let mut out = Vec::new();
        assert!(cache.copy_range(0, 0, 1, &mut out));
        cache.insert(16, vec![16]);
        assert!(!cache.chunks.contains_key(&1));
        assert!(cache.chunks.contains_key(&0));
        assert!(cache.chunks.contains_key(&16));
    }
}
