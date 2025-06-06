// SPDX-License-Identifier: GPL-2.0

use kernel::device;
use kernel::devres::Devres;
use kernel::dma::CoherentAllocation;
use kernel::pci;
use kernel::prelude::*;

use crate::dma::DmaObject;
use crate::driver::Bar0;
use crate::fb::FbLayout;
use crate::firmware::Firmware;
use crate::nvfw::r570_144 as fw;

pub(crate) const GSP_PAGE_SHIFT: usize = 12;
pub(crate) const GSP_PAGE_SIZE: usize = 1 << GSP_PAGE_SHIFT;
pub(crate) const GSP_HEAP_SHIFT: u64 = 1 << 20;

unsafe impl FromBytes for fw::GspFwWprMeta {}
unsafe impl AsBytes for fw::GspFwWprMeta {}
unsafe impl FromBytes for fw::GspSystemInfo {}
unsafe impl AsBytes for fw::GspSystemInfo {}

pub(crate) fn build_wpr_meta(
    dev: &device::Device<device::Bound>,
    fw: &Firmware,
    fb_layout: &FbLayout,
) -> Result<CoherentAllocation<fw::GspFwWprMeta>> {
    let mut wpr_meta =
        CoherentAllocation::<fw::GspFwWprMeta>::alloc_coherent(dev, 1, GFP_KERNEL | __GFP_ZERO)?;
    dma_write!(wpr_meta[0].magic = fw::GSP_FW_WPR_META_MAGIC as u64);
    dma_write!(wpr_meta[0].revision = fw::GSP_FW_WPR_META_REVISION as u64);
    dma_write!(wpr_meta[0].sysmemAddrOfRadix3Elf = fw.gsp.lvl0_dma_handle() as u64);
    dma_write!(wpr_meta[0].sizeOfRadix3Elf = fw.gsp.size() as u64);
    dma_write!(wpr_meta[0].sysmemAddrOfBootloader = fw.bootloader.ucode.dma_handle());
    dma_write!(wpr_meta[0].sizeOfBootloader = fw.bootloader.ucode.size() as u64);
    dma_write!(wpr_meta[0].bootloaderCodeOffset = fw.bootloader.code_offset as u64);
    dma_write!(wpr_meta[0].bootloaderDataOffset = fw.bootloader.data_offset as u64);
    dma_write!(wpr_meta[0].bootloaderManifestOffset = fw.bootloader.manifest_offset as u64);
    dma_write!(
        wpr_meta[0]
            .__bindgen_anon_1
            .__bindgen_anon_1
            .sysmemAddrOfSignature = fw.gsp_sigs.dma_handle() as u64
    );
    dma_write!(
        wpr_meta[0]
            .__bindgen_anon_1
            .__bindgen_anon_1
            .sizeOfSignature = fw.gsp_sigs.size() as u64
    );
    dma_write!(wpr_meta[0].gspFwRsvdStart = fb_layout.heap.start);
    dma_write!(wpr_meta[0].nonWprHeapOffset = fb_layout.heap.start);
    dma_write!(wpr_meta[0].nonWprHeapSize = fb_layout.heap.end - fb_layout.heap.start);
    dma_write!(wpr_meta[0].gspFwWprStart = fb_layout.wpr2.start);
    dma_write!(wpr_meta[0].gspFwHeapOffset = fb_layout.wpr2_heap.start);
    dma_write!(wpr_meta[0].gspFwHeapSize = fb_layout.wpr2_heap.end - fb_layout.wpr2_heap.start);
    dma_write!(wpr_meta[0].gspFwOffset = fb_layout.elf.start);
    dma_write!(wpr_meta[0].bootBinOffset = fb_layout.boot.start);
    dma_write!(wpr_meta[0].frtsOffset = fb_layout.frts.start);
    dma_write!(wpr_meta[0].frtsSize = fb_layout.frts.end - fb_layout.frts.start);
    dma_write!(wpr_meta[0].gspFwWprEnd = fb_layout.vga_workspace.start & !(0x20000 - 1));
    dma_write!(wpr_meta[0].gspFwHeapVfPartitionCount = fb_layout.vf_partition_count);
    dma_write!(wpr_meta[0].fbSize = fb_layout.fb.end - fb_layout.fb.start);
    dma_write!(wpr_meta[0].vgaWorkspaceOffset = fb_layout.vga_workspace.start);
    dma_write!(
        wpr_meta[0].vgaWorkspaceSize = fb_layout.vga_workspace.end - fb_layout.vga_workspace.start
    );
    dma_write!(wpr_meta[0].bootCount = 0);
    dma_write!(
        wpr_meta[0]
            .__bindgen_anon_2
            .__bindgen_anon_1
            .partitionRpcAddr = 0
    );
    dma_write!(
        wpr_meta[0]
            .__bindgen_anon_2
            .__bindgen_anon_1
            .partitionRpcRequestOffset = 0
    );
    dma_write!(
        wpr_meta[0]
            .__bindgen_anon_2
            .__bindgen_anon_1
            .partitionRpcReplyOffset = 0
    );
    dma_write!(wpr_meta[0].verified = 0);

    Ok(wpr_meta)
}

unsafe impl FromBytes for fw::GSP_ARGUMENTS_CACHED {}
unsafe impl AsBytes for fw::GSP_ARGUMENTS_CACHED {}

#[allow(unused)]
pub(crate) struct GspSharedMemObjects {
    pub libos: DmaObject,
    loginit: DmaObject,
    logintr: DmaObject,
    logrm: DmaObject,
    pub rmargs: CoherentAllocation<fw::GSP_ARGUMENTS_CACHED>,
    // kern: Option<DmaObject>,
    pub cmdq: GspCmdq,
    // wpr_meta: DmaObject,
}

/// Generates the `ID8` identifier required for some GSP objects.
fn id8(name: &str) -> u64 {
    let mut bytes = [0u8; core::mem::size_of::<u64>()];

    for (c, b) in name.bytes().rev().zip(&mut bytes) {
        *b = c;
    }

    u64::from_ne_bytes(bytes)
}

/// Creates a self-mapping page table for `obj` at its beginning.
fn create_pte_array(obj: &mut DmaObject) {
    let num_pages = obj.size().div_ceil(GSP_PAGE_SIZE);
    let handle = obj.dma_handle();

    let ptes = unsafe {
        let ptr = obj
            .start_ptr_mut()
            .add(core::mem::size_of::<u64>())
            .cast::<u64>();
        core::slice::from_raw_parts_mut(ptr, num_pages)
    };

    for (i, pte) in ptes.iter_mut().enumerate() {
        *pte = handle as u64 + ((i as u64) << GSP_PAGE_SHIFT);
    }
}

/// Creates a new `DmaObject` with `name` of `size`, and register it into the `libos` object at
/// argument position `libos_arg_nr`.
fn create_dma_object(
    dev: &device::Device<device::Bound>,
    name: &'static str,
    size: usize,
    libos: &mut DmaObject,
    libos_arg_nr: usize,
) -> Result<DmaObject> {
    let mut obj = DmaObject::new(dev, size)?;

    let arg_offset = libos_arg_nr * size_of::<fw::LibosMemoryRegionInitArgument>();
    let libos_start_ptr = unsafe { libos.start_ptr_mut().add(arg_offset) };

    let libos_mem_init_args = fw::LibosMemoryRegionInitArgument {
        id8: id8(name),
        pa: obj.dma_handle(),
        size: obj.size() as u64,
        kind: fw::LibosMemoryRegionKind_LIBOS_MEMORY_REGION_CONTIGUOUS as u8,
        loc: fw::LibosMemoryRegionLoc_LIBOS_MEMORY_REGION_LOC_SYSMEM as u8,
    };
    unsafe {
        core::ptr::copy_nonoverlapping(
            &libos_mem_init_args as *const fw::LibosMemoryRegionInitArgument,
            libos_start_ptr as *mut fw::LibosMemoryRegionInitArgument,
            1,
        );
    };

    Ok(obj)
}

fn create_coherent_dma_object<A: AsBytes + FromBytes>(
    dev: &device::Device<device::Bound>,
    name: &'static str,
    libos: &mut DmaObject,
    libos_arg_nr: usize,
) -> Result<CoherentAllocation<A>> {
    let mut obj = CoherentAllocation::<A>::alloc_coherent(dev, 1, GFP_KERNEL | __GFP_ZERO)?;

    let arg_offset = libos_arg_nr * size_of::<fw::LibosMemoryRegionInitArgument>();
    let libos_start_ptr = unsafe { libos.start_ptr_mut().add(arg_offset) };

    let libos_mem_init_args = fw::LibosMemoryRegionInitArgument {
        id8: id8(name),
        pa: obj.dma_handle(),
        size: obj.size() as u64,
        kind: fw::LibosMemoryRegionKind_LIBOS_MEMORY_REGION_CONTIGUOUS as u8,
        loc: fw::LibosMemoryRegionLoc_LIBOS_MEMORY_REGION_LOC_SYSMEM as u8,
    };
    unsafe {
        core::ptr::copy_nonoverlapping(
            &libos_mem_init_args as *const fw::LibosMemoryRegionInitArgument,
            libos_start_ptr as *mut fw::LibosMemoryRegionInitArgument,
            1,
        );
    };

    Ok(obj)
}

const GSP_REGISTRY_NUM_ENTRIES: usize = 2;
struct RegistryEntry {
    key: &'static str,
    value: u32,
}

struct RegistryTable {
    entries: [RegistryEntry; GSP_REGISTRY_NUM_ENTRIES],
}

impl RegistryTable {
    // Allocate properly aligned memory and serialize the registry table
    fn allocate_and_serialize(&self) -> Result<(*mut u8, usize)> {
        let total_size = self.size();
        let align = core::mem::align_of::<fw::PACKED_REGISTRY_TABLE>();
        let layout = Layout::from_size_align(total_size, align).map_err(|_| ENOMEM)?;

        unsafe {
            // Use the kernel allocator which respects alignment
            let allocation = Kmalloc::alloc(layout, GFP_KERNEL | __GFP_ZERO)?;
            let ptr = allocation.as_ptr() as *mut u8;

            // Verify alignment (debug only)
            debug_assert_eq!(ptr as usize % align, 0);

            // Serialize the data into the allocated memory
            let table = ptr as *mut fw::PACKED_REGISTRY_TABLE;
            let mut table_data = ptr.add(
                size_of::<fw::PACKED_REGISTRY_TABLE>()
                    + GSP_REGISTRY_NUM_ENTRIES * size_of::<fw::PACKED_REGISTRY_ENTRY>(),
            );

            (*table).numEntries = GSP_REGISTRY_NUM_ENTRIES as u32;
            (*table).size = total_size as u32;

            for i in 0..GSP_REGISTRY_NUM_ENTRIES {
                let entry_ptr = ptr.add(
                    size_of::<fw::PACKED_REGISTRY_TABLE>()
                        + i * size_of::<fw::PACKED_REGISTRY_ENTRY>(),
                ) as *mut fw::PACKED_REGISTRY_ENTRY;

                (*entry_ptr).nameOffset = table_data.offset_from(table as *const u8) as u32;
                (*entry_ptr).type_ = fw::REGISTRY_TABLE_ENTRY_TYPE_DWORD as u8;
                (*entry_ptr).data = self.entries[i].value;
                (*entry_ptr).length = 0;

                // Copy the key string to table_data and null terminate it
                let key_bytes = self.entries[i].key.as_bytes();
                core::ptr::copy_nonoverlapping(key_bytes.as_ptr(), table_data, key_bytes.len());
                table_data = table_data.add(key_bytes.len());
                *table_data = 0; // Add null terminator
                table_data = table_data.add(1); // Move past null terminator
            }

            Ok((ptr, total_size))
        }
    }
}

impl GspMessageElement for RegistryTable {
    fn copy_to_slice(
        &self,
        sub_index: usize,
        msg_slice_1: &mut [[u8; GSP_PAGE_SIZE]],
        msg_slice_2: &mut Option<&mut [[u8; GSP_PAGE_SIZE]]>,
    ) {
        let total_size = self.size();
        let align = core::mem::align_of::<fw::PACKED_REGISTRY_TABLE>();
        let layout = Layout::from_size_align(total_size, align)
            .map_err(|_| ENOMEM)
            .unwrap();
        let cmd_slice = unsafe {
            // Use the kernel allocator which respects alignment
            let allocation = Kmalloc::alloc(layout, GFP_KERNEL | __GFP_ZERO).unwrap();
            let ptr = allocation.as_ptr() as *mut u8;

            // Verify alignment (debug only)
            debug_assert_eq!(ptr as usize % align, 0);

            // Serialize the data into the allocated memory
            let table = ptr as *mut fw::PACKED_REGISTRY_TABLE;
            let mut table_data = ptr.add(
                size_of::<fw::PACKED_REGISTRY_TABLE>()
                    + GSP_REGISTRY_NUM_ENTRIES * size_of::<fw::PACKED_REGISTRY_ENTRY>(),
            );

            (*table).numEntries = GSP_REGISTRY_NUM_ENTRIES as u32;
            (*table).size = total_size as u32;

            for i in 0..GSP_REGISTRY_NUM_ENTRIES {
                let entry_ptr = ptr.add(
                    size_of::<fw::PACKED_REGISTRY_TABLE>()
                        + i * size_of::<fw::PACKED_REGISTRY_ENTRY>(),
                ) as *mut fw::PACKED_REGISTRY_ENTRY;

                (*entry_ptr).nameOffset = table_data.offset_from(table as *const u8) as u32;
                (*entry_ptr).type_ = fw::REGISTRY_TABLE_ENTRY_TYPE_DWORD as u8;
                (*entry_ptr).data = self.entries[i].value;
                (*entry_ptr).length = 0;

                // Copy the key string to table_data and null terminate it
                let key_bytes = self.entries[i].key.as_bytes();
                core::ptr::copy_nonoverlapping(key_bytes.as_ptr(), table_data, key_bytes.len());
                table_data = table_data.add(key_bytes.len());
                *table_data = 0; // Add null terminator
                table_data = table_data.add(1); // Move past null terminator
            }

            core::slice::from_raw_parts(ptr as *const u8, layout.size())
        };

        // Use the common copying logic from the trait
        self.copy_slice_to_ring_buffer(cmd_slice, sub_index, msg_slice_1, msg_slice_2);

        // Free the allocated memory by converting slice back to pointer
        unsafe {
            use core::ptr::NonNull;
            let ptr = cmd_slice.as_ptr() as *mut u8;
            let ptr_nn = NonNull::new_unchecked(ptr);
            Kmalloc::free(ptr_nn, layout);
        }
    }

    unsafe fn copy_to(&self, ptr: *mut c_void) -> *mut c_void {
        // We need to construct the GSP representation of the RegistryTable which we do in place.
        unsafe {
            let table = ptr as *mut fw::PACKED_REGISTRY_TABLE;
            let mut table_data = (ptr as *const u8).add(
                size_of::<fw::PACKED_REGISTRY_TABLE>()
                    + GSP_REGISTRY_NUM_ENTRIES * size_of::<fw::PACKED_REGISTRY_ENTRY>(),
            ) as *mut u8;
            (*table).numEntries = 2;
            (*table).size = self.size() as u32;

            for i in 0..GSP_REGISTRY_NUM_ENTRIES {
                let entry_ptr = (ptr as *const u8).add(
                    size_of::<fw::PACKED_REGISTRY_TABLE>()
                        + i * size_of::<fw::PACKED_REGISTRY_ENTRY>(),
                ) as *mut fw::PACKED_REGISTRY_ENTRY;

                (*entry_ptr).nameOffset = table_data.byte_offset_from(table) as u32;
                (*entry_ptr).type_ = fw::REGISTRY_TABLE_ENTRY_TYPE_DWORD as u8;
                (*entry_ptr).data = self.entries[i].value;
                (*entry_ptr).length = 0;

                // Copy the key string to table_data and null terminate it
                let key_bytes = self.entries[i].key.as_bytes();
                core::ptr::copy_nonoverlapping(key_bytes.as_ptr(), table_data, key_bytes.len());
                table_data = table_data.add(key_bytes.len());
                *table_data = 0; // Add null terminator
                table_data = table_data.add(1); // Move past null terminator
            }

            (ptr as *const u8).add((*table).size as usize) as *mut c_void
        }
    }

    fn size(&self) -> usize {
        let mut key_size = 0;
        for i in 0..GSP_REGISTRY_NUM_ENTRIES {
            key_size += self.entries[i].key.len() + 1; // +1 for NULL terminator
        }
        size_of::<fw::PACKED_REGISTRY_TABLE>()
            + GSP_REGISTRY_NUM_ENTRIES * size_of::<fw::PACKED_REGISTRY_ENTRY>()
            + key_size
    }
}

fn build_registry(cmdq: &mut GspCmdq, bar: &Devres<Bar0>) {
    let registry = RegistryTable {
        entries: [
            RegistryEntry {
                key: "RMSecBusResetEnable",
                value: 1,
            },
            RegistryEntry {
                key: "RMForcePcieConfigSave",
                value: 1,
            },
        ],
    };

    cmdq.send(bar, fw::NV_VGPU_MSG_FUNCTION_SET_REGISTRY, &registry);
}

impl GspMessageElement for fw::GspSystemInfo {}

fn set_system_info(
    dev: &pci::Device<device::Bound>,
    cmdq: &mut GspCmdq,
    bar: &Devres<Bar0>,
) -> Result {
    let mut info = unsafe { MaybeUninit::<fw::GspSystemInfo>::zeroed().assume_init() };

    info.gpuPhysAddr = dev.resource_start(0)?;
    info.gpuPhysFbAddr = dev.resource_start(1)?;
    info.gpuPhysInstAddr = dev.resource_start(3)?;
    info.nvDomainBusDeviceFunc = dev.dev_id() as u64;

    // Using TASK_SIZE in r535_gsp_rpc_set_system_info() seems wrong because
    // TASK_SIZE is per-task. That's probably a design issue in GSP-RM though.
    info.maxUserVa = (1 << 47) - 4096;
    info.pciConfigMirrorBase = 0x088000;
    info.pciConfigMirrorSize = 0x001000;

    info.PCIDeviceID = ((dev.device_id() as u32) << 16) | dev.vendor_id() as u32;
    info.PCISubDeviceID =
        ((dev.subsystem_device_id() as u32) << 16) | dev.subsystem_vendor_id() as u32;
    info.PCIRevisionID = dev.revision_id() as u32;
    info.bIsPrimary = 0;
    info.bPreserveVideoMemoryAllocations = 0;

    cmdq.send(bar, fw::NV_VGPU_MSG_FUNCTION_GSP_SET_SYSTEM_INFO, &info);
    Ok(())
}

impl GspSharedMemObjects {
    pub(crate) fn new(pdev: &pci::Device<device::Bound>, bar: &Devres<Bar0>) -> Result<Self> {
        let dev = pdev.as_ref();
        let mut libos = DmaObject::new(dev, GSP_PAGE_SIZE)?;
        let mut loginit = create_dma_object(dev, "LOGINIT", 0x10000, &mut libos, 0)?;
        create_pte_array(&mut loginit);
        let mut logintr = create_dma_object(dev, "LOGINTR", 0x10000, &mut libos, 1)?;
        create_pte_array(&mut logintr);
        let mut logrm = create_dma_object(dev, "LOGRM", 0x10000, &mut libos, 2)?;
        create_pte_array(&mut logrm);

        // Creates its own PTE array
        let mut cmdq = GspCmdq::new(dev)?;
        let rmargs =
            create_coherent_dma_object::<fw::GSP_ARGUMENTS_CACHED>(dev, "RMARGS", &mut libos, 3)?;
        dma_write!(
            rmargs[0].messageQueueInitArguments.sharedMemPhysAddr = cmdq.gsp_mem.dma_handle()
        );
        dma_write!(rmargs[0].messageQueueInitArguments.pageTableEntryCount = cmdq.nr_ptes);
        dma_write!(rmargs[0].messageQueueInitArguments.cmdQueueOffset = 0x1000);
        dma_write!(rmargs[0].messageQueueInitArguments.statQueueOffset = 0x41000);
        dma_write!(rmargs[0].srInitArguments.oldLevel = 0);
        dma_write!(rmargs[0].srInitArguments.flags = 0);
        dma_write!(rmargs[0].srInitArguments.bInPMTransition = 0);
        dma_write!(rmargs[0].bDmemStack = 1);

        set_system_info(pdev, &mut cmdq, bar)?;
        build_registry(&mut cmdq, bar);

        Ok(GspSharedMemObjects {
            libos,
            loginit,
            logintr,
            logrm,
            rmargs,
            cmdq,
        })
    }
}
