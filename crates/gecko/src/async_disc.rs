//! Host-driven asynchronous DVD read protocol.
//!
//! The browser cannot synchronously read a File.slice() from the emulation
//! thread. This module describes the request/response boundary needed by a
//! host loop: the core publishes a request, the host fulfills it asynchronously,
//! and the core consumes the bytes before completing the emulated DI transfer.
//!
//! This protocol is deliberately independent of wasm-bindgen so it can be
//! unit-tested on native targets as well.

/// One disc read requested by the emulated DVD interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscReadRequest {
    /// Monotonically increasing identifier, used to reject stale responses.
    pub id: u64,
    /// Byte offset in the logical disc image.
    pub offset: u64,
    /// Number of bytes requested.
    pub length: usize,
    /// Physical emulated RAM destination.
    pub destination: u32,
}

/// Result returned by the host after its asynchronous read has completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscReadResponse {
    Data { id: u64, bytes: Vec<u8> },
    Error { id: u64, message: String },
}

/// Minimal state machine for a single in-flight disc request.
///
/// A single request is intentional: the DI device has one transfer engine, and
/// the emulated CPU must not issue another DI transfer while the first one is
/// suspended. The caller remains responsible for pausing/resuming CPU execution
/// and copying successful data into emulated RAM.
#[derive(Debug, Default)]
pub struct AsyncDiscPort {
    next_id: u64,
    pending: Option<DiscReadRequest>,
}

impl AsyncDiscPort {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            pending: None,
        }
    }

    /// Start a read, failing if a previous read has not been completed.
    pub fn begin(&mut self, offset: u64, length: usize, destination: u32) -> Result<DiscReadRequest, &'static str> {
        if self.pending.is_some() {
            return Err("a disc read is already pending");
        }
        if length == 0 {
            return Err("disc read length must be non-zero");
        }
        let request = DiscReadRequest {
            id: self.next_id,
            offset,
            length,
            destination,
        };
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.pending = Some(request);
        Ok(request)
    }

    /// Return the current request for the host to service.
    pub fn pending(&self) -> Option<DiscReadRequest> {
        self.pending
    }

    /// Accept a response only if it matches the active request and exact length.
    pub fn complete(&mut self, response: DiscReadResponse) -> Result<Vec<u8>, String> {
        let expected = self.pending.ok_or_else(|| "no disc read is pending".to_owned())?;
        let (id, bytes) = match response {
            DiscReadResponse::Data { id, bytes } => (id, bytes),
            DiscReadResponse::Error { id, message } => {
                if id != expected.id {
                    return Err(format!(
                        "stale disc read error response: expected {}, got {id}",
                        expected.id
                    ));
                }
                self.pending = None;
                return Err(message);
            }
        };
        if id != expected.id {
            return Err(format!(
                "stale disc read response: expected {}, got {id}",
                expected.id
            ));
        }
        if bytes.len() != expected.length {
            self.pending = None;
            return Err(format!(
                "disc read length mismatch: expected {}, got {}",
                expected.length,
                bytes.len()
            ));
        }
        self.pending = None;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_waits_for_matching_response() {
        let mut port = AsyncDiscPort::new();
        let request = port.begin(0x1234, 4, 0x8000_1000).unwrap();
        assert_eq!(port.pending(), Some(request));
        assert_eq!(
            port.complete(DiscReadResponse::Data {
                id: request.id,
                bytes: vec![1, 2, 3, 4]
            })
            .unwrap(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(port.pending(), None);
    }

    #[test]
    fn rejects_overlapping_requests() {
        let mut port = AsyncDiscPort::new();
        port.begin(0, 4, 0x8000_0000).unwrap();
        assert_eq!(port.begin(4, 4, 0x8000_0004), Err("a disc read is already pending"));
    }

    #[test]
    fn rejects_stale_responses_without_consuming_active_request() {
        let mut port = AsyncDiscPort::new();
        let request = port.begin(0, 2, 0x8000_0000).unwrap();
        assert!(
            port.complete(DiscReadResponse::Data {
                id: request.id + 1,
                bytes: vec![1, 2]
            })
            .is_err()
        );
        assert_eq!(port.pending(), Some(request));
    }

    #[test]
    fn rejects_short_reads_and_clears_request() {
        let mut port = AsyncDiscPort::new();
        let request = port.begin(0, 3, 0x8000_0000).unwrap();
        assert!(
            port.complete(DiscReadResponse::Data {
                id: request.id,
                bytes: vec![1, 2]
            })
            .is_err()
        );
        assert_eq!(port.pending(), None);
    }
}
