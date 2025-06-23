// SPDX-License-Identifier: GPL-2.0

use kernel::device;
use kernel::dma::DataDirection;
use kernel::dma::DmaAddress;
use kernel::prelude::*;
use kernel::scatterlist::Owned;
use kernel::scatterlist::SGTable;

use crate::dma::DmaObject;
use crate::firmware::riscv::RiscvFirmware;
use crate::gpu::Architecture;
use crate::gpu::Chipset;
use crate::gsp::GSP_PAGE_SIZE;

/// Ad-hoc and temporary module to extract sections from ELF images.
///
/// Some firmware images are currently packaged as ELF files, where sections names are used as keys
/// to specific and related bits of data. Future firmware versions are scheduled to move away from
/// that scheme before nova-core becomes stable, which means this module will eventually be
/// removed.
mod elf {
    use kernel::bindings;
    use kernel::str::CStr;
    use kernel::transmute::FromBytes;

    /// Newtype to provide a [`FromBytes`] implementation.
    #[repr(transparent)]
    struct Elf64Hdr(bindings::elf64_hdr);

    // SAFETY: all bit patterns are valid for this type, and it doesn't use interior mutability.
    unsafe impl FromBytes for Elf64Hdr {}

    /// Tries to extract section with name `name` from the ELF64 image `elf`, and returns it.
    pub(super) fn elf64_section<'a, 'b>(elf: &'a [u8], name: &'b str) -> Option<&'a [u8]> {
        let hdr = &elf
            .get(0..size_of::<bindings::elf64_hdr>())
            .and_then(Elf64Hdr::from_bytes)?
            .0;

        // Get all the section headers.
        let shdr = {
            let shdr_num = usize::from(hdr.e_shnum);
            let shdr_start = usize::try_from(hdr.e_shoff).ok()?;
            let shdr_end = shdr_num
                .checked_mul(size_of::<bindings::elf64_shdr>())
                .and_then(|v| v.checked_add(shdr_start))?;

            elf.get(shdr_start..shdr_end)
                .map(|slice| slice.as_ptr())
                .filter(|ptr| ptr.align_offset(align_of::<bindings::elf64_shdr>()) == 0)
                // `FromBytes::from_bytes` does not support slices yet, so build it manually.
                //
                // SAFETY:
                // * `get` guarantees that the slice is within the bounds of `elf` and of size
                //   `elf64_shdr * shdr_num`.
                // * We checked that `ptr` had the correct alignment for `elf64_shdr`.
                .map(|ptr| unsafe {
                    core::slice::from_raw_parts(ptr.cast::<bindings::elf64_shdr>(), shdr_num)
                })?
        };

        // Get the strings table.
        let strhdr = shdr.get(usize::from(hdr.e_shstrndx))?;

        // Find the section which name matches `name` and return it.
        shdr.iter()
            .find(|sh| {
                let Some(name_idx) = strhdr
                    .sh_offset
                    .checked_add(u64::from(sh.sh_name))
                    .and_then(|idx| usize::try_from(idx).ok())
                else {
                    return false;
                };

                // Get the start of the name.
                elf.get(name_idx..)
                    // Stop at the first `0`.
                    .and_then(|nstr| nstr.get(0..=nstr.iter().position(|b| *b == 0)?))
                    // Convert into CStr. This should never fail because of the line above.
                    .and_then(|nstr| CStr::from_bytes_with_nul(nstr).ok())
                    // Convert into str.
                    .and_then(|c_str| c_str.to_str().ok())
                    // Check that the name matches.
                    .map(|str| str == name)
                    .unwrap_or(false)
            })
            // Return the slice containing the section.
            .and_then(|sh| {
                let start = usize::try_from(sh.sh_offset).ok()?;
                let end = usize::try_from(sh.sh_size)
                    .ok()
                    .and_then(|sh_size| start.checked_add(sh_size))?;

                elf.get(start..end)
            })
    }
}

/// GSP firmware with 3-level radix page tables for the GSP bootloader.
///
/// The bootloader expects firmware to be mapped starting at address 0 in GSP's virtual address
/// space:
///
/// ```text
/// Level 0:  1 page, 1 entry         -> points to first level 1 page
/// Level 1:  Multiple pages/entries  -> each entry points to a level 2 page
/// Level 2:  Multiple pages/entries  -> each entry points to a firmware page
/// ```
///
/// Each page is 4KB, each entry is 8 bytes (64-bit DMA address).
/// Also known as "Radix3" firmware.
#[pin_data]
pub(crate) struct GspFirmware {
    /// The GSP firmware inside a [`VVec`], device-mapped via a SG table.
    #[pin]
    fw: SGTable<Owned<VVec<u8>>>,
    /// Level 2 page table whose entries contain DMA addresses of firmware pages.
    #[pin]
    level2: SGTable<Owned<VVec<u8>>>,
    /// Level 1 page table whose entries contain DMA addresses of level 2 pages.
    #[pin]
    level1: SGTable<Owned<VVec<u8>>>,
    /// Level 0 page table (single 4KB page) with one entry: DMA address of first level 1 page.
    level0: DmaObject,
    /// Size in bytes of the firmware contained in [`Self::fw`].
    pub size: usize,
    /// Device-mapped GSP signatures matching the GPU's [`Chipset`].
    pub signatures: DmaObject,
    /// GSP bootloader, verifies the GSP firmware before loading and running it.
    pub bootloader: RiscvFirmware,
}

impl GspFirmware {
    /// Loads the GSP firmware binaries, map them into `dev`'s address-space, and creates the page
    /// tables expected by the GSP bootloader to load it.
    pub(crate) fn new<'a, 'b>(
        dev: &'a device::Device<device::Bound>,
        chipset: Chipset,
        ver: &'b str,
    ) -> Result<impl PinInit<Self, Error> + 'a> {
        let fw = super::request_nv_firmware(dev, chipset, "gsp", ver)?;

        let fw_section = elf::elf64_section(fw.data(), ".fwimage").ok_or(EINVAL)?;

        let sigs_section = match chipset.arch() {
            Architecture::Ampere => ".fwsignature_ga10x",
            Architecture::Ada => ".fwsignature_ad10x",
            _ => return Err(ENOTSUPP),
        };
        let signatures = elf::elf64_section(fw.data(), sigs_section)
            .ok_or(EINVAL)
            .and_then(|data| DmaObject::from_data(dev, data))?;

        let size = fw_section.len();

        // Move the firmware into a vmalloc'd vector and map it into the device address
        // space.
        let fw_vvec = VVec::with_capacity(fw_section.len(), GFP_KERNEL)
            .and_then(|mut v| {
                v.extend_from_slice(fw_section, GFP_KERNEL)?;
                Ok(v)
            })
            .map_err(|_| ENOMEM)?;

        let bl = super::request_nv_firmware(dev, chipset, "bootloader", ver)?;
        let bootloader = RiscvFirmware::new(dev, &bl)?;

        Ok(try_pin_init!(&this in Self {
            fw <- SGTable::new(dev, fw_vvec, DataDirection::ToDevice, GFP_KERNEL),
            level2 <- {
                // Allocate the level 2 page table, map the firmware onto it, and map it into the
                // device address space.
                // SAFETY: `this` is a valid pointer, and `fw` has been initialized.
                let fw_sg_table = unsafe { &(*this.as_ptr()).fw };
                VVec::<u8>::with_capacity(
                    fw_sg_table.iter().count() * core::mem::size_of::<u64>(),
                    GFP_KERNEL,
                )
                .map_err(|_| ENOMEM)
                .and_then(|level2| map_into_lvl(fw_sg_table, level2))
                .map(|level2| SGTable::new(dev, level2, DataDirection::ToDevice, GFP_KERNEL))?
            },
            level1 <- {
                // Allocate the level 1 page table, map the level 2 page table onto it, and map it
                // into the device address space.
                // SAFETY: `this` is a valid pointer, and `level2` has been initialized.
                let level2_sg_table = unsafe { &(*this.as_ptr()).level2 };
                VVec::<u8>::with_capacity(
                    level2_sg_table.iter().count() * core::mem::size_of::<u64>(),
                    GFP_KERNEL,
                )
                .map_err(|_| ENOMEM)
                .and_then(|level1| map_into_lvl(level2_sg_table, level1))
                .map(|level1| SGTable::new(dev, level1, DataDirection::ToDevice, GFP_KERNEL))?
            },
            level0: {
                // Allocate the level 0 page table as a device-visible DMA object, and map the
                // level 1 page table onto it.
                // SAFETY: `this` is a valid pointer, and `level1` has been initialized.
                let level1_sg_table = unsafe { &(*this.as_ptr()).level1 };
                let mut level0 = DmaObject::new(dev, GSP_PAGE_SIZE)?;
                // SAFETY: we are the only owner of this newly-created object, making races
                // impossible.
                let level0_slice = unsafe { level0.as_slice_mut(0, GSP_PAGE_SIZE) }?;
                level0_slice[0..core::mem::size_of::<u64>()].copy_from_slice(
                    #[allow(clippy::useless_conversion)]
                    &(u64::from(level1_sg_table.iter().next().unwrap().dma_address()))
                        .to_le_bytes(),
                );

                level0
            },
            size,
            signatures,
            bootloader,
        }))
    }

    /// Returns the DMA handle of the radix3 level 0 page table.
    pub(crate) fn radix3_dma_handle(&self) -> DmaAddress {
        self.level0.dma_handle()
    }
}

/// Build a page table from a scatter-gather list.
///
/// Takes each DMA-mapped region from `sg_table` and writes page table entries
/// for all 4KB pages within that region. For example, a 16KB SG entry becomes
/// 4 consecutive page table entries.
fn map_into_lvl(sg_table: &SGTable<Owned<VVec<u8>>>, mut dst: VVec<u8>) -> Result<VVec<u8>> {
    for sg_entry in sg_table.iter() {
        // Number of pages we need to map.
        let num_pages = (sg_entry.dma_len() as usize).div_ceil(GSP_PAGE_SIZE);

        for i in 0..num_pages {
            let entry = sg_entry.dma_address() + (i as u64 * GSP_PAGE_SIZE as u64);
            dst.extend_from_slice(&entry.to_le_bytes(), GFP_KERNEL)?;
        }
    }

    Ok(dst)
}
