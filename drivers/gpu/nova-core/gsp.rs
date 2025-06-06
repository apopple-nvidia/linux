// SPDX-License-Identifier: GPL-2.0

// Shut up silly warnings for now
#![allow(dead_code, unused)]

use core::alloc::Layout;
use core::cmp::min;
use core::ffi::{c_char, c_int, c_void};
use core::mem::MaybeUninit;

use kernel::alloc::allocator::Kmalloc;
use kernel::alloc::flags::{GFP_KERNEL, __GFP_ZERO};
use kernel::alloc::{Allocator, Flags};
use kernel::bindings;
use kernel::device;
use kernel::devres::Devres;
use kernel::dma::CoherentAllocation;
use kernel::pci;
use kernel::prelude::*;
use kernel::transmute::{AsBytes, FromBytes};
use kernel::{asm, dma_read, dma_write, pr_info};

use crate::dma::DmaObject;
use crate::driver::Bar0;
use crate::fb::FbLayout;
use crate::firmware::Firmware;
use crate::nvfw::r570_144 as fw;
use crate::regs::NV_PGSP_QUEUE_HEAD;

pub(crate) const GSP_PAGE_SHIFT: usize = 12;
pub(crate) const GSP_PAGE_SIZE: usize = 1 << GSP_PAGE_SHIFT;
pub(crate) const GSP_HEAP_SHIFT: u64 = 1 << 20;

extern "C" {
    fn ioread32(addr: *const c_void);
    fn iowrite32(val: u32, addr: *const c_void);
    fn print_hex_dump(
        level: *const c_char,
        prefix_str: *const c_char,
        prefix_type: c_int,
        rowsize: c_int,
        groupsize: c_int,
        buf: *const c_char,
        len: usize,
        ascii: c_int,
    );
}

// We provide this trait because not all our structs are Sized so therefore the
// AsBytes and FromBytes traits don't work. However we can provide default
// implementations for all structs that are Sized, which we do here.
//
// This also allows us to create a convenient internal representation of a
// message which is only converted to bytes when actually doing the call. See the
// registry for an example.
pub(crate) trait GspMessageElement {
    fn byte_slice(&self) -> &[u8]
    where
        Self: Sized,
    {
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }

    // Helper method to copy from a byte slice to ring buffer slices
    fn copy_slice_to_ring_buffer(
        &self,
        cmd_slice: &[u8],
        sub_index: usize,
        msg_slice_1: &mut [[u8; GSP_PAGE_SIZE]],
        msg_slice_2: &mut Option<&mut [[u8; GSP_PAGE_SIZE]]>,
    ) {
        let mut index = 0;

        // Number of bytes of the command left to send
        let mut bytes_remaining = cmd_slice.len();

        // Some of the bytes in the first page are used for rpc/msg headers.
        let slice_len = min(4096 - sub_index, bytes_remaining);

        // Copy the first bit of the command into the queue
        msg_slice_1[0][sub_index..sub_index + slice_len].copy_from_slice(&cmd_slice[0..slice_len]);
        bytes_remaining -= slice_len;
        index += 1;

        // Copy the remainder of the command into queue pages
        for slice in msg_slice_1[1..].iter_mut() {
            let slice_len = min(4096, bytes_remaining);
            slice[0..slice_len].copy_from_slice(&cmd_slice[index * 4096..index * 4096 + slice_len]);
            index += 1;
            bytes_remaining -= slice_len;
        }

        if let Some(some_msg_slice) = msg_slice_2 {
            for slice in some_msg_slice.iter_mut() {
                let slice_len = min(4096, bytes_remaining);

                slice[0..slice_len]
                    .copy_from_slice(&cmd_slice[index * 4096..index * 4096 + slice_len]);
                index += 1;
                bytes_remaining -= slice_len;
            }
        }
    }

    fn copy_to_slice(
        &self,
        sub_index: usize,
        msg_slice_1: &mut [[u8; GSP_PAGE_SIZE]],
        msg_slice_2: &mut Option<&mut [[u8; GSP_PAGE_SIZE]]>,
    ) where
        Self: Sized,
    {
        let cmd_slice =
            unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, self.size()) };

        self.copy_slice_to_ring_buffer(cmd_slice, sub_index, msg_slice_1, msg_slice_2);
    }

    unsafe fn copy_to(&self, ptr: *mut c_void) -> *mut c_void
    where
        Self: Sized,
    {
        unsafe {
            core::ptr::copy_nonoverlapping(self as *const Self, ptr as *mut Self, 1);
            (ptr as *const Self).add(1) as *mut c_void
        }
    }

    unsafe fn new_from_raw(ptr: *mut c_void, size: u32) -> Result<(*mut c_void, u32, Self)>
    where
        Self: Sized,
    {
        if size < size_of::<Self>() as u32 {
            return Err(ENOMEM);
        }
        // SAFETY: We've checked the size and know the pointer is valid
        Ok((
            unsafe { (ptr as *const Self).add(1) as *mut c_void },
            size - size_of::<Self>() as u32,
            unsafe { core::ptr::read(ptr as *const Self) },
        ))
    }

    unsafe fn new(ptr: *mut c_void) -> Self
    where
        Self: Sized,
    {
        unsafe { core::ptr::read(ptr as *const Self) }
    }

    fn size(&self) -> usize
    where
        Self: Sized,
    {
        return size_of::<Self>();
    }

    fn dump(&self) {
        pr_info!("Dump not implemented for this GspMessageElement\n");
    }
}

// This next section contains constants and structures hand-coded from the GSP
// headers We could replace these with bindgen versions, but that's a bit of a
// pain because they basically end up pulling in the world (ie. definitions for
// every rpc method). So for now the hand-coded ones are fine. They are just
// structs so we can easily move to bindgen generated ones if/when we want to.

// A GSP RPC header
#[repr(C)]
#[derive(Debug)]
struct GspRpcHeader {
    header_version: u32,
    signature: u32,
    length: u32,
    function: u32,
    rpc_result: u32,
    rpc_result_private: u32,
    sequence: u32,
    cpu_rm_gfid: u32,
}
impl GspMessageElement for GspRpcHeader {}

// A GSP message element header
#[repr(C)]
#[derive(Debug)]
struct GspMsgHeader {
    auth_tag_buffer: [u8; 16],
    aad_buffer: [u8; 16],
    checksum: u32,
    sequence: u32,
    elem_count: u32,
    pad: u32,
}
impl GspMessageElement for GspMsgHeader {}

// These next two structs come from msgq_priv.h. Hopefully the will never
// need updating once the ABI is stabalised.
#[repr(C)]
#[derive(Debug)]
struct MsgqTxHeader {
    version: u32,    // queue version
    size: u32,       // bytes, page aligned
    msg_size: u32,   // entry size, bytes, must be power-of-2, 16 is minimum
    msg_count: u32,  // number of entries in queue
    write_ptr: u32,  // message id of next slot
    flags: u32,      // if set it means "i want to swap RX"
    rx_hdr_off: u32, // Offset of msgqRxHeader from start of backing store
    entry_off: u32,  // Offset of entries from start of backing store
}

#[repr(C)]
#[derive(Debug)]
struct MsgqRxHeader {
    read_ptr: u32, // message id of last message read
}

// There is no struct defined for this in the open-gpu-kernel-source headers.
// Instead it is defined by code in GspMsgQueuesInit().
#[repr(C)]
#[derive(Debug)]
struct Msgq {
    tx: MsgqTxHeader,
    rx: MsgqRxHeader,
    _pad: [u8; GSP_PAGE_SIZE - size_of::<MsgqTxHeader>() - size_of::<MsgqRxHeader>()],
    msgq: [[u8; GSP_PAGE_SIZE]; 0x3f],
}

#[repr(C)]
#[derive(Debug)]
struct GspMem {
    ptes: [u8; GSP_PAGE_SIZE],
    cpuq: Msgq,
    gspq: Msgq,
}

impl GspMessageElement for fw::GspStaticConfigInfo_t {}
impl GspMessageElement for fw::rpc_run_cpu_sequencer_v17_00 {
    fn dump(&self) {
        pr_info!("CPU Sequencer\n");
        pr_info!("{:?}\n", self);
    }
}

// Needed for CoherentAllocation
unsafe impl FromBytes for GspMem {}
unsafe impl AsBytes for GspMem {}

// SAFETY: this hack isn't :-) Only required until Nova core can boot GSP.
unsafe impl Send for GspCmdq {}

pub(crate) struct GspCmdq {
    msg_count: u32,
    seq: u32,
    gsp_mem: CoherentAllocation<GspMem>,
    cpu_ptr: *mut c_void,
    gsp_ptr: *mut c_void,
    nr_ptes: u32,
}

impl GspCmdq {
    // This is equivalent to gsp_shared_init()
    fn new(dev: &device::Device<device::Bound>) -> Result<GspCmdq> {
        let mut gsp_mem =
            CoherentAllocation::<GspMem>::alloc_coherent(dev, 1, GFP_KERNEL | __GFP_ZERO)?;

        let nr_ptes = size_of::<GspMem>() >> GSP_PAGE_SHIFT;
        build_assert!((size_of::<GspMem>() >> GSP_PAGE_SHIFT) * size_of::<u64>() <= GSP_PAGE_SIZE);

        // Basically the same as create_pte_array() but we don't skip the first
        // PTE.
        // SAFETY: By the above build_assert which ensures the number of ptes
        // fits in the GSP_PAGE_SIZE allocated for GspMem.ptes
        let ptes = unsafe {
            let ptr = gsp_mem.start_ptr_mut() as *mut u64;
            core::slice::from_raw_parts_mut(ptr, nr_ptes)
        };

        for (i, pte) in ptes.iter_mut().enumerate() {
            *pte = gsp_mem.dma_handle() as u64 + ((i as u64) << GSP_PAGE_SHIFT);
        }

        let msg_count = ((0x40000 - GSP_PAGE_SIZE) / GSP_PAGE_SIZE) as u32;
        dma_write!(gsp_mem[0].cpuq.tx.version = 0);
        dma_write!(gsp_mem[0].cpuq.tx.size = 0x40000);
        dma_write!(gsp_mem[0].cpuq.tx.entry_off = GSP_PAGE_SIZE as u32);
        dma_write!(gsp_mem[0].cpuq.tx.msg_size = GSP_PAGE_SIZE as u32);
        dma_write!(gsp_mem[0].cpuq.tx.msg_count = msg_count);
        dma_write!(gsp_mem[0].cpuq.tx.write_ptr = 0);
        dma_write!(gsp_mem[0].cpuq.tx.flags = 1);

        // TODO: Hard-coded for now because offset_of!() isn't stable for nested types
        dma_write!(gsp_mem[0].cpuq.tx.rx_hdr_off = 32);

        let cpu_ptr = unsafe { (*gsp_mem.start_ptr_mut()).cpuq.msgq.as_mut_ptr() as *mut c_void };
        let gsp_ptr = unsafe { (*gsp_mem.start_ptr_mut()).gspq.msgq.as_mut_ptr() as *mut c_void };

        Ok(GspCmdq {
            msg_count,
            seq: 0,
            gsp_mem,
            cpu_ptr,
            gsp_ptr,
            nr_ptes: nr_ptes as u32,
        })
    }

    // We need the next four accessors because the dma_read macro is failable
    // and uses `?` which requires any calling function to return a Result<>.
    // However in the first instance a dma_read failure probably needs to be dealt with
    // by the function trying to do the read, so we need the accessors to permit that.
    //
    // Of course at the moment we "deal" with errors by panicing...
    //
    // I think we need to update the dma macro's to return a Result<u32>
    fn cpu_wptr(self: &Self) -> Result<u32> {
        Ok(dma_read!(self.gsp_mem[0].cpuq.tx.write_ptr))
    }

    fn gsp_rptr(self: &Self) -> Result<u32> {
        Ok(dma_read!(self.gsp_mem[0].gspq.rx.read_ptr))
    }

    fn cpu_rptr(self: &Self) -> Result<u32> {
        Ok(dma_read!(self.gsp_mem[0].cpuq.rx.read_ptr))
    }

    fn gsp_wptr(self: &Self) -> Result<u32> {
        Ok(dma_read!(self.gsp_mem[0].gspq.tx.write_ptr))
    }

    // Returns the numbers of bytes free for sending an RPC to GSP.
    fn get_free_tx_bytes(self: &Self) -> u32 {
        let wptr = self.cpu_wptr().unwrap();
        let rptr = self.gsp_rptr().unwrap();
        let mut free = rptr + self.msg_count - wptr - 1;

        if free >= self.msg_count {
            free -= self.msg_count;
        }

        free << GSP_PAGE_SHIFT
    }

    // Returns the number of bytes the GSP has written to the queue.
    fn get_used_rx_bytes(self: &Self) -> u32 {
        let rptr = self.cpu_rptr().unwrap();
        let wptr = self.gsp_wptr().unwrap();
        let mut used = wptr + self.msg_count - rptr;
        if used >= self.msg_count {
            used -= self.msg_count;
        }

        used << GSP_PAGE_SHIFT
    }

    fn get_free_tx_pages(self: &Self) -> u32 {
        let wptr = self.cpu_wptr().unwrap();
        let rptr = self.gsp_rptr().unwrap();
        let mut free = rptr + self.msg_count - wptr - 1;

        if free >= self.msg_count {
            free -= self.msg_count;
        }

        free
    }

    // Returns the number of pages the GSP has written to the queue.
    fn get_used_rx_pages(self: &Self) -> u32 {
        let rptr = self.cpu_rptr().unwrap();
        let wptr = self.gsp_wptr().unwrap();
        let mut used = wptr + self.msg_count - rptr;
        if used >= self.msg_count {
            used -= self.msg_count;
        }

        used
    }

    fn calculate_checksum(sum: u32, msg_bytes: &[u8]) -> u32 {
        let mut sum64: u64 = sum as u64;
        for &byte in msg_bytes.iter().rev() {
            sum64 = sum64.rotate_left(8) ^ (byte as u64);
        }
        ((sum64 >> 32) as u32) ^ (sum64 as u32)
    }

    fn alloc_cmd<A: GspMessageElement>(
        self: &mut Self,
        msg: &A,
    ) -> Result<(
        &mut [[u8; GSP_PAGE_SIZE]],
        Option<&mut [[u8; GSP_PAGE_SIZE]]>,
    )> {
        let msg_size = msg.size().div_ceil(GSP_PAGE_SIZE) as usize;

        while self.get_free_tx_pages() < msg_size as u32 {}
        let wptr = self.cpu_wptr().unwrap() as usize;
        let mut ptr =
            unsafe { core::ptr::addr_of_mut!((*self.gsp_mem.start_ptr_mut()).cpuq.msgq[wptr]) };

        // Simple case where the queue doesn't wrap
        if wptr + msg_size < 0x3f {
            let slice: &mut [[u8; 4096]] =
                unsafe { core::slice::from_raw_parts_mut(ptr, msg_size) };

            return Ok((slice, None));
        }

        // First slice contains the remaining free pages in the queue
        let slice_1: &mut [[u8; 4096]] =
            unsafe { core::slice::from_raw_parts_mut(ptr, 0x3f - wptr) };
        ptr = unsafe { core::ptr::addr_of_mut!((*self.gsp_mem.start_ptr_mut()).cpuq.msgq[0]) };
        let slice_2: &mut [[u8; 4096]] =
            unsafe { core::slice::from_raw_parts_mut(ptr, msg_size - 0x3f + wptr) };
        return Ok((slice_1, Some(slice_2)));
    }

    fn send<A: GspMessageElement>(
        self: &mut Self,
        bar: &Devres<Bar0>,
        function: u32,
        cmd: &A,
    ) -> Result<fw::GspSystemInfo> {
        let mut msg_header = GspMsgHeader {
            auth_tag_buffer: [0; 16],
            aad_buffer: [0; 16],
            checksum: 0,
            sequence: self.seq,
            elem_count: 1,
            pad: 0,
        };
        let mut rpc = GspRpcHeader {
            header_version: 0x03000000,
            signature: 0x43505256,
            length: 0,
            function,
            rpc_result: 0xffffffff,
            rpc_result_private: 0xffffffff,
            sequence: 0,
            cpu_rm_gfid: 0,
        };

        self.seq += 1;
        rpc.length = (size_of::<GspRpcHeader>() + cmd.size()) as u32;

        let (msg_slice, mut some_msg_slice) = self.alloc_cmd(cmd)?;
        let msg_header_slice = unsafe {
            core::slice::from_raw_parts(
                &msg_header as *const GspMsgHeader as *const u8,
                size_of::<GspMsgHeader>(),
            )
        };
        let rpc_slice = unsafe {
            core::slice::from_raw_parts(
                &rpc as *const GspRpcHeader as *const u8,
                size_of::<GspRpcHeader>(),
            )
        };
        let mut index = 0;
        let mut sub_index = 0;

        msg_slice[0][0..msg_header_slice.len()].copy_from_slice(msg_header_slice);
        sub_index = msg_header_slice.len();
        msg_slice[0][sub_index..sub_index + rpc_slice.len()].copy_from_slice(rpc_slice);
        sub_index += rpc_slice.len();

        let mut msg_slice_len = msg_slice.len();
        if let Some(slice) = &some_msg_slice {
            msg_slice_len += slice.len();
        }
        cmd.copy_to_slice(sub_index, msg_slice, &mut some_msg_slice);
        msg_header.checksum = 0;

        for slice in msg_slice.iter() {
            msg_header.checksum = GspCmdq::calculate_checksum(msg_header.checksum, slice);
        }

        if let Some(some_slice) = some_msg_slice {
            for slice in some_slice.iter() {
                msg_header.checksum = GspCmdq::calculate_checksum(msg_header.checksum, slice);
            }
        }

        // Need to copy it again now that the checksum has been updated
        msg_slice[0][0..msg_header_slice.len()].copy_from_slice(msg_header_slice);

        unsafe {
            print_hex_dump(
                "\0".as_ptr() as *const i8,
                "gsp: \0".as_ptr() as *const i8,
                2,
                16,
                1,
                msg_slice.as_ptr() as *const i8,
                rpc.length as usize + size_of::<GspMsgHeader>(),
                1,
            );
        }

        let mut wptr = self.cpu_wptr().unwrap() as u32;
        wptr += msg_slice_len as u32;
        wptr %= 0x3f;

        // TODO: Figure out Rust barriers
        unsafe {
            asm!("sfence";);
            dma_write!(self.gsp_mem[0].cpuq.tx.write_ptr = wptr);
            asm!("mfence";);
        };

        bar.try_access_with(|b| {
            NV_PGSP_QUEUE_HEAD::default().set_address(0 as u32).write(b);
        });

        Err(EINVAL)
    }

    fn receive_headers(self: &mut Self) -> Result<(GspMsgHeader, GspRpcHeader, *mut c_void, u32)> {
        let size = loop {
            let size = self.get_used_rx_bytes();
            if size as usize >= size_of::<GspMsgHeader>() + size_of::<GspRpcHeader>() {
                break size;
            }
        };

        let mut rptr = self.cpu_rptr()?;
        let msg_ptr = (self.gsp_ptr as usize + (rptr as usize) * 0x1000) as *mut c_void;
        let (rpc_ptr, size, msg) = unsafe { GspMsgHeader::new_from_raw(msg_ptr, size)? };
        let (args_ptr, size, rpc) = unsafe { GspRpcHeader::new_from_raw(rpc_ptr, size)? };

        pr_info!("Got {}/{} bytes\n", size, rpc.length);

        Ok((msg, rpc, args_ptr, size))
    }

    fn create_result<A: GspMessageElement + 'static>(
        ptr: *mut c_void,
        size: u32,
    ) -> Result<KBox<dyn GspMessageElement>> {
        let mut result = KBox::<A>::new_uninit(GFP_KERNEL)?;

        unsafe {
            let (_, _, msg) = A::new_from_raw(ptr, size)?;
            msg.copy_to(result.as_mut_ptr() as *mut c_void);
        };

        Ok(unsafe { result.assume_init() })
    }

    pub(crate) fn receive(self: &mut Self) -> Result<KBox<dyn GspMessageElement>> {
        let (msg, rpc, args_ptr, size) = self.receive_headers()?;
        pr_info!("Got fn 0x{:x}\n", rpc.function);

        let result = match rpc.function {
            fw::NV_VGPU_MSG_EVENT_GSP_RUN_CPU_SEQUENCER => {
                GspCmdq::create_result::<fw::rpc_run_cpu_sequencer_v17_00>(args_ptr, size)
            }
            _ => Err(ENOTSUPP),
        };

        // TODO: Increment by what we actually received
        let mut rptr = self.cpu_rptr()?;
        rptr += 1;

        // TODO: Figure out Rust barriers
        unsafe {
            asm!("mfence";);
            dma_write!(self.gsp_mem[0].cpuq.rx.read_ptr = rptr);
        };

        // TODO: Validate checksum, etc.
        result
    }
}

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
