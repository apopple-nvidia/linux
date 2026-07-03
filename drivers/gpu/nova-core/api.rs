// SPDX-License-Identifier: GPL-2.0

//! Nova-core auxbus data. Contains all the methods used by the auxbus drivers
//! to interact with nova-core.

use core::pin::Pin;

use kernel::{
    auxiliary,
    device::Bound,
    prelude::*,
    types::CovariantForLt, //
};

use crate::gpu::Chipset;
use crate::gpu::Gpu;

/// API handle for the auxiliary bus child drivers to interact with nova-core.
pub struct NovaCoreApi<'bound> {
    pub(crate) gpu: Pin<&'bound Gpu<'bound>>,
}

impl NovaCoreApi<'_> {
    /// Obtain a [`NovaCoreApi`] handle from an auxiliary device registered
    /// by nova-core.
    pub fn of(adev: &auxiliary::Device<Bound>) -> Result<Pin<&NovaCoreApi<'_>>> {
        adev.registration_data::<CovariantForLt!(NovaCoreApi<'_>)>()
    }

    /// Returns the chipset of this GPU.
    pub fn chipset(&self) -> Chipset {
        self.gpu.spec.chipset
    }

    /// Returns the total usable VRAM size of this GPU in bytes.
    pub fn vram_size(&self) -> u64 {
        self.gpu.gsp_static_info.vram_size()
    }
}
