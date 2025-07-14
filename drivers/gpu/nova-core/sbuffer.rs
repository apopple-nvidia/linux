use kernel::alloc::{flags::GFP_KERNEL, KVec};
use kernel::error::code::*;
use kernel::prelude::*;
use kernel::str::CString;

use core::ops::Index;

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
    pub capacity: usize,
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

        let capacity = slices[..len]
            .iter()
            .filter_map(|s| s.as_ref()) /* Filters out None and unwraps the Option */
            .map(|s| s.len())
            .sum();

        SBuffer {
            slices,
            len,
            capacity,
        }
    }

    pub(crate) fn write(&mut self, offset: usize, mut src: &[u8]) -> Result {
        if src.len() > self.capacity - offset {
            return Err(EINVAL);
        }
        let (mut idx, mut offset) = self.get_pos_from_offset(offset)?;

        while !src.is_empty() && idx < self.len {
            if let Some(current) = self.slices[idx].as_mut() {
                let remaining = &mut current[offset..];
                let n = remaining.len().min(src.len());
                remaining[..n].copy_from_slice(&src[..n]);
                offset += n;
                src = &src[n..];
                if offset == current.len() {
                    idx += 1;
                    offset = 0;
                }
            } else {
                // TODO: Is this valid?
                idx += 1;
                offset = 0;
            }
        }
        Ok(())
    }

    pub(crate) fn read(&self, offset: usize, mut dst: &mut [u8]) -> Result {
        if dst.len() > self.capacity - offset {
            return Err(EINVAL);
        }
        let (mut idx, mut offset) = self.get_pos_from_offset(offset)?;

        while !dst.is_empty() && idx < self.len {
            if let Some(current) = self.slices[idx].as_ref() {
                let remaining = &current[offset..];
                let n = remaining.len().min(dst.len());
                dst[..n].copy_from_slice(&remaining[..n]);
                offset += n;
                dst = &mut dst[n..];
                if offset == current.len() {
                    idx += 1;
                    offset = 0;
                }
            } else {
                // TODO: Is this valid?
                idx += 1;
                offset = 0;
            }
        }
        Ok(())
    }

    pub(crate) fn read_byte(&self, offset: usize) -> Result<u8> {
        let mut buf = [0u8];
        self.read(offset, &mut buf)?;
        Ok(buf[0])
    }

    #[allow(dead_code)]
    pub(crate) fn read_word(&self, offset: usize) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.read(offset, &mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    #[allow(dead_code)]
    pub(crate) fn read_dword(&self, offset: usize) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read(offset, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    #[allow(dead_code)]
    pub(crate) fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> Result {
        self.read(offset, dst)
    }

    #[allow(dead_code)]
    pub(crate) fn read_str(&self, offset: usize, len: usize) -> Result<CString> {
        let mut buf = KVec::<u8>::with_capacity(len, GFP_KERNEL)?;
        self.read(offset, &mut buf)?;
        let string = core::str::from_utf8(&buf).map_err(|_| EINVAL)?;
        CString::try_from_fmt(fmt!("{}", string)).map_err(|_| ENOMEM)
    }

    fn get_pos_from_offset(&self, mut offset: usize) -> Result<(usize, usize)> {
        if offset >= self.capacity {
            return Err(ERANGE);
        }

        let mut idx = 0;
        for some_slice in &self.slices {
            if let Some(slice) = some_slice {
                if offset < slice.len() {
                    return Ok((idx, offset));
                } else {
                    offset -= slice.len();
                }
                idx += 1;
            }
        }

        Err(ERANGE)
    }

    pub(crate) fn iter_mut<'b>(&'b mut self) -> SBufferIteratorMut<'a, 'b> {
        SBufferIteratorMut { sbuf: self, pos: 0 }
    }

    pub(crate) fn iter(&'a self) -> SBufferIterator<'a> {
        SBufferIterator { sbuf: self, pos: 0 }
    }
}

impl Index<usize> for SBuffer<'_> {
    type Output = u8;

    fn index(&self, offset: usize) -> &Self::Output {
        let (idx, slice_offset) = self.get_pos_from_offset(offset).unwrap();
        if let Some(current) = self.slices[idx].as_ref() {
            return &current[slice_offset];
        }
        panic!();
    }
}

pub(crate) struct SBufferIteratorMut<'a, 'b> {
    sbuf: &'b mut SBuffer<'a>,
    pos: usize,
}

impl SBufferIteratorMut<'_, '_> {
    pub(crate) fn write_slice(&mut self, data: &[u8]) -> Result {
        self.sbuf.write(self.pos, data)?;
        self.pos += data.len();
        Ok(())
    }

    pub(crate) fn write_byte(&mut self, byte: u8) -> Result {
        self.sbuf.write(self.pos, &[byte])?;
        self.pos += size_of::<u8>();
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn write_word(&mut self, word: u16) -> Result {
        self.sbuf.write(self.pos, &word.to_le_bytes())?;
        self.pos += size_of::<u16>();
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn write_dword(&mut self, dword: u32) -> Result {
        self.sbuf.write(self.pos, &dword.to_le_bytes())?;
        self.pos += size_of::<u32>();
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn write_bytes(&mut self, data: &[u8]) -> Result {
        self.sbuf.write(self.pos, data)?;
        self.pos += data.len();
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn write_str(&mut self, s: &str) -> Result {
        self.sbuf.write(self.pos, s.as_bytes())?;
        self.pos += s.as_bytes().len();
        Ok(())
    }
}

pub(crate) struct SBufferIterator<'a> {
    sbuf: &'a SBuffer<'a>,
    pos: usize,
}

impl<'a> Iterator for SBufferIterator<'a> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        let result = match self.sbuf.read_byte(self.pos) {
            Ok(byte) => Some(byte),
            Err(_) => None,
        };
        self.pos += 1;
        result
    }
}

impl<'a> DoubleEndedIterator for SBufferIterator<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.pos == self.sbuf.capacity {
            return None;
        }

        let result = match self.sbuf.read_byte(self.sbuf.capacity - self.pos - 1) {
            Ok(byte) => Some(byte),
            Err(_) => None,
        };
        self.pos += 1;
        result
    }
}
