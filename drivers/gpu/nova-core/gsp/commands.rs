// SPDX-License-Identifier: GPL-2.0

use core::alloc::Layout;
use kernel::alloc::allocator::Kmalloc;
use kernel::alloc::flags::GFP_KERNEL;
use kernel::alloc::kvec::KVec;
use kernel::alloc::Allocator;
use kernel::device;
use kernel::pci;
use kernel::prelude::*;
use kernel::time::Delta;
use kernel::transmute::{AsBytes, FromBytesSized};

use crate::driver::Bar0;
use crate::gsp::cmdq::{GspCmdq, GspCommand, GspMessage};
use crate::gsp::GSP_PAGE_SIZE;
use crate::nvfw::r570_144 as fw;
use crate::sbuffer::SBuffer;

unsafe impl FromBytesSized for fw::GSP_ARGUMENTS_CACHED {}
unsafe impl AsBytes for fw::GSP_ARGUMENTS_CACHED {}
unsafe impl AsBytes for fw::MESSAGE_QUEUE_INIT_ARGUMENTS {}
unsafe impl AsBytes for fw::GSP_SR_INIT_ARGUMENTS {}
unsafe impl FromBytesSized for fw::GspFwWprMeta {}
unsafe impl AsBytes for fw::GspFwWprMeta {}
unsafe impl FromBytesSized for fw::GspSystemInfo {}
unsafe impl AsBytes for fw::GspSystemInfo {}
unsafe impl FromBytesSized for fw::GspStaticConfigInfo_t {}

/// FB region information
#[derive(Debug, Default, Copy, Clone)]
pub(crate) struct FbRegion {
    pub addr: u64,
    pub size: u64,
}

pub(crate) struct GspStaticConfigInfo {
    pub gpu_name: [u8; 40],
    pub h_internal_client: u32,
    pub h_internal_device: u32,
    pub h_internal_subdevice: u32,
    pub fb_regions: KVec<FbRegion>,
    pub fb_region_count: usize,
    pub bar1_pdb: u64,
    pub bar2_pdb: u64,
}

struct GspInitDone {}
impl GspMessage for GspInitDone {
    const FUNCTION: u32 = fw::NV_VGPU_MSG_EVENT_GSP_INIT_DONE;
}

pub(crate) fn gsp_init_done(
    cmdq: &mut GspCmdq,
    dev: &device::Device<device::Bound>,
    timeout: Delta,
) -> Result {
    loop {
        cmdq.wait_msg_available(timeout)?;
        let msg = loop {
            match cmdq.receive_msg(dev) {
                Ok(x) => break Ok(x),
                Err(EAGAIN) => continue,
                Err(x) => break Err(x),
            };
        }?;

        let init_done = msg.try_as::<GspInitDone>().map(|_| ());

        msg.ack()?;

        match init_done {
            Ok(_) => break Ok(()),
            Err(ERANGE) => continue,
            Err(e) => break Err(e),
        };
    }
}

impl GspMessage for fw::GspStaticConfigInfo_t {
    const FUNCTION: u32 = fw::NV_VGPU_MSG_FUNCTION_GET_GSP_STATIC_INFO;
}

impl GspCommand for fw::GspStaticConfigInfo_t {
    const FUNCTION: u32 = fw::NV_VGPU_MSG_FUNCTION_GET_GSP_STATIC_INFO;
}

pub(crate) fn get_gsp_info(
    cmdq: &mut GspCmdq,
    dev: &device::Device<device::Bound>,
    bar: &Bar0,
) -> Result<GspStaticConfigInfo> {
    let mut msg = cmdq.alloc_gsp_queue_command(size_of::<fw::GspStaticConfigInfo_t>())?;
    msg.try_as::<fw::GspStaticConfigInfo_t>();
    msg.send(bar)?;
    cmdq.wait_msg_available(Delta::from_secs(5))?;
    let msg = cmdq.receive_msg(dev)?;
    let info = msg.try_as::<fw::GspStaticConfigInfo_t>().map(|(x, _)| x)?;

    let gpu_name_str = info
        .gpuNameString
        .get(
            0..=info
                .gpuNameString
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(info.gpuNameString.len() - 1),
        )
        .and_then(|bytes| CStr::from_bytes_with_nul(bytes).ok())
        .and_then(|cstr| cstr.to_str().ok())
        .unwrap_or("invalid utf8");

    let mut gpu_name = [0u8; 40];
    let bytes = gpu_name_str.as_bytes();
    let copy_len = core::cmp::min(bytes.len(), gpu_name.len());
    gpu_name[..copy_len].copy_from_slice(&bytes[..copy_len]);
    gpu_name[copy_len] = b'\0';

    // Parse FB regions
    let mut fb_regions = KVec::new();
    let fb_info = &info.fbRegionInfoParams;

    // TODO: Need to use dev_dbg
    pr_info!("nova: Found {} FB regions\n", fb_info.numFBRegions);

    for i in 0..fb_info.numFBRegions as usize {
        if i >= 16 {
            break;
        } // Max regions in the array
        let region = &fb_info.fbRegion[i];

        // TODO: Need to use dev_dbg
        pr_info!("nova: FB region {}: base={:#x} limit={:#x} reserved={:#x} compressed={} iso={} protected={}\n",
            i, region.base, region.limit, region.reserved,
            region.supportCompressed, region.supportISO, region.bProtected);

        // Only add usable regions (not reserved, not protected, supports compression and ISO)
        if region.reserved == 0 && region.bProtected == 0 {
            if region.supportCompressed != 0 && region.supportISO != 0 {
                let size = (region.limit + 1) - region.base;
                fb_regions.push(
                    FbRegion {
                        addr: region.base,
                        size,
                    },
                    GFP_KERNEL,
                )?;
            }
        }
    }

    let fb_region_count = fb_regions.len();
    let config_info = GspStaticConfigInfo {
        gpu_name,
        h_internal_client: info.hInternalClient,
        h_internal_device: info.hInternalDevice,
        h_internal_subdevice: info.hInternalSubdevice,
        fb_regions,
        fb_region_count,
        bar1_pdb: info.bar1PdeBase,
        bar2_pdb: info.bar2PdeBase,
    };

    msg.ack()?;
    Ok(config_info)
}

const GSP_REGISTRY_NUM_ENTRIES: usize = 2;
struct RegistryEntry {
    key: &'static str,
    value: u32,
}

struct RegistryTable {
    entries: [RegistryEntry; GSP_REGISTRY_NUM_ENTRIES],
}

struct GspRegistryTable;
impl GspCommand for GspRegistryTable {
    const FUNCTION: u32 = fw::NV_VGPU_MSG_FUNCTION_SET_REGISTRY;
}

impl RegistryTable {
    fn copy_to_sbuf_iter(&self, mut sbuf: SBuffer<core::array::IntoIter<&mut [u8], 2>>) -> Result {
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

        sbuf.write_all(cmd_slice)?;

        // Free the allocated memory by converting slice back to pointer.
        unsafe {
            use core::ptr::NonNull;
            let ptr = cmd_slice.as_ptr() as *mut u8;
            let ptr_nn = NonNull::new_unchecked(ptr);
            Kmalloc::free(ptr_nn, layout);
        }

        Ok(())
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

pub(crate) fn build_registry(cmdq: &mut GspCmdq, bar: &Bar0) -> Result {
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
    let mut msg = cmdq.alloc_gsp_queue_command(registry.size())?;
    {
        let (_, some_sbuf) = msg.try_as::<GspRegistryTable>();
        let sbuf = some_sbuf.ok_or(ENOMEM)?;
        registry.copy_to_sbuf_iter(sbuf)?;
    }
    msg.send(bar)?;

    Ok(())
}

impl GspCommand for fw::GspSystemInfo {
    const FUNCTION: u32 = fw::NV_VGPU_MSG_FUNCTION_GSP_SET_SYSTEM_INFO;
}

pub(crate) fn set_system_info(
    cmdq: &mut GspCmdq,
    dev: &pci::Device<device::Bound>,
    bar: &Bar0,
) -> Result {
    build_assert!(size_of::<fw::GspSystemInfo>() < GSP_PAGE_SIZE);
    let mut msg = cmdq.alloc_gsp_queue_command(size_of::<fw::GspSystemInfo>())?;
    {
        let (info, _) = msg.try_as::<fw::GspSystemInfo>();

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
    }
    msg.send(bar)?;
    Ok(())
}
