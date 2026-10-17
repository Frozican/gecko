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

    fn get(&mut self, index: u64) -> Option<Vec<u8>> {
        let bytes = self.chunks.get(&index)?.clone();
        self.lru.retain(|&item| item != index);
        self.lru.push_back(index);
        Some(bytes)
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
            let cached = { self.cache.borrow_mut().get(chunk_index) };
            let chunk = if let Some(bytes) = cached {
                bytes
            } else {
                let slice_end = (chunk_start + CHUNK_SIZE as u64).min(self.len());
                let blob = self.file.slice_with_f64_and_f64(chunk_start as f64, slice_end as f64)?;
                let array_buffer = JsFuture::from(blob.array_buffer()).await?;
                let bytes = js_sys::Uint8Array::new(&array_buffer).to_vec();
                self.cache.borrow_mut().insert(chunk_index, bytes.clone());
                bytes
            };

            let within = (cursor - chunk_start) as usize;
            if within >= chunk.len() {
                return Err(JsValue::from_str("browser returned an incomplete disc chunk"));
            }
            let take = ((end - cursor) as usize).min(chunk.len() - within);
            output.extend_from_slice(&chunk[within..within + take]);
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
        let fst_offset = (be_u32(&boot, 0x424)? as u64) * 4;
        let fst_size = (be_u32(&boot, 0x428)? as usize).checked_mul(4)
            .ok_or_else(|| JsValue::from_str("FST size overflow"))?;
        if fst_size < 12 || fst_size > MAX_FST_SIZE {
            return Err(JsValue::from_str("FST size is outside the diagnostic probe limit"));
        }
        let fst = self.inner.read_range(data_start + fst_offset, fst_size).await?;
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
            "NKit metadata probe: PASS\nDisc ID: {game_id}\nISO size: {} bytes\nPartition offset: 0x{partition_offset:X}\nPartition data start: 0x{data_start:X}\nFST offset (partition-relative): 0x{fst_offset:X}\nFST size: {fst_size} bytes\nFST entries: {entry_count}\nLeaf file entries scanned: {leaf_files}\nFilenames containing 'strm' (up to 8): {}\n\nDiagnostic only: this does not restore NKit gaps/hash data and does not boot the game.",
            self.inner.len(),
            if matching_stream_names.is_empty() { "(none in this scan)".to_string() } else { matching_stream_names.join(", ") }
        ))
    }
}
