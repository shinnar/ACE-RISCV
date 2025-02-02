// SPDX-FileCopyrightText: 2023 IBM Corporation
// SPDX-FileContributor: Wojciech Ozga <woz@zurich.ibm.com>, IBM Research - Zurich
// SPDX-License-Identifier: Apache-2.0
#![no_std]
#![no_main]
#![feature(panic_info_message)]
#![feature(asm_const)]
#![feature(asm_experimental_arch)]

use core::arch::{global_asm};
use buddy_system_allocator::LockedHeap;
use core::sync::atomic::AtomicUsize;
use uart::Uart;
use alloc::format;
use crate::error::Error;
extern crate alloc;

mod uart;
#[macro_use]
mod macros;
mod trap;
mod virtio;
mod hal;
mod calls;
mod error;

global_asm!(include_str!("asm/boot.S"));
global_asm!(include_str!("asm/trap.S"));

#[no_mangle]
extern "C" fn eh_personality() {}

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap<32> = LockedHeap::<32>::empty();
static mut DMA_PADDR: Option<AtomicUsize> = None;
static mut SCRATCH_PAGE: Option<crate::hal::ScratchPage> = None;
const UART_BASE_ADDRESS: usize = 0x1000_0000;
pub static mut TRAP_FRAME: [[trap::TrapFrame; 8]; 4] = [[trap::TrapFrame::zero(); 8]; 4];

extern "C" {
    fn _stack_start();
    fn _stack_end();
    fn _memory_start();
    fn _memory_end();
    fn _dma_start();
    fn _dma_end();
    fn _heap_start();
    fn _heap_size();
}

#[no_mangle]
extern "C" fn init(_hart_id: usize, fdt_paddr: usize) {
    let mut uart = Uart::new(UART_BASE_ADDRESS);
    init_memory(&mut uart);    
    init_trap();

    crate::calls::sm::esm().expect("ESM failed");

    uart.println("Hello IBM from confidential VM!");

    test_exception_delegation(&mut uart);

    match test_base_sbi(&mut uart) {
        Ok(_) => uart.println("SBI base test: success"),
        Err(error) => {
            uart.println(&format!("Error: {:?}", error));
            uart.println("SBI base test: failed");
        },
    };

    match test_virtio(fdt_paddr) {
        Ok(_) => uart.println("Virtio blk test: success"),
        Err(error) => {
            uart.println(&format!("Error: {:?}", error));
            uart.println("Virtio blk test: failed");
        },
    };

    sbi::system_reset::system_reset(sbi::system_reset::ResetType::Shutdown, sbi::system_reset::ResetReason::NoReason).expect("system reset failed");
}

fn test_exception_delegation(uart: &mut Uart) {
    uart.println("Exception delegation test");
    unsafe {
        core::arch::asm!("j 2");
        core::arch::asm!("addi x0, x0, 2");
    }
    uart.println("Success");
}

fn test_base_sbi(uart: &mut Uart) -> Result<(), usize> {
    let spec_version = sbi::base::spec_version();
    uart.println(&format!("Spec version: {:?}", spec_version));

    let imp_id = sbi::base::impl_id();
    uart.println(&format!("Implementation id: {:?}", imp_id));
    
    let imp_version = sbi::base::impl_version();
    uart.println(&format!("Implementation version: {:x}", imp_version));
    
    let mvendorid = sbi::base::mvendorid();
    uart.println(&format!("mvendorid: {:x}", mvendorid));

    let marchid = sbi::base::marchid();
    uart.println(&format!("marchid: {:x}", marchid));

    let mimpid = sbi::base::mimpid();
    uart.println(&format!("mimpid: {:x}", mimpid));

    let availability = sbi::base::probe_extension(crate::calls::ace::KVM_ACE_EXTID);
    uart.println(&format!("KVM's ACE extension: {:?}", availability));    

    Ok(())
}

fn test_virtio(fdt_paddr: usize) -> Result<(), Error> {
    let mut blk = virtio::get_block_device(fdt_paddr).expect("failed geting blk device");

    let (input_paddr, output_paddr) = prepare_shared_memory()?;
    let input: &mut [u8] = unsafe { core::slice::from_raw_parts_mut(input_paddr as *mut u8, 512) };
    let mut output: &mut [u8] = unsafe { core::slice::from_raw_parts_mut(output_paddr as *mut u8, 512) };
    for x in input.iter_mut() {
        *x = 'I' as u8;
    }
    for x in output.iter_mut() {
        *x = '0' as u8;
    }
    
    blk.write_block(0, &input).expect("failed to write");
    blk.read_block(0, &mut output).expect("failed to read");
    let mut result = 1;
    for i in 0..input.len() {
        if input[i] != output[i] {
            result = 0;
        }
    }
    match result {
        1 => Ok(()),
        _ => Err(Error::VirtioError()),
    }
}

fn init_memory(uart: &mut Uart) {
    unsafe {
        HEAP_ALLOCATOR.lock().init(_heap_start as usize, _heap_size as usize);
        uart.println(&format!("Stack 0x{:x}-0x{:x}", _stack_start as usize, _stack_end as usize));
        uart.println(&format!("DMA   0x{:x}-0x{:x}", _dma_start as usize, _dma_end as usize));
        uart.println(&format!("Heap  0x{:x}-0x{:x}", _heap_start as usize, _heap_start as usize + _heap_size as usize));
        let dma_start = (_dma_start as usize + 4096 - 1) & !(4096 - 1);
        crate::DMA_PADDR = Some(AtomicUsize::new(dma_start));
    }
    crate::calls::ace::load_all_pages().expect("Load all pages call failed");
}

fn init_trap() {
    let hart_id: usize = 0;
    unsafe {
        let ptr: usize = (&mut crate::TRAP_FRAME[hart_id][0] as *mut trap::TrapFrame) as usize;
		core::arch::asm!("csrw sscratch, {0}", in(reg) ptr);
	}
}

fn prepare_shared_memory() -> Result<(usize, usize), Error> {
    let pages_to_allocate = 3;
    let paddr = unsafe {
        if let Some(v) = &crate::DMA_PADDR {
            v.fetch_add(virtio_drivers::PAGE_SIZE * pages_to_allocate, core::sync::atomic::Ordering::SeqCst)
        } else {
            return Err(Error::DmaNotInitialized());
        }
    };
    for i in 0..pages_to_allocate {
        crate::calls::sm::share_page(paddr + i*4096, 1)?;
    }
    let input_paddr = paddr;
    let output_paddr = paddr + 4096;
    let scratch_paddr = paddr + 2*4096;

    unsafe {
        crate::SCRATCH_PAGE = Some(crate::hal::ScratchPage{base_paddr: scratch_paddr, position: 0, translations: alloc::vec![]});
    }

    Ok((input_paddr, output_paddr))
}

