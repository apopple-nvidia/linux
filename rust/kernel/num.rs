// SPDX-License-Identifier: GPL-2.0

//! Numerical and binary utilities for primitive types.

/// Extension trait providing useful methods for the kernel on integers.
pub trait NumExt {
    /// Align `self` down to `alignment`.
    ///
    /// `alignment` must be a power of 2 for accurate results.
    ///
    /// # Examples
    ///
    /// ```
    /// use kernel::num::NumExt;
    ///
    /// assert_eq!(0x4fffu32.align_down(0x1000), 0x4000);
    /// assert_eq!(0x4fffu32.align_down(0x0), 0x0);
    /// ```
    fn align_down(self, alignment: Self) -> Self;

    /// Align `self` up to `alignment`.
    ///
    /// `alignment` must be a power of 2 for accurate results.
    ///
    /// Wraps around to `0` if the requested alignment pushes the result above the type's limits.
    ///
    /// # Examples
    ///
    /// ```
    /// use kernel::num::NumExt;
    ///
    /// assert_eq!(0x4fffu32.align_up(0x1000), 0x5000);
    /// assert_eq!(0x4000u32.align_up(0x1000), 0x4000);
    /// assert_eq!(0x0u32.align_up(0x1000), 0x0);
    /// assert_eq!(0xffffu16.align_up(0x100), 0x0);
    /// assert_eq!(0x4fffu32.align_up(0x0), 0x0);
    /// ```
    fn align_up(self, alignment: Self) -> Self;

    /// Find Last Set Bit: return the 1-based index of the last (i.e. most significant) set bit in
    /// `self`.
    ///
    /// Equivalent to the C `fls` function.
    ///
    /// # Examples
    ///
    /// ```
    /// use kernel::num::NumExt;
    ///
    /// assert_eq!(0x0u32.fls(), 0);
    /// assert_eq!(0x1u32.fls(), 1);
    /// assert_eq!(0x10u32.fls(), 5);
    /// assert_eq!(0xffffu32.fls(), 16);
    /// assert_eq!(0x8000_0000u32.fls(), 32);
    /// ```
    fn fls(self) -> u32;
}

macro_rules! numext_impl {
    ($($t:ty),+) => {
        $(
            impl NumExt for $t {
                #[inline]
                fn align_down(self, alignment: Self) -> Self {
                    self & !alignment.wrapping_sub(1)
                }

                #[inline]
                fn align_up(self, alignment: Self) -> Self {
                    self.wrapping_add(alignment.wrapping_sub(1)).align_down(alignment)
                }

                #[inline]
                fn fls(self) -> u32 {
                    Self::BITS - self.leading_zeros()
                }
            }
        )+
    };
}

numext_impl!(usize, u8, u16, u32, u64, u128);
