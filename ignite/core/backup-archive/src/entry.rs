//! Bounded, verified inflation of one validated entry.

use std::io::{self, Read, Seek, SeekFrom, Take};

use flate2::read::DeflateDecoder;

use crate::container::Entry;
use crate::error::{RestoreError, ZipErrorCode as Z};

/// Streams one entry's inflated bytes. It never yields more than the declared size, and at
/// end of stream it requires the exact declared size, the CRC-32, and that the deflate stream
/// consumed exactly the entry's compressed bytes. A failure is recorded (so the caller can
/// report the ZIP-level cause even when the consumer only saw an I/O error) and returned as
/// an `io::Error`.
pub(crate) struct EntryReader<'a, R: Read> {
    decoder: DeflateDecoder<Take<&'a mut R>>,
    entry: &'a Entry,
    crc: crc32fast::Hasher,
    out: u64,
    ended: bool,
    failure: Option<RestoreError>,
}

impl<'a, R: Read + Seek> EntryReader<'a, R> {
    pub fn open(r: &'a mut R, entry: &'a Entry) -> Result<Self, RestoreError> {
        r.seek(SeekFrom::Start(entry.data_start))
            .map_err(|e| RestoreError::io(format!("seek to {}: {e}", entry.path)))?;
        Ok(Self {
            decoder: DeflateDecoder::new(r.take(entry.compressed_size)),
            entry,
            crc: crc32fast::Hasher::new(),
            out: 0,
            ended: false,
            failure: None,
        })
    }
}

impl<R: Read> EntryReader<'_, R> {
    /// The ZIP-level failure seen so far, if any.
    pub fn failure(&self) -> Option<&RestoreError> {
        self.failure.as_ref()
    }

    fn fail(&mut self, code: Z, msg: String) -> io::Error {
        let e = RestoreError::zip(code, format!("entry {}: {msg}", self.entry.path));
        let out = io::Error::new(io::ErrorKind::InvalidData, e.to_string());
        self.failure.get_or_insert(e);
        out
    }

    /// Read to the end (verifying everything) and report the outcome.
    pub fn finish(mut self) -> Result<(), RestoreError> {
        let mut sink = [0u8; 8192];
        loop {
            match self.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    return Err(self.failure.take().unwrap_or_else(|| {
                        RestoreError::io(format!("read {}: {e}", self.entry.path))
                    }))
                }
            }
        }
        match self.failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl<R: Read> Read for EntryReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(e) = &self.failure {
            return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
        }
        if self.ended || buf.is_empty() {
            return Ok(0);
        }
        // Never ask for more than one byte past the declared size.
        let room = (self.entry.size - self.out).saturating_add(1);
        let want = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        let n = match self.decoder.read(&mut buf[..want]) {
            Ok(n) => n,
            Err(e) => {
                let msg = if e.kind() == io::ErrorKind::UnexpectedEof {
                    format!("compressed data ends early ({e})")
                } else {
                    format!("invalid deflate data ({e})")
                };
                return Err(self.fail(Z::CorruptData, msg));
            }
        };
        self.out += n as u64;
        if self.out > self.entry.size {
            let declared = self.entry.size;
            return Err(self.fail(
                Z::SizeMismatch,
                format!("inflates to more than its declared {declared} bytes"),
            ));
        }
        self.crc.update(&buf[..n]);
        if n == 0 {
            self.ended = true;
            if self.out != self.entry.size {
                let (out, declared) = (self.out, self.entry.size);
                return Err(self.fail(
                    Z::SizeMismatch,
                    format!("inflates to {out} bytes, declared {declared}"),
                ));
            }
            if self.decoder.total_in() != self.entry.compressed_size {
                let (used, have) = (self.decoder.total_in(), self.entry.compressed_size);
                return Err(self.fail(
                    Z::CorruptData,
                    format!("deflate stream used {used} of {have} compressed bytes"),
                ));
            }
            let crc = self.crc.clone().finalize();
            if crc != self.entry.crc32 {
                let want = self.entry.crc32;
                return Err(self.fail(
                    Z::CrcMismatch,
                    format!("CRC-32 {crc:08x}, declared {want:08x}"),
                ));
            }
        }
        Ok(n)
    }
}
