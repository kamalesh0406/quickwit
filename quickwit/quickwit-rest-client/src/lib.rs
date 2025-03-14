// Copyright (C) 2023 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::path::Path;
use std::{io, mem};

use bytes::Bytes;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tracing::warn;

pub mod error;
pub mod models;
pub mod rest_client;

// re-exports
pub use quickwit_config::ConfigFormat;
pub use reqwest::Url;

pub(crate) struct BatchLineReader {
    buf_reader: BufReader<Box<dyn AsyncRead + Send + Sync + Unpin>>,
    buffer: Vec<u8>,
    alloc_num_bytes: usize,
    max_batch_num_bytes: usize,
    num_lines: usize,
    has_next: bool,
}

impl BatchLineReader {
    pub async fn from_file(filepath: &Path, max_batch_num_bytes: usize) -> io::Result<Self> {
        let file = File::open(&filepath).await?;
        Ok(Self::new(Box::new(file), max_batch_num_bytes))
    }

    pub fn from_stdin(max_batch_num_bytes: usize) -> Self {
        Self::new(Box::new(tokio::io::stdin()), max_batch_num_bytes)
    }

    pub fn new(
        reader: Box<dyn AsyncRead + Send + Sync + Unpin>,
        max_batch_num_bytes: usize,
    ) -> Self {
        let alloc_num_bytes = max_batch_num_bytes + 100 * 1024; // Add 100 KiB headroom to avoid reallocation.
        Self {
            buf_reader: BufReader::new(reader),
            buffer: Vec::with_capacity(alloc_num_bytes),
            alloc_num_bytes,
            max_batch_num_bytes,
            num_lines: 0,
            has_next: true,
        }
    }

    pub async fn next_batch(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            let line_num_bytes = self.buf_reader.read_until(b'\n', &mut self.buffer).await?;

            if line_num_bytes > self.max_batch_num_bytes {
                warn!(
                    "Skipping line {}, which exceeds the maximum allowed content length ({} vs. \
                     {} bytes).",
                    self.num_lines + 1,
                    line_num_bytes,
                    self.max_batch_num_bytes
                );
                let new_len = self.buffer.len() - line_num_bytes;
                self.buffer.truncate(new_len);
                continue;
            }
            if self.buffer.len() > self.max_batch_num_bytes {
                let mut new_buffer = Vec::with_capacity(self.alloc_num_bytes);
                let new_len = self.buffer.len() - line_num_bytes;
                new_buffer.extend_from_slice(&self.buffer[new_len..]);
                self.buffer.truncate(new_len);
                let batch = mem::replace(&mut self.buffer, new_buffer);
                return Ok(Some(Bytes::from(batch)));
            }
            if line_num_bytes == 0 {
                self.has_next = false;
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let batch = mem::take(&mut self.buffer);
                return Ok(Some(Bytes::from(batch)));
            }
            self.num_lines += 1;
        }
    }

    /// Returns whether there is still data available
    ///
    /// This can spuriously return `true` when there was no data
    /// to send at all.
    pub fn has_next(&self) -> bool {
        self.has_next
    }

    fn from_string(payload: impl ToString, max_batch_num_bytes: usize) -> Self {
        Self::new(
            Box::new(std::io::Cursor::new(payload.to_string().into_bytes())),
            max_batch_num_bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_batch_reader() {
        {
            let mut batch_reader = BatchLineReader::from_string("".to_string(), 10);
            assert!(batch_reader.next_batch().await.unwrap().is_none());
            assert!(batch_reader.next_batch().await.unwrap().is_none());
        }
        {
            let mut batch_reader = BatchLineReader::from_string("foo\n", 10);
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"foo\n"
            );
            assert!(batch_reader.next_batch().await.unwrap().is_none());
            assert!(batch_reader.next_batch().await.unwrap().is_none());
        }
        {
            let mut batch_reader = BatchLineReader::from_string("foo\nbar\nqux\n", 10);
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"foo\nbar\n"
            );
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"qux\n"
            );
            assert!(batch_reader.next_batch().await.unwrap().is_none());
            assert!(batch_reader.next_batch().await.unwrap().is_none());
        }
        {
            let mut batch_reader = BatchLineReader::from_string("fooo\nbaar\nqux\n", 10);
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"fooo\nbaar\n"
            );
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"qux\n"
            );
            assert!(batch_reader.next_batch().await.unwrap().is_none());
            assert!(batch_reader.next_batch().await.unwrap().is_none());
        }
        {
            let mut batch_reader =
                BatchLineReader::from_string("foobarquxbaz\nfoo\nbar\nqux\n", 10);
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"foo\nbar\n"
            );
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"qux\n"
            );
            assert!(batch_reader.next_batch().await.unwrap().is_none());
            assert!(batch_reader.next_batch().await.unwrap().is_none());
        }
        {
            let mut batch_reader =
                BatchLineReader::from_string("foo\nbar\nfoobarquxbaz\nqux\n", 10);
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"foo\nbar\n"
            );
            assert_eq!(
                &batch_reader.next_batch().await.unwrap().unwrap()[..],
                b"qux\n"
            );
            assert!(batch_reader.next_batch().await.unwrap().is_none());
            assert!(batch_reader.next_batch().await.unwrap().is_none());
        }
    }
}
