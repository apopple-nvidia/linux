// SPDX-License-Identifier: GPL-2.0
//
// RM Control implementation for nova-core
//
// This module provides the interface for sending RM (Resource Manager) control
// commands to the GSP firmware. RM control commands are used to query and
// configure various GPU resources.
//
// TODO: To properly handle responses, the following needs to be implemented:
// 1. Add RM control response handling to gsp.rs receive() method for function 76
//    (NV_VGPU_MSG_FUNCTION_GSP_RM_CONTROL)
// 2. Parse the RM control response which includes the RmControlRpc header
//    followed by the response data
// 3. Check the status field in the response header for errors
// 4. Return the parsed response data to the caller

use crate::gsp::{GspCmdq, GspInfo, GspMessageElement};
use crate::nvfw::r570_144 as fw;
use kernel::prelude::*;
use kernel::{pr_info, pr_err};

// Command constants
pub mod commands {
    pub const NV2080_CTRL_CMD_INTERNAL_INTR_GET_KERNEL_TABLE: u32 = 0x20800a5c;
    pub const NV2080_CTRL_CMD_FB_GET_INFO: u32 = 0x20801301;
    // Add more as needed
}

// Error types
#[derive(Debug)]
pub enum RmControlError {
    InvalidResponse,
    RmError(u32), // Raw RM status code
    NotEnoughData,
    GspError(kernel::error::Error),
}

impl From<RmControlError> for kernel::error::Error {
    fn from(e: RmControlError) -> Self {
        match e {
            RmControlError::InvalidResponse => EINVAL,
            RmControlError::RmError(_) => EIO,
            RmControlError::NotEnoughData => EINVAL,
            RmControlError::GspError(e) => e,
        }
    }
}

// RM Control RPC wrapper structure
#[repr(C)]
#[derive(Debug)]
struct RmControlRpc {
    h_client: u32,
    h_object: u32,
    cmd: u32,
    status: u32,
    params_size: u32,
    flags: u32,
    // params follow as bytes
}

// Wrapper to send RPC header + params together
struct RmControlMessage<'a> {
    rpc: RmControlRpc,
    params: Option<&'a [u8]>,
}

impl<'a> GspMessageElement for RmControlMessage<'a> {
    fn copy_to_slices(&self, msg_slice_1: &mut [u8], msg_slice_2: &mut Option<&mut [u8]>) {
        // First copy the RPC header
        let header_bytes = unsafe {
            core::slice::from_raw_parts(&self.rpc as *const RmControlRpc as *const u8, size_of::<RmControlRpc>())
        };
        
        let mut offset = 0;
        let header_len = header_bytes.len();
        let copy_len = core::cmp::min(msg_slice_1.len(), header_len);
        msg_slice_1[0..copy_len].copy_from_slice(&header_bytes[0..copy_len]);
        offset += copy_len;
        
        // Copy remaining header to slice_2 if needed
        let mut remaining_header = 0;
        if copy_len < header_len {
            if let Some(slice_2) = msg_slice_2 {
                remaining_header = header_len - copy_len;
                slice_2[0..remaining_header].copy_from_slice(&header_bytes[copy_len..]);
                offset = remaining_header;
            }
        }
        
        // Now copy params if any
        if let Some(params) = self.params {
            if offset < msg_slice_1.len() {
                // Still space in slice_1
                let remaining_1 = msg_slice_1.len() - offset;
                let params_copy_1 = core::cmp::min(remaining_1, params.len());
                msg_slice_1[offset..offset + params_copy_1].copy_from_slice(&params[0..params_copy_1]);
                
                // Copy rest to slice_2 if needed
                if params_copy_1 < params.len() {
                    if let Some(slice_2) = msg_slice_2 {
                        let start = if copy_len < header_len { remaining_header } else { 0 };
                        slice_2[start..start + params.len() - params_copy_1]
                            .copy_from_slice(&params[params_copy_1..]);
                    }
                }
            } else if let Some(slice_2) = msg_slice_2 {
                // All in slice_2
                slice_2[offset..offset + params.len()].copy_from_slice(params);
            }
        }
    }
    
    fn size(&self) -> usize {
        size_of::<RmControlRpc>() + self.rpc.params_size as usize
    }
}

// Trait for all RM control responses
pub trait RmControlResponse: Sized {
    /// Parse response from bytes
    fn from_bytes(data: &[u8]) -> Result<Self>;
    
    /// Get the expected size (if known at compile time)
    fn expected_size() -> Option<usize> {
        None
    }
}

// Trait for all RM control parameters
pub trait RmControlParams {
    fn to_bytes(&self) -> &[u8];
}

// Main RM Control structure
pub struct RmControl<'a> {
    cmdq: &'a mut GspCmdq<'a>,
    gsp_info: &'a GspInfo,
}

impl<'a> RmControl<'a> {
    /// Create new RM control instance
    pub fn new(cmdq: &'a mut GspCmdq<'a>, gsp_info: &'a GspInfo) -> Self {
        Self { cmdq, gsp_info }
    }

    /// Send an RM control command and get typed response
    pub fn send<P: RmControlParams, T: RmControlResponse>(
        &mut self,
        cmd: u32,
        params: Option<&P>,
    ) -> Result<T> {
        let params_size = params.map_or(0, |p| p.to_bytes().len());

        // Create the RPC header
        let rpc = RmControlRpc {
            h_client: self.gsp_info.h_internal_client,
            h_object: self.gsp_info.h_internal_subdevice,
            cmd,
            status: 0,
            params_size: params_size as u32,
            flags: 0,
        };
        
        // Create message wrapper
        let msg = RmControlMessage { rpc, params: params.map(|p| p.to_bytes()) };
        
        // Send the command
        self.cmdq.send(fw::NV_VGPU_MSG_FUNCTION_GSP_RM_CONTROL, &msg)?;
        
        pr_info!("RM Control: Sent command {:#x} with {} bytes params\n", cmd, params_size);
        
        // Receive the response
        let (status, data) = self.cmdq.get_rm_control(kernel::time::Delta::from_secs(5))?;
        
        // Check for RM errors
        if status != 0 {
            pr_err!("RM Control: Command {:#x} failed with status {:#x}\n", cmd, status);
            return Err(RmControlError::RmError(status).into());
        }
        
        // Parse the response data
        T::from_bytes(&data)
    }
}

// Generic empty response for commands that just return status
#[derive(Debug)]
pub struct RmEmptyResponse;

impl RmControlResponse for RmEmptyResponse {
    fn from_bytes(_data: &[u8]) -> Result<Self> {
        Ok(Self)
    }
    
    fn expected_size() -> Option<usize> {
        Some(0)
    }
}