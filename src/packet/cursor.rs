//! A minimal big-endian byte cursor: a [`Reader`] for parsing and a [`Writer`]
//! for serialization.
//!
//! Both track a byte position so that every failure can report the exact
//! [`offset`](crate::error::CodecError::offset) at which it occurred. The
//! cursor is deliberately tiny and dependency-free; it is the whole of the
//! codec's parsing and serialization machinery.

use crate::error::{CodecError, CodecErrorKind};

type Result<T> = core::result::Result<T, CodecError>;

/// A forward-only reader over a borrowed byte buffer.
///
/// Every accessor advances the position on success and leaves it unchanged on
/// failure, so a failed read never partially consumes the buffer.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Creates a reader positioned at the start of `buf`.
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// The current byte offset into the buffer.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// The number of bytes left to read.
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn check(&self, n: usize) -> Result<()> {
        if self.remaining() < n {
            Err(CodecError {
                offset: self.pos,
                kind: CodecErrorKind::UnexpectedEof {
                    needed: n - self.remaining(),
                },
            })
        } else {
            Ok(())
        }
    }

    /// Reads `n` bytes, returning a borrow into the underlying buffer.
    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.check(n)?;
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Reads a fixed-size array of `N` bytes.
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    /// Reads a single byte.
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    /// Reads a big-endian `u16`.
    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    /// Reads a big-endian `u32`.
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    /// Advances past `n` bytes without returning them.
    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
}

/// A forward-only writer into a borrowed mutable byte buffer.
///
/// Writes fail (rather than panicking) when the buffer is exhausted, reporting
/// how many more bytes were needed.
#[derive(Debug)]
pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    /// Creates a writer positioned at the start of `buf`.
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// The number of bytes written so far.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    fn check(&self, n: usize) -> Result<()> {
        let available = self.buf.len() - self.pos;
        if available < n {
            Err(CodecError {
                offset: self.pos,
                kind: CodecErrorKind::BufferTooSmall {
                    needed: n - available,
                    available,
                },
            })
        } else {
            Ok(())
        }
    }

    /// Writes a slice of bytes verbatim.
    pub(crate) fn bytes(&mut self, src: &[u8]) -> Result<()> {
        self.check(src.len())?;
        self.buf[self.pos..self.pos + src.len()].copy_from_slice(src);
        self.pos += src.len();
        Ok(())
    }

    /// Writes a single byte.
    pub(crate) fn u8(&mut self, v: u8) -> Result<()> {
        self.bytes(&[v])
    }

    /// Writes a big-endian `u16`.
    pub(crate) fn u16(&mut self, v: u16) -> Result<()> {
        self.bytes(&v.to_be_bytes())
    }

    /// Writes a big-endian `u32`.
    pub(crate) fn u32(&mut self, v: u32) -> Result<()> {
        self.bytes(&v.to_be_bytes())
    }

    /// Writes `n` zero bytes.
    pub(crate) fn zeros(&mut self, n: usize) -> Result<()> {
        self.check(n)?;
        self.buf[self.pos..self.pos + n].fill(0);
        self.pos += n;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CodecErrorKind;

    // --- Reader -------------------------------------------------------------

    #[test]
    fn reader_reads_scalars_big_endian() {
        let mut r = Reader::new(&[0x12, 0x34, 0x56, 0xde, 0xad, 0xbe, 0xef, 0x78, 0x9a]);
        assert_eq!(r.position(), 0);
        assert_eq!(r.remaining(), 9);
        assert_eq!(r.u8().unwrap(), 0x12);
        assert_eq!(r.u16().unwrap(), 0x3456);
        assert_eq!(r.u32().unwrap(), 0xdead_beef);
        assert_eq!(r.u8().unwrap(), 0x78);
        assert_eq!(r.position(), 8);
        assert_eq!(r.remaining(), 1);
    }

    #[test]
    fn reader_reads_bytes_and_array() {
        let mut r = Reader::new(&[1, 2, 3, 4, 5]);
        assert_eq!(r.bytes(2).unwrap(), &[1, 2]);
        assert_eq!(r.array::<3>().unwrap(), [3, 4, 5]);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reader_skip_advances() {
        let mut r = Reader::new(&[0, 0, 0, 7]);
        r.skip(3).unwrap();
        assert_eq!(r.position(), 3);
        assert_eq!(r.u8().unwrap(), 7);
    }

    #[test]
    fn reader_reads_exactly_to_end_then_eofs() {
        let mut r = Reader::new(&[1, 2]);
        assert_eq!(r.u16().unwrap(), 0x0102);
        assert!(matches!(
            r.u8().unwrap_err().kind,
            CodecErrorKind::UnexpectedEof { needed: 1 }
        ));
    }

    #[test]
    fn reader_eof_reports_offset_and_needed() {
        let mut r = Reader::new(&[1, 2, 3]);
        r.skip(2).unwrap();
        // One byte left, ask for four: needs three more, at offset 2.
        let err = r.bytes(4).unwrap_err();
        assert_eq!(err.offset, 2);
        assert!(matches!(
            err.kind,
            CodecErrorKind::UnexpectedEof { needed: 3 }
        ));
        // Position unchanged; the remaining byte is still readable.
        assert_eq!(r.position(), 2);
        assert_eq!(r.remaining(), 1);
    }

    #[test]
    fn reader_zero_length_reads_are_noops() {
        let mut r = Reader::new(&[9]);
        assert_eq!(r.bytes(0).unwrap(), &[] as &[u8]);
        r.skip(0).unwrap();
        assert_eq!(r.position(), 0);
        assert_eq!(r.remaining(), 1);
    }

    // --- Writer -------------------------------------------------------------

    #[test]
    fn writer_writes_scalars_big_endian() {
        let mut buf = [0u8; 7];
        let mut w = Writer::new(&mut buf);
        w.u8(0x12).unwrap();
        w.u16(0x3456).unwrap();
        w.u32(0xdead_beef).unwrap();
        assert_eq!(w.position(), 7);
        assert_eq!(buf, [0x12, 0x34, 0x56, 0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn writer_writes_bytes_and_zeros() {
        let mut buf = [0xffu8; 5];
        let mut w = Writer::new(&mut buf);
        w.bytes(&[1, 2]).unwrap();
        w.zeros(3).unwrap();
        assert_eq!(buf, [1, 2, 0, 0, 0]);
    }

    #[test]
    fn writer_fills_buffer_exactly_then_overflows() {
        let mut buf = [0u8; 2];
        let mut w = Writer::new(&mut buf);
        w.u16(0x0102).unwrap();
        assert!(matches!(
            w.u8(9).unwrap_err().kind,
            CodecErrorKind::BufferTooSmall {
                needed: 1,
                available: 0
            }
        ));
    }

    #[test]
    fn writer_overflow_reports_offset_needed_available() {
        let mut buf = [0u8; 3];
        let mut w = Writer::new(&mut buf);
        w.u8(1).unwrap();
        // Two bytes left, write five: needs three more, at offset 1.
        let err = w.bytes(&[0; 5]).unwrap_err();
        assert_eq!(err.offset, 1);
        assert!(matches!(
            err.kind,
            CodecErrorKind::BufferTooSmall {
                needed: 3,
                available: 2
            }
        ));
    }

    #[test]
    fn writer_failed_write_leaves_position_and_bytes_untouched() {
        let mut buf = [0xaau8; 3];
        {
            let mut w = Writer::new(&mut buf);
            w.u8(1).unwrap();
            assert!(w.u32(0).is_err());
            assert_eq!(w.position(), 1);
        }
        assert_eq!(buf, [1, 0xaa, 0xaa]);
    }

    #[test]
    fn writer_zeros_overflow_errors() {
        let mut buf = [0u8; 2];
        let mut w = Writer::new(&mut buf);
        assert!(matches!(
            w.zeros(3).unwrap_err().kind,
            CodecErrorKind::BufferTooSmall {
                needed: 1,
                available: 2
            }
        ));
    }
}
