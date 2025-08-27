// SPDX-License-Identifier: GPL-2.0

use kernel::alloc::flags::GFP_KERNEL;
use kernel::bindings;
use kernel::device;
use kernel::dma::CoherentAllocation;
use kernel::dma_write;
use kernel::pci;
use kernel::prelude::*;
use kernel::ptr::Alignment;
use kernel::transmute::{AsBytes, FromBytes};

use crate::gsp::cmdq::GspCmdq;
use crate::nvfw::{
    LibosMemoryRegionInitArgument, GSP_ARGUMENTS_CACHED, GSP_SR_INIT_ARGUMENTS,
    MESSAGE_QUEUE_INIT_ARGUMENTS,
};

pub(crate) mod cmdq;

pub(crate) const GSP_PAGE_SHIFT: usize = 12;
pub(crate) const GSP_PAGE_SIZE: usize = 1 << GSP_PAGE_SHIFT;
pub(crate) const GSP_HEAP_ALIGNMENT: Alignment = Alignment::new(1 << 20);

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for GSP_ARGUMENTS_CACHED {}

// SAFETY: This struct only contains integer types for which all bit patterns
// are valid.
unsafe impl FromBytes for GSP_ARGUMENTS_CACHED {}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for MESSAGE_QUEUE_INIT_ARGUMENTS {}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for GSP_SR_INIT_ARGUMENTS {}

#[allow(unused)]
pub(crate) struct GspMemObjects {
    libos: CoherentAllocation<LibosMemoryRegionInitArgument>,
    pub loginit: CoherentAllocation<u8>,
    pub logintr: CoherentAllocation<u8>,
    pub logrm: CoherentAllocation<u8>,
    pub cmdq: GspCmdq,
    rmargs: CoherentAllocation<GSP_ARGUMENTS_CACHED>,
}

/// Creates a self-mapping page table for `obj` at its beginning.
fn create_pte_array<T: AsBytes + FromBytes>(obj: &mut CoherentAllocation<T>, skip: usize) {
    let num_pages = obj.size().div_ceil(GSP_PAGE_SIZE);
    let handle = obj.dma_handle();

    // SAFETY:
    //  - By the invariants of the CoherentAllocation ptr is non-NULL.
    //  - CoherentAllocation CPU addresses are always aligned to a
    //    page-boundary, satisfying the alignment requirements for
    //    from_raw_parts_mut()
    //  - The allocation size is at least as long as 8 * num_pages as
    //    GSP_PAGE_SIZE is larger than 8 bytes.
    let ptes = unsafe {
        let ptr = obj.start_ptr_mut().cast::<u64>().add(skip);
        core::slice::from_raw_parts_mut(ptr, num_pages)
    };

    for (i, pte) in ptes.iter_mut().enumerate() {
        *pte = handle + ((i as u64) << GSP_PAGE_SHIFT);
    }
}

/// Creates a new `CoherentAllocation<A>` with `name` of `size` elements, and
/// register it into the `libos` object at argument position `libos_arg_nr`.
fn create_coherent_dma_object<A: AsBytes + FromBytes>(
    dev: &device::Device<device::Bound>,
    name: &'static str,
    size: usize,
    libos: &mut CoherentAllocation<LibosMemoryRegionInitArgument>,
    libos_arg_nr: usize,
) -> Result<CoherentAllocation<A>> {
    let obj = CoherentAllocation::<A>::alloc_coherent(dev, size, GFP_KERNEL | __GFP_ZERO)?;

    dma_write!(libos[libos_arg_nr] = LibosMemoryRegionInitArgument::new(name, &obj))?;

    Ok(obj)
}

impl GspMemObjects {
    pub(crate) fn new(pdev: &pci::Device<device::Bound>) -> Result<Self> {
        let dev = pdev.as_ref();
        let mut libos = CoherentAllocation::<LibosMemoryRegionInitArgument>::alloc_coherent(
            dev,
            GSP_PAGE_SIZE / size_of::<LibosMemoryRegionInitArgument>(),
            GFP_KERNEL | __GFP_ZERO,
        )?;
        let mut loginit = create_coherent_dma_object::<u8>(dev, "LOGINIT", 0x10000, &mut libos, 0)?;
        create_pte_array(&mut loginit, 1);
        let mut logintr = create_coherent_dma_object::<u8>(dev, "LOGINTR", 0x10000, &mut libos, 1)?;
        create_pte_array(&mut logintr, 1);
        let mut logrm = create_coherent_dma_object::<u8>(dev, "LOGRM", 0x10000, &mut libos, 2)?;
        create_pte_array(&mut logrm, 1);

        // Creates its own PTE array
        let mut cmdq = GspCmdq::new(dev)?;
        let rmargs =
            create_coherent_dma_object::<GSP_ARGUMENTS_CACHED>(dev, "RMARGS", 1, &mut libos, 3)?;
        let (shared_mem_phys_addr, cmd_queue_offset, stat_queue_offset) = cmdq.get_cmdq_offsets();

        dma_write!(
            rmargs[0].messageQueueInitArguments = MESSAGE_QUEUE_INIT_ARGUMENTS {
                sharedMemPhysAddr: shared_mem_phys_addr,
                pageTableEntryCount: cmdq.nr_ptes,
                cmdQueueOffset: cmd_queue_offset,
                statQueueOffset: stat_queue_offset,
                ..Default::default()
            }
        )?;
        dma_write!(
            rmargs[0].srInitArguments = GSP_SR_INIT_ARGUMENTS {
                oldLevel: 0,
                flags: 0,
                bInPMTransition: 0,
                ..Default::default()
            }
        )?;
        dma_write!(rmargs[0].bDmemStack = 1)?;

        Ok(GspMemObjects {
            libos,
            loginit,
            logintr,
            logrm,
            rmargs,
            cmdq,
        })
    }

    #[expect(unused)]
    pub(crate) fn libos_dma_handle(&self) -> bindings::dma_addr_t {
        self.libos.dma_handle()
    }
}
