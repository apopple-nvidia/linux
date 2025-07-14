use kernel::alloc::{flags::GFP_KERNEL, KVec};
use kernel::prelude::*;
use kernel::str::CString;
use kernel::error::code::*;

/// A buffer abstraction for discontiguous byte slices.
///
/// This allows you to treat multiple non-contiguous `&mut [u8]` slices
/// as a single stream-like read/write buffer.
///
/// Example:
///
/// let mut buf1 = [0u8; 3];
/// let mut buf2 = [0u8; 5];
/// let mut sbuffer = SBuffer::new([&mut buf1[..], &mut buf2[..]]);
///
/// let data = b"hellowo";
/// let result = sbuffer.write(data);
///

const MAX_SLICES: usize = 2;

pub(crate) struct SBuffer<'a> {
    /// storage for MAX_SLICES slices
    slices: [Option<&'a mut [u8]>; MAX_SLICES],
    /// entries used
    len: usize,
    idx: usize,
    offset: usize,
}

impl<'a> SBuffer<'a> {
    /// Create from any iterator of `&'a mut [u8]`, up to MAX_SLICES entries
    pub(crate) fn new<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = &'a mut [u8]>,
    {
        // Initialize array with None values using const block
        let mut slices: [Option<&'a mut [u8]>; MAX_SLICES] = [const { None }; MAX_SLICES];
        let mut len = 0;
        
        // Fill in actual slices
        for s in iter {
            if len < MAX_SLICES {
                slices[len] = Some(s);
                len += 1;
            } else {
                break;
            }
        }
        
        SBuffer { slices, len, idx: 0, offset: 0 }
    }

    pub(crate) fn total_capacity(&self) -> usize {
        self.slices[..self.len].iter()
            .filter_map(|s| s.as_ref()) /* Filters out None and unwraps the Option */
            .map(|s| s.len())
            .sum()
    }

    pub(crate) fn total_remaining(&self) -> usize {
        let used: usize = self.slices[..self.idx]
            .iter()
            .filter_map(|s| s.as_ref()) /* Filters out None and unwraps the Option */
            .map(|s| s.len())
            .sum::<usize>() + self.offset;
        self.total_capacity() - used
    }

    pub(crate) fn write(&mut self, mut src: &[u8]) -> Result {
        if src.len() > self.total_remaining() {
            return Err(ENOSPC);
        }

        while !src.is_empty() && self.idx < self.len {
            if let Some(current) = self.slices[self.idx].as_mut() {
                let remaining = &mut current[self.offset..];
                let n = remaining.len().min(src.len());
                remaining[..n].copy_from_slice(&src[..n]);
                self.offset += n;
                src = &src[n..];
                if self.offset == current.len() {
                    self.advance_slice();
                }
            } else {
                self.advance_slice();
            }
        }
        Ok(())
    }

    pub(crate) fn read(&mut self, mut dst: &mut [u8]) -> Result {
        if dst.len() > self.total_remaining() {
            return Err(EINVAL);
        }

        while !dst.is_empty() && self.idx < self.len {
            if let Some(current) = self.slices[self.idx].as_ref() {
                let remaining = &current[self.offset..];
                let n = remaining.len().min(dst.len());
                dst[..n].copy_from_slice(&remaining[..n]);
                self.offset += n;
                dst = &mut dst[n..];
                if self.offset == current.len() {
                    self.advance_slice();
                }
            } else {
                self.advance_slice();
            }
        }
        Ok(())
    }

    pub(crate) fn write_byte(&mut self, byte: u8) -> Result {
        self.write(&[byte])
    }

    pub(crate) fn write_word(&mut self, word: u16) -> Result {
        self.write(&word.to_le_bytes())
    }

    pub(crate) fn write_dword(&mut self, dword: u32) -> Result {
        self.write(&dword.to_le_bytes())
    }

    pub(crate) fn write_bytes(&mut self, data: &[u8]) -> Result {
        self.write(data)
    }

    pub(crate) fn write_str(&mut self, s: &str) -> Result {
        self.write(s.as_bytes())
    }

    pub(crate) fn write_slice(&mut self, data: &[u8]) -> Result {
        self.write(data)
    }

    pub(crate) fn read_byte(&mut self) -> Result<u8> {
        let mut buf = [0u8];
        self.read(&mut buf)?;
        Ok(buf[0])
    }

    pub(crate) fn read_word(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.read(&mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    pub(crate) fn read_dword(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read(&mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    pub(crate) fn read_bytes(&mut self, dst: &mut [u8]) -> Result {
        self.read(dst)
    }
    
    pub(crate) fn read_str(&mut self, len: usize) -> Result<CString> {
        let mut buf = KVec::<u8>::with_capacity(len, GFP_KERNEL)?;        
        self.read(&mut buf)?;
        let string = core::str::from_utf8(&buf).map_err(|_| EINVAL)?;        
        CString::try_from_fmt(fmt!("{}", string))
            .map_err(|_| ENOMEM)
    }

    pub(crate) fn reset_pos(&mut self) {
        self.idx = 0;
        self.offset = 0;
    }

    pub(crate) fn current_pos(&self) -> (usize, usize) {
        (self.idx, self.offset)
    }

    /// Move to the next slice
    fn advance_slice(&mut self) {
        self.idx += 1;
        self.offset = 0;
    }

    /// Seek the position forward by the specified number of bytes
    pub(crate) fn seek(&mut self, mut offset: usize) -> Result {
        if offset > self.total_remaining() {
            return Err(EINVAL);
        }

        while offset > 0 && self.idx < self.len {
            if let Some(current_slice) = self.slices[self.idx].as_ref() {
                let remaining_in_slice = current_slice.len() - self.offset;

                if offset < remaining_in_slice {
                    // Seek within current slice
                    self.offset += offset;
                    offset = 0;
                } else {
                    // Consume rest of current slice and move to next
                    offset -= remaining_in_slice;
                    self.advance_slice();
                }
            } else {
                self.advance_slice();
            }
        }
        Ok(())
    }

    pub fn byte_iter(&'a mut self) -> SBufferByteIterator<'a> {
        SBufferByteIterator { sbuf: self, pos: 0 }
    }
}

struct SBufferByteIterator<'a> {
    sbuf: &'a mut SBuffer<'a>,
    pos: usize,
}

impl<'a> Iterator for SBufferByteIterator<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        match self.sbuf.read_byte() {
            Ok(byte) => Some(byte),
            Err(_) => None,
        }
    }
}

impl<'a> DoubleEndedIterator for SBufferByteIterator<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self.sbuf.read_byte() {
            Ok(byte) => Some(byte),
            Err(_) => None,
        }
    }
}
