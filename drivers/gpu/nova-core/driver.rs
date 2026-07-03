// SPDX-License-Identifier: GPL-2.0

use kernel::{
    auxiliary,
    device::Core,
    pci,
    pci::{
        Class,
        ClassMask,
        Vendor, //
    },
    prelude::*,
    sizes::SZ_16M,
    sync::atomic::{
        Atomic,
        Relaxed, //
    },
    types::CovariantForLt,
};

use crate::api::NovaCoreApi;
use crate::gpu::Gpu;

/// Counter for generating unique auxiliary device IDs.
static AUXILIARY_ID_COUNTER: Atomic<u32> = Atomic::new(0);

#[pin_data]
pub(crate) struct NovaCore<'bound> {
    // Fields are dropped in declaration order: unregister the auxiliary
    // device before dropping `gpu`, and drop `gpu` before `bar` because `Gpu`
    // borrows `bar`.
    #[allow(clippy::type_complexity)]
    _reg: auxiliary::Registration<'bound, CovariantForLt!(NovaCoreApi<'_>)>,
    #[pin]
    pub(crate) gpu: Gpu<'bound>,
    bar: pci::Bar<'bound, BAR0_SIZE>,
}

pub(crate) struct NovaCoreDriver;

const BAR0_SIZE: usize = SZ_16M;

pub(crate) type Bar0<'a> = &'a pci::Bar<'a, BAR0_SIZE>;

kernel::pci_device_table!(
    PCI_TABLE,
    MODULE_PCI_TABLE,
    <NovaCoreDriver as pci::Driver>::IdInfo,
    [
        // Modern NVIDIA GPUs will show up as either VGA or 3D controllers.
        (
            pci::DeviceId::from_class_and_vendor(
                Class::DISPLAY_VGA,
                ClassMask::ClassSubclass,
                Vendor::NVIDIA
            ),
            ()
        ),
        (
            pci::DeviceId::from_class_and_vendor(
                Class::DISPLAY_3D,
                ClassMask::ClassSubclass,
                Vendor::NVIDIA
            ),
            ()
        ),
    ]
);

impl pci::Driver for NovaCoreDriver {
    type IdInfo = ();
    type Data<'bound> = NovaCore<'bound>;
    const ID_TABLE: pci::IdTable<Self::IdInfo> = &PCI_TABLE;

    fn probe<'bound>(
        pdev: &'bound pci::Device<Core<'_>>,
        _info: &'bound Self::IdInfo,
    ) -> impl PinInit<Self::Data<'bound>, Error> + 'bound {
        pin_init::pin_init_scope(move || {
            dev_dbg!(pdev, "Probe Nova Core GPU driver.\n");

            pdev.enable_device_mem()?;
            pdev.set_master();

            Ok(try_pin_init!(NovaCore {
                bar: pdev.iomap_region_sized::<BAR0_SIZE>(0, c"nova-core/bar0")?,
                // TODO: Use `&bar` self-referential pin-init syntax once available.
                //
                // SAFETY: `bar` is initialized before this expression is evaluated
                // (`try_pin_init!()` initializes fields in declaration order), lives at a pinned
                // stable address, and is dropped after `gpu` (struct field drop order).
                gpu <- Gpu::new(pdev, unsafe { &*core::ptr::from_ref(bar) }),

                // SAFETY:
                // - `NovaCore` is dropped when the device is unbound; i.e.
                //   `mem::forget()` is never called on it.
                // - `gpu` is initialized above, lives at a pinned stable
                //   address, and is dropped after `_reg` (struct field drop
                //   order).
                _reg: unsafe {
                    auxiliary::Registration::new_with_lt(
                        pdev.as_ref(),
                        c"nova-drm",
                        // TODO[XARR]: Use XArray or perhaps IDA for proper ID allocation/recycling.
                        // For now, use a simple atomic counter that never recycles IDs.
                        AUXILIARY_ID_COUNTER.fetch_add(1, Relaxed),
                        crate::MODULE_NAME,
                        NovaCoreApi {
                            // TODO: Use `&gpu` self-referential pin-init syntax once available.
                            //
                            // SAFETY: `gpu` is initialized before this expression is evaluated
                            // (`try_pin_init!()` initializes fields in declaration order), lives at
                            // a pinned stable address, and is dropped after `_reg` (struct field
                            // drop order).
                            gpu: Pin::new_unchecked(
                                &*core::ptr::from_ref(gpu.as_ref().get_ref()),
                            ),
                        },
                    )?
                },
            }))
        })
    }
}
