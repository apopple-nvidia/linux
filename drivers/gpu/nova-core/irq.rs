// SPDX-License-Identifier: GPL-2.0

use crate::gpu::Gpu;
use crate::gsp::rm_control::{RmControl, RmControlResponse, RmControlParams, commands};
use crate::gsp::{GspSharedMemObjects, GspInfo};
use kernel::prelude::*;
use kernel::alloc::KVec;
use kernel::{pr_info, pr_err};

// Constants from Nouveau
const NV2080_CTRL_INTERNAL_INTR_MAX_TABLE_SIZE: usize = 128;
const NV2080_INTR_CATEGORY_ENUM_COUNT: usize = 7;

// Category subtree map structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SubtreeMap {
    pub subtree_start: u8,
    pub subtree_end: u8,
}

// Interrupt table entry - matches NV2080_CTRL_INTERNAL_INTR_GET_KERNEL_TABLE_ENTRY
// Note: Nouveau uses u16 for engine_idx
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IrqTableEntry {
    pub engine_idx: u16,
    pub pmc_intr_mask: u32,
    pub vector_stall: u32,
    pub vector_nonstall: u32,
}

// Parameters structure for NV2080_CTRL_CMD_INTERNAL_INTR_GET_KERNEL_TABLE
// matches NV2080_CTRL_INTERNAL_INTR_GET_KERNEL_TABLE_PARAMS
#[repr(C)]
pub struct IrqTableParams {
    pub table_len: u32,
    pub table: [IrqTableEntry; NV2080_CTRL_INTERNAL_INTR_MAX_TABLE_SIZE],
    pub subtree_map: [SubtreeMap; NV2080_INTR_CATEGORY_ENUM_COUNT],
}

impl_from_bytes!(IrqTableParams);

impl RmControlParams for IrqTableParams {
    fn to_bytes(&self) -> &[u8] {
        // SAFETY: IrqTableParams is a fixed size struct who's size is known.
        unsafe {
            core::slice::from_raw_parts(
                self as *const IrqTableParams as *const u8,
                size_of::<IrqTableParams>()
            )
        }
    }
}

// parsed representation of interrupt table
#[repr(C)]
#[derive(Debug)]
pub struct IrqTable {
    pub table_len: u32,
    pub entries: KVec<IrqTableEntry>,
}

impl RmControlResponse for IrqTable {
    fn from_bytes(data: &[u8]) -> Result<Self> {
        let params = IrqTableParams::from_bytes(data)?;
        
        // Create entries vector from the valid portion of the table
        let mut entries = KVec::new();
        let table_len = params.table_len as usize;
        
        if table_len > NV2080_CTRL_INTERNAL_INTR_MAX_TABLE_SIZE {
            pr_err!("Invalid interrupt table length: {}\n", table_len);
            return Err(EINVAL);
        }
        
        for i in 0..table_len {
            entries.push(params.table[i], GFP_KERNEL)?;
        }
        
        Ok(Self { 
            table_len: params.table_len, 
            entries 
        })
    }
}


pub fn dump_table<'a>(libos: &'a mut GspSharedMemObjects<'a>, gsp_info: &'a GspInfo) -> Result {
    let mut rm_control = RmControl::new(&mut libos.cmdq, gsp_info);
    
    let params = IrqTableParams {
        table_len: 0,
        table: [IrqTableEntry {
            engine_idx: 0,
            pmc_intr_mask: 0,
            vector_stall: 0,
            vector_nonstall: 0,
        }; NV2080_CTRL_INTERNAL_INTR_MAX_TABLE_SIZE],
        subtree_map: [SubtreeMap {
            subtree_start: 0,
            subtree_end: 0,
        }; NV2080_INTR_CATEGORY_ENUM_COUNT],
    };
    
    let table: IrqTable = rm_control.send(
        commands::NV2080_CTRL_CMD_INTERNAL_INTR_GET_KERNEL_TABLE,
        Some(&params),
    )?;
    
    pr_info!("Interrupt table: {} entries\n", table.table_len);
    for (i, entry) in table.entries.iter().enumerate() {
        pr_info!(
            "  [{}]: engine_idx={} pmc_mask={:#x} stall={:#x} nonstall={:#x}\n",
            i, entry.engine_idx, entry.pmc_intr_mask, 
            entry.vector_stall, entry.vector_nonstall
        );
    }
    
    Ok(())
}