use super::bindings;

use kernel::prelude::*;
use kernel::transmute::{AsBytes, FromBytes};
use kernel::{device, pci};

#[repr(transparent)]
pub(crate) struct GspSystemInfo(bindings::GspSystemInfo);

impl GspSystemInfo {
    pub(crate) fn init(&mut self, dev: &pci::Device<device::Bound>) -> Result {
        self.0.gpuPhysAddr = dev.resource_start(0)?;
        self.0.gpuPhysFbAddr = dev.resource_start(1)?;
        self.0.gpuPhysInstAddr = dev.resource_start(3)?;
        self.0.nvDomainBusDeviceFunc = u64::from(dev.dev_id());

        // Using TASK_SIZE in r535_gsp_rpc_set_system_info() seems wrong because
        // TASK_SIZE is per-task. That's probably a design issue in GSP-RM though.
        self.0.maxUserVa = (1 << 47) - 4096;
        self.0.pciConfigMirrorBase = 0x088000;
        self.0.pciConfigMirrorSize = 0x001000;

        self.0.PCIDeviceID = (u32::from(dev.device_id()) << 16) | u32::from(dev.vendor_id());
        self.0.PCISubDeviceID =
            (u32::from(dev.subsystem_device_id()) << 16) | u32::from(dev.subsystem_vendor_id());
        self.0.PCIRevisionID = u32::from(dev.revision_id());
        self.0.bIsPrimary = 0;
        self.0.bPreserveVideoMemoryAllocations = 0;

        Ok(())
    }
}

// SAFETY: These structs don't meet the no-padding requirements of AsBytes but
//         that is not a problem because they are not used outside the kernel.
unsafe impl AsBytes for GspSystemInfo {}

// SAFETY: These structs don't meet the no-padding requirements of FromBytes but
//         that is not a problem because they are not used outside the kernel.
unsafe impl FromBytes for GspSystemInfo {}
