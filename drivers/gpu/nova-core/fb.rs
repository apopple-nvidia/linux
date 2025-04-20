// SPDX-License-Identifier: GPL-2.0

use core::ops::Range;

use kernel::num::NumExt;
use kernel::prelude::*;
use kernel::types::ARef;
use kernel::{dev_warn, device};

use crate::dma::DmaObject;
use crate::driver::Bar0;
use crate::gpu::Chipset;
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

/// Layout of the GPU framebuffer memory.
///
/// Contains ranges of GPU memory reserved for a given purpose during the GSP bootup process.
#[derive(Debug)]
#[expect(dead_code)]
pub(crate) struct FbLayout {
    pub fb: Range<u64>,
    pub vga_workspace: Range<u64>,
    pub frts: Range<u64>,
}

impl FbLayout {
    /// Computes the FB layout.
    pub(crate) fn new(chipset: Chipset, bar: &Bar0) -> Result<Self> {
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

        Ok(Self {
            fb,
            vga_workspace,
            frts,
        })
    }
}
