//! Bounded asynchronous reads from a browser-selected disc image.
//!
//! This is the browser-side I/O primitive for future resumable Wii boot/runtime
//! work. It deliberately does not implement image::Dvd: Gecko's current Dvd
//! interface is synchronous, so callers must await reads and resume emulation
//! rather than trying to block the browser event loop.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::File;

const CHUNK_SIZE: usize = 256 * 1024;
const MAX_CACHED_CHUNKS: usize = 16;

struct Cache {
    chunks: HashMap<u64, Vec<u8>>,
    lru: VecDeque<u64>,
}

impl Cache {
    fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            lru: VecDeque::new(),
        }
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
        Self {
            file,
            cache: RefCell::new(Cache::new()),
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.file.size() as u64
    }

    pub(crate) async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, JsValue> {
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| JsValue::from_str("disc range overflow"))?;
        if end > self.len() {
            return Err(JsValue::from_str("disc range extends beyond end of file"));
        }
        if length == 0 {
            return Ok(Vec::new());
        }

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
                let blob = self
                    .file
                    .slice_with_f64_and_f64(chunk_start as f64, slice_end as f64)?;
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
