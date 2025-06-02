// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A circular buffer for storing log messages when no other logging
//! method is available.

use core::fmt;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Size of the circular log buffer in bytes
pub const LOG_BUFFER_SIZE: usize = 4096;

/// A circular buffer for storing log messages when no other logging is available.
#[repr(C)]
pub struct LogBuffer {
    /// The actual buffer storage
    buffer: [u8; LOG_BUFFER_SIZE],
    /// Current write position in the buffer
    position: AtomicUsize,
}

impl LogBuffer {
    /// Creates a new, empty log buffer
    pub const fn new() -> Self {
        Self {
            buffer: [0; LOG_BUFFER_SIZE],
            position: AtomicUsize::new(0),
        }
    }

    /// Writes data to the buffer, wrapping around if necessary
    fn write_to_buffer(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        // Get current position and reserve space for the new data
        let current_pos = self.position.load(Ordering::Relaxed);
        let data_len = data.len();

        // Copy data to the buffer, wrapping around if necessary
        for (i, &byte) in data.iter().enumerate() {
            let buf_pos = (current_pos + i) % LOG_BUFFER_SIZE;
            self.buffer[buf_pos] = byte;
        }

        // Update position atomically
        self.position.store(
            (current_pos + data_len) % LOG_BUFFER_SIZE,
            Ordering::Release,
        );
    }

    /// Gets the current write position in the buffer
    pub fn get_position(&self) -> usize {
        self.position.load(Ordering::Acquire)
    }

    /// Gets raw access to the buffer for memory-mapped access
    pub fn get_raw_buffer(&self) -> &[u8; LOG_BUFFER_SIZE] {
        &self.buffer
    }

    /// Gets a copy of the current buffer contents
    pub fn get_buffer(&self) -> [u8; LOG_BUFFER_SIZE] {
        // To present a coherent view, we need to reconstruct the buffer in the proper order
        let mut result = [0u8; LOG_BUFFER_SIZE];
        let current_pos = self.position.load(Ordering::Acquire);

        // First copy from current position to the end
        for i in 0..LOG_BUFFER_SIZE {
            let src_pos = (current_pos + i) % LOG_BUFFER_SIZE;
            result[i] = self.buffer[src_pos];
        }

        result
    }
}

impl Write for LogBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_to_buffer(s.as_bytes());
        Ok(())
    }
}
