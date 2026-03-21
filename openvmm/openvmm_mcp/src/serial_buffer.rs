// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thread-safe ring buffer for capturing serial console output.
//!
//! Serial output is written by the VM's serial device sink and read by MCP
//! tools. A monotonic cursor allows clients to efficiently read only new data
//! since their last read.

/// Default ring buffer capacity (64 KiB).
const DEFAULT_CAPACITY: usize = 64 * 1024;

/// A thread-safe ring buffer for serial output.
pub struct SerialRingBuffer {
    inner: std::sync::Mutex<RingBufferInner>,
}

struct RingBufferInner {
    buffer: Vec<u8>,
    /// Current write position within the buffer (wraps around).
    write_pos: usize,
    /// Total bytes written since creation (monotonically increasing).
    total_written: u64,
}

impl SerialRingBuffer {
    /// Create a new ring buffer with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a new ring buffer with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be > 0");
        Self {
            inner: std::sync::Mutex::new(RingBufferInner {
                buffer: vec![0u8; capacity],
                write_pos: 0,
                total_written: 0,
            }),
        }
    }

    /// Write data into the ring buffer. May overwrite oldest data if the
    /// buffer is full.
    pub fn write(&self, data: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        let cap = inner.buffer.len();
        for &byte in data {
            let pos = inner.write_pos;
            inner.buffer[pos] = byte;
            inner.write_pos = (pos + 1) % cap;
        }
        inner.total_written += data.len() as u64;
    }

    /// Read all data written since the given cursor position.
    ///
    /// Returns `(data, new_cursor)` where `new_cursor` should be passed to the
    /// next call to read only newly-arrived data. If `cursor` is behind by
    /// more than the buffer capacity, some data will have been lost and the
    /// returned data starts from the oldest available byte.
    ///
    /// Pass `cursor = 0` on the first call to read everything currently in the
    /// buffer.
    pub fn read_since(&self, cursor: u64) -> (Vec<u8>, u64) {
        let inner = self.inner.lock().unwrap();
        let cap = inner.buffer.len() as u64;
        let total = inner.total_written;

        if cursor >= total {
            // Nothing new.
            return (Vec::new(), total);
        }

        // How many bytes are available since cursor?
        let mut available = total - cursor;
        // Clamp to buffer capacity — we can't return data that was overwritten.
        if available > cap {
            available = cap;
        }

        let start_total = total - available;
        let start_pos = (inner.write_pos as u64 + cap - available) % cap;
        let mut result = Vec::with_capacity(available as usize);

        for i in 0..available {
            let idx = ((start_pos + i) % cap) as usize;
            result.push(inner.buffer[idx]);
        }

        let _ = start_total;
        (result, total)
    }

    /// Return the current cursor (total bytes written so far).
    pub fn cursor(&self) -> u64 {
        self.inner.lock().unwrap().total_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_write_read() {
        let buf = SerialRingBuffer::with_capacity(16);
        buf.write(b"hello");
        let (data, cursor) = buf.read_since(0);
        assert_eq!(data, b"hello");
        assert_eq!(cursor, 5);

        buf.write(b" world");
        let (data, cursor2) = buf.read_since(cursor);
        assert_eq!(data, b" world");
        assert_eq!(cursor2, 11);
    }

    #[test]
    fn wrap_around() {
        let buf = SerialRingBuffer::with_capacity(8);
        buf.write(b"abcdefgh"); // fills exactly
        buf.write(b"ij"); // overwrites a, b
        let (data, cursor) = buf.read_since(0);
        // Only the last 8 bytes are available.
        assert_eq!(data, b"cdefghij");
        assert_eq!(cursor, 10);
    }

    #[test]
    fn read_nothing_new() {
        let buf = SerialRingBuffer::new();
        buf.write(b"test");
        let (_, cursor) = buf.read_since(0);
        let (data, cursor2) = buf.read_since(cursor);
        assert!(data.is_empty());
        assert_eq!(cursor, cursor2);
    }
}
