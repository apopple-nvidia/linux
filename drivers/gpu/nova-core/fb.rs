// SPDX-License-Identifier: GPL-2.0

use core::ops::Range;

use kernel::num::NumExt;
use kernel::prelude::*;
use kernel::types::ARef;
use kernel::{dev_warn, device};

use crate::dma::DmaObject;
use crate::driver::Bar0;
use crate::firmware::Firmware;
use crate::gpu::Chipset;
use crate::gsp::GSP_HEAP_SHIFT;
use crate::nvfw::r570_133_07 as nvfw;
use crate::regs;

mod hal;

/// Type holding the sysmem flush memory page, a page of memory to be written into the
/// `NV_PFB_NISO_FLUSH_SYSMEM_ADDR*` registers and used to maintain memory coherency.
///
/// Users are responsible for manually calling [`Self::unregister`] before dropping this object, or
/// the page might remain in use even after it has been freed.
pub(crate) struct SysmemFlush {
    /// Chipset we are operating on.
    chipset: Chipset,
    device: ARef<device::Device>,
    /// Keep the page alive as long as we need it.
    page: DmaObject,
}

impl SysmemFlush {
    /// Allocate a memory page and register it as the sysmem flush page.
    pub(crate) fn register(
        dev: &device::Device<device::Bound>,
        bar: &Bar0,
        chipset: Chipset,
    ) -> Result<Self> {
        let page = DmaObject::new(dev, kernel::bindings::PAGE_SIZE)?;

        chipset
            .fb_hal()
            .write_sysmem_flush_page(bar, page.dma_handle())?;

        Ok(Self {
            chipset,
            device: dev.into(),
            page,
        })
    }

    /// Unregister the managed sysmem flush page.
    ///
    /// Users must make sure to call this method before dropping the object.
    pub(crate) fn unregister(self, bar: &Bar0) {
        let hal = self.chipset.fb_hal();

        if hal.read_sysmem_flush_page(bar) == self.page.dma_handle() {
            let _ = hal.write_sysmem_flush_page(bar, 0).inspect_err(|e| {
                dev_warn!(
                    &self.device,
                    "failed to unregister sysmem flush page: {:?}",
                    e
                )
            });
        } else {
            // Another page has been registered after us for some reason - warn as this is a bug.
            dev_warn!(
                &self.device,
                "attempt to unregister a sysmem flush page that is not active\n"
            );
        }
    }
}

/// Computes the size of the WPR heap.
fn calc_wpr_heap(chipset: Chipset, fb_size_fb: u64) -> u64 {
    let (carveout, heap_min) = if chipset >= Chipset::GA102 {
        (
            nvfw::GSP_FW_HEAP_PARAM_OS_SIZE_LIBOS3_BAREMETAL as u64,
            nvfw::GSP_FW_HEAP_SIZE_OVERRIDE_LIBOS3_BAREMETAL_MIN_MB << 20,
        )
    } else {
        (
            nvfw::GSP_FW_HEAP_PARAM_OS_SIZE_LIBOS2 as u64,
            nvfw::GSP_FW_HEAP_SIZE_OVERRIDE_LIBOS2_MIN_MB << 20,
        )
    };

    let size = carveout
        + nvfw::GSP_FW_HEAP_PARAM_BASE_RM_SIZE_TU10X as u64
        + (nvfw::GSP_FW_HEAP_PARAM_SIZE_PER_GB_FB as u64 * fb_size_fb)
            .next_multiple_of(GSP_HEAP_SHIFT)
        + (nvfw::GSP_FW_HEAP_PARAM_CLIENT_ALLOC_SIZE as u64).next_multiple_of(GSP_HEAP_SHIFT);

    core::cmp::max(size, heap_min as u64)
}

/// Layout of the GPU framebuffer memory.
///
/// Contains ranges of GPU memory reserved for a given purpose during the GSP bootup process.
#[derive(Debug)]
#[expect(dead_code)]
pub(crate) struct FbLayout {
    pub fb: Range<u64>,
    pub vga_workspace: Range<u64>,
    pub frts: Range<u64>,
    pub boot: Range<u64>,
    pub elf: Range<u64>,
    pub wpr2_heap: Range<u64>,
    pub vf_partition_count: u8,
    pub wpr2: Range<u64>,

    pub heap: Range<u64>,
    pub region: [Range<u64>; 16],
    pub nr_region: usize,
    pub rsvd_size: u32,
}

impl FbLayout {
    /// Computes the FB layout.
    pub(crate) fn new(chipset: Chipset, bar: &Bar0, fw: &Firmware) -> Result<Self> {
        let hal = chipset.fb_hal();

        let fb = {
            let fb_size = hal.vidmem_size(bar);

            0..fb_size
        };

        let vga_workspace = {
            let vga_base = {
                const NV_PRAMIN_SIZE: u64 = 0x100000;
                let base = fb.end - NV_PRAMIN_SIZE;

                if hal.supports_display(bar) {
                    match regs::NV_PDISP_VGA_WORKSPACE_BASE::read(bar).vga_workspace_addr() {
                        Some(addr) => {
                            if addr < base {
                                const VBIOS_WORKSPACE_SIZE: u64 = 0x20000;

                                // Point workspace address to end of framebuffer.
                                fb.end - VBIOS_WORKSPACE_SIZE
                            } else {
                                addr
                            }
                        }
                        None => base,
                    }
                } else {
                    base
                }
            };

            vga_base..fb.end
        };

        let frts = {
            const FRTS_DOWN_ALIGN: u64 = 0x20000;
            const FRTS_SIZE: u64 = 0x100000;
            let frts_base = vga_workspace.start.align_down(FRTS_DOWN_ALIGN) - FRTS_SIZE;

            frts_base..frts_base + FRTS_SIZE
        };

        let boot = {
            const BOOTLOADER_DOWN_ALIGN: u64 = 0x1000;
            let bootloader_size = fw.bootloader.ucode.size() as u64;
            let bootloader_base = (frts.start - bootloader_size).align_down(BOOTLOADER_DOWN_ALIGN);

            bootloader_base..bootloader_base + bootloader_size
        };

        let elf = {
            const ELF_DOWN_ALIGN: u64 = 0x10000;
            let elf_size = fw.gsp.size() as u64;
            let elf_addr = (boot.start - elf_size).align_down(ELF_DOWN_ALIGN);

            elf_addr..elf_addr + elf_size
        };

        let fb_size_fb = fb.end.div_ceil(1 << 30);
        let wpr2_heap = {
            const WPR2_HEAP_DOWN_ALIGN: u64 = 0x100000;
            let wpr2_heap_size = calc_wpr_heap(chipset, fb_size_fb);
            let wpr2_heap_addr = (elf.start - wpr2_heap_size).align_down(WPR2_HEAP_DOWN_ALIGN);

            wpr2_heap_addr..elf.start.align_down(WPR2_HEAP_DOWN_ALIGN)
        };

        let wpr2 = {
            const WPR2_DOWN_ALIGN: u64 = 0x100000;
            let wpr2_addr = (wpr2_heap.start - nvfw::s_GspFwWprMeta::str_size() as u64)
                .align_down(WPR2_DOWN_ALIGN);

            wpr2_addr..frts.end
        };

        let heap = {
            const HEAP_SIZE: u64 = 0x100000;

            wpr2.start - HEAP_SIZE..wpr2.start
        };

        Ok(Self {
            fb,
            vga_workspace,
            frts,
            boot,
            elf,
            wpr2_heap,
            wpr2,
            heap,
            vf_partition_count: 0,
            region: Default::default(),
            nr_region: 0,
            rsvd_size: 0,
        })
    }
}
