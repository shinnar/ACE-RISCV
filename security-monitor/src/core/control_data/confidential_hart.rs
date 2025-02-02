// SPDX-FileCopyrightText: 2023 IBM Corporation
// SPDX-FileContributor: Wojciech Ozga <woz@zurich.ibm.com>, IBM Research - Zurich
// SPDX-License-Identifier: Apache-2.0
use crate::core::control_data::ConfidentialVmId;
use crate::core::hart::{FpRegisters, GpRegister, GpRegisters, HartState};
use crate::core::transformations::{
    ExposeToConfidentialVm, GuestLoadPageFaultRequest, GuestLoadPageFaultResult, GuestStorePageFaultRequest,
    GuestStorePageFaultResult, MmioLoadRequest, MmioStoreRequest, PendingRequest, SbiRequest, SbiResult,
    SharePageRequest, TrapReason,
};
use crate::error::Error;

/// ConfidentialHart represents the dump state of the confidential VM's hart (aka
/// vcpu). The only publicly exposed way to modify the virtual hart state
/// (registers/CSRs) is by calling the constructor or applying a transformation.
#[repr(C)]
pub struct ConfidentialHart {
    // Safety: Careful, HardwareHart and ConfidentialHart must both start with the HartState element because based on
    // this we automatically calculate offsets of registers' and CSRs' for the asm code.
    confidential_hart_state: HartState,
    pending_request: Option<PendingRequest>,
    // a dummy virtual hart means that the confidential_hart is not associated with any confidential VM
    dummy: bool,
}

impl ConfidentialHart {
    pub fn dummy(id: usize) -> Self {
        let confidential_hart_state = HartState::empty(id);
        Self { confidential_hart_state, pending_request: None, dummy: true }
    }

    pub fn from_vm_hart_reset(id: usize, from: &HartState) -> Self {
        let mut confidential_hart_state = HartState::from_existing(id, from);

        // delegate VS-level interrupts directly to the confidential VM. All other
        // interrupts will trap in the security monitor.
        confidential_hart_state.mideleg = 0b010001000100;
        confidential_hart_state.hideleg = confidential_hart_state.mideleg;

        // delegate exceptions that can be handled directly in the confidential VM
        confidential_hart_state.medeleg = 0b1011001111111111;
        confidential_hart_state.hedeleg = confidential_hart_state.medeleg;

        Self { confidential_hart_state, pending_request: None, dummy: false }
    }

    pub fn from_vm_hart(id: usize, from: &HartState) -> Self {
        let mut confidential_hart = Self::from_vm_hart_reset(id, from);
        GpRegisters::iter().for_each(|x| {
            confidential_hart.confidential_hart_state.gprs.0[x] = from.gprs.0[x];
        });
        FpRegisters::iter().for_each(|x| {
            confidential_hart.confidential_hart_state.fprs.0[x] = from.fprs.0[x];
        });

        // We create a virtual hart as a result of the SBI request
        // the ESM call traps in the security monitor, which creates
        // the confidential VM but then the security monitor makes an SBI call to
        // the hypervisor to let him know that this VM become an confidential VM.
        // The hypervisor should then return to the confidential VM providing it
        // with the result of this transformation.
        confidential_hart.pending_request = Some(PendingRequest::SbiRequest());
        confidential_hart
    }

    pub fn confidential_vm_id(&self) -> ConfidentialVmId {
        ConfidentialVmId::new(riscv::register::hgatp::Hgatp::from(self.confidential_hart_state.hgatp).vmid())
    }

    pub(super) fn confidential_hart_id(&self) -> usize {
        self.confidential_hart_state.id
    }

    pub fn take_request(&mut self) -> Option<PendingRequest> {
        self.pending_request.take()
    }

    pub fn is_dummy(&self) -> bool {
        self.dummy
    }

    pub fn set_pending_request(&mut self, request: PendingRequest) -> Result<(), Error> {
        assure!(self.pending_request.is_none(), Error::PendingRequest())?;
        self.pending_request = Some(request);
        Ok(())
    }
}

// functions to inject information to a confidential VM.
impl ConfidentialHart {
    pub fn set_hgatp(&mut self, hgatp: usize) {
        self.confidential_hart_state.hgatp = hgatp;
    }

    pub fn apply(&mut self, transformation: ExposeToConfidentialVm) -> usize {
        match transformation {
            ExposeToConfidentialVm::SbiResult(v) => self.apply_sbi_result(v),
            ExposeToConfidentialVm::GuestLoadPageFaultResult(v) => self.apply_guest_load_page_fault_result(v),
            ExposeToConfidentialVm::GuestStorePageFaultResult(v) => self.apply_guest_store_page_fault_result(v),
            ExposeToConfidentialVm::Resume() => {}
        }
        core::ptr::addr_of!(self.confidential_hart_state) as usize
    }

    fn apply_sbi_result(&mut self, result: SbiResult) {
        self.confidential_hart_state.set_gpr(GpRegister::a0, result.a0());
        self.confidential_hart_state.set_gpr(GpRegister::a1, result.a1());
        self.confidential_hart_state.mepc += result.pc_offset();
    }

    fn apply_guest_load_page_fault_result(&mut self, result: GuestLoadPageFaultResult) {
        self.confidential_hart_state.set_gpr(result.result_gpr(), result.value());
        self.confidential_hart_state.mepc += result.instruction_length();
    }

    fn apply_guest_store_page_fault_result(&mut self, result: GuestStorePageFaultResult) {
        self.confidential_hart_state.mepc += result.instruction_length();
    }
}

// functions to expose portions of confidential virtual hart state
impl ConfidentialHart {
    pub fn trap_reason(&self) -> TrapReason {
        self.confidential_hart_state.trap_reason()
    }

    pub fn hypercall_request(&self) -> SbiRequest {
        SbiRequest::from_hart_state(&self.confidential_hart_state)
    }

    pub fn guest_load_page_fault_request(&self) -> Result<(GuestLoadPageFaultRequest, MmioLoadRequest), Error> {
        let mcause = riscv::register::mcause::read().code();
        let (instruction, instruction_length) = self.read_instruction();
        let gpr = read_result_gpr(instruction)?;
        let mtval = self.confidential_hart_state.mtval;
        let mtval2 = self.confidential_hart_state.mtval2;

        let load_fault_request = GuestLoadPageFaultRequest::new(instruction_length, gpr);
        let mmio_load_request = MmioLoadRequest::new(mcause, mtval, mtval2, instruction);

        Ok((load_fault_request, mmio_load_request))
    }

    pub fn guest_store_page_fault_request(&self) -> Result<(GuestStorePageFaultRequest, MmioStoreRequest), Error> {
        let mcause = riscv::register::mcause::read().code();
        let (instruction, instruction_length) = self.read_instruction();
        let gpr = read_result_gpr(instruction)?;
        let gpr_value = self.confidential_hart_state.gpr(gpr);
        let mtval = self.confidential_hart_state.mtval;
        let mtval2 = self.confidential_hart_state.mtval2;

        let guest_store_page_fault_request = GuestStorePageFaultRequest::new(instruction_length);
        let mmio_store_request = MmioStoreRequest::new(mcause, mtval, mtval2, instruction, gpr, gpr_value);

        Ok((guest_store_page_fault_request, mmio_store_request))
    }

    pub fn share_page_request(&self) -> Result<(SharePageRequest, SbiRequest), Error> {
        let shared_page_address = self.confidential_hart_state.gpr(GpRegister::a0);
        let share_page_request = SharePageRequest::new(shared_page_address)?;
        let sbi_request = SbiRequest::kvm_ace_page_in(shared_page_address);

        Ok((share_page_request, sbi_request))
    }

    fn read_instruction(&self) -> (usize, usize) {
        // mepc stores the virtual address of the instruction that caused trap. Setting
        // mstatus.MPRV bit allows reading the faulting instruction in memory using the
        // virtual address.
        let fault_instruction_virtual_address = self.confidential_hart_state.mepc as *const usize;
        let instruction = unsafe {
            riscv::register::mstatus::set_mprv();
            let instruction = fault_instruction_virtual_address.read_volatile();
            riscv::register::mstatus::clear_mprv();
            instruction
        };

        // We only expose the faulting instruction, which can be of different length.
        // Therefore, we must trim the read memory to this size by disabling unwanted
        // bits after learning what is the size of the fault instruction.
        let instruction_length = riscv_decode::instruction_length(instruction as u16);
        let mask = (1 << 8 * instruction_length) - 1;
        let instruction = (instruction & mask) as usize;

        (instruction, instruction_length)
    }
}

// TODO: remove below once riscv_decode supports compressed instructions
fn read_result_gpr(mtinst: usize) -> Result<GpRegister, Error> {
    use riscv_decode::Instruction::{Lb, Lbu, Ld, Lh, Lhu, Lw, Lwu, Sb, Sd, Sh, Sw};
    let register_index = match riscv_decode::decode(mtinst as u32) {
        Ok(Sb(i)) => Ok(i.rs2()),
        Ok(Sh(i)) => Ok(i.rs2()),
        Ok(Sw(i)) => Ok(i.rs2()),
        Ok(Sd(i)) => Ok(i.rs2()),
        Ok(Lb(i)) => Ok(i.rd()),
        Ok(Lbu(i)) => Ok(i.rd()),
        Ok(Lhu(i)) => Ok(i.rd()),
        Ok(Lwu(i)) => Ok(i.rd()),
        Ok(Lh(i)) => Ok(i.rd()),
        Ok(Lw(i)) => Ok(i.rd()),
        Ok(Ld(i)) => Ok(i.rd()),
        _ => {
            // TODO: do not try to understand what is going on below. Remove all this
            // section once compressed instructions are supported in the
            // rust-decode crate!
            const SH_RS2C: usize = 2;
            const INSN_MATCH_C_LD: usize = 0x6000;
            const INSN_MASK_C_LD: usize = 0xe003;
            const INSN_MATCH_C_SD: usize = 0xe000;
            const INSN_MASK_C_SD: usize = 0xe003;
            const INSN_MATCH_C_LW: usize = 0x4000;
            const INSN_MASK_C_LW: usize = 0xe003;
            const INSN_MATCH_C_SW: usize = 0xc000;
            const INSN_MASK_C_SW: usize = 0xe003;
            const INSN_MATCH_C_LDSP: usize = 0x6002;
            const INSN_MASK_C_LDSP: usize = 0xe003;
            const INSN_MATCH_C_SDSP: usize = 0xe002;
            const INSN_MASK_C_SDSP: usize = 0xe003;
            const INSN_MATCH_C_LWSP: usize = 0x4002;
            const INSN_MASK_C_LWSP: usize = 0xe003;
            const INSN_MATCH_C_SWSP: usize = 0xc002;
            const INSN_MASK_C_SWSP: usize = 0xe003;

            let log_regbytes = 3; // for 64b!
            let shift_right = |x: usize, y: isize| {
                if y < 0 {
                    x << -y
                } else {
                    x >> y
                }
            };
            let reg_mask = (1 << (5 + log_regbytes)) - (1 << log_regbytes);
            let rv_x = |x: usize, s: usize, n: usize| (((x) >> (s)) & ((1 << (n)) - 1));

            if mtinst & INSN_MASK_C_LW == INSN_MATCH_C_LW {
                let index = 8 + rv_x(mtinst, SH_RS2C, 3);
                Ok(index as u32)
            } else if mtinst & INSN_MASK_C_LD == INSN_MATCH_C_LD {
                let index = 8 + rv_x(mtinst, SH_RS2C, 3);
                Ok(index as u32)
            } else if mtinst & INSN_MASK_C_SW == INSN_MATCH_C_SW {
                let tmp_inst = 8 + rv_x(mtinst, SH_RS2C, 3);
                let index = shift_right(tmp_inst, 0isize - log_regbytes as isize) & reg_mask;
                let index = index / 8;
                Ok(index as u32)
            } else if mtinst & INSN_MASK_C_SD == INSN_MATCH_C_SD {
                let tmp_inst = 8 + rv_x(mtinst, SH_RS2C, 3);
                let index = shift_right(tmp_inst, 0isize - log_regbytes as isize) & reg_mask;
                let index = index / 8;
                Ok(index as u32)
            } else if mtinst & INSN_MASK_C_LWSP == INSN_MATCH_C_LWSP {
                Ok(0u32)
            } else if mtinst & INSN_MASK_C_SWSP == INSN_MATCH_C_SWSP {
                let index = shift_right(mtinst, SH_RS2C as isize - log_regbytes as isize) & reg_mask;
                let index = index / 8;
                Ok(index as u32)
            } else if mtinst & INSN_MASK_C_LDSP == INSN_MATCH_C_LDSP {
                Ok(0u32)
            } else if mtinst & INSN_MASK_C_SDSP == INSN_MATCH_C_SDSP {
                let index = shift_right(mtinst, SH_RS2C as isize - log_regbytes as isize) & reg_mask;
                let index = index / 8;
                Ok(index as u32)
            } else {
                Err(Error::InvalidRiscvInstruction(mtinst))
            }
        }
    }?;
    Ok(GpRegister::from_index(register_index as usize).ok_or(Error::InvalidRiscvInstruction(mtinst))?)
}
