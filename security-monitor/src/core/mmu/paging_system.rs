// SPDX-FileCopyrightText: 2023 IBM Corporation
// SPDX-FileContributor: Wojciech Ozga <woz@zurich.ibm.com>, IBM Research - Zurich
// SPDX-License-Identifier: Apache-2.0
use crate::core::mmu::PageSize;
use crate::core::transformations::ConfidentialVmVirtualAddress;
use riscv::register::hgatp::HgatpMode;

// TODO: add more 2nd-level paging systems corresponding to 3 and 4 level page
// tables.
#[derive(Debug, Copy, Clone)]
pub enum PagingSystem {
    Sv57x4,
}

impl PagingSystem {
    pub fn from(mode: &HgatpMode) -> Option<Self> {
        match mode {
            HgatpMode::Sv57x4 => Some(PagingSystem::Sv57x4),
        }
    }

    pub fn hgatp_mode(&self) -> HgatpMode {
        match self {
            Self::Sv57x4 => HgatpMode::Sv57x4,
        }
    }

    pub fn levels(&self) -> PageTableLevel {
        match self {
            PagingSystem::Sv57x4 => PageTableLevel::Level5,
        }
    }

    // number of pages required to store the configuration of the page table at the given level
    // in RISC-V all page tables fit at 4KiB pages except for the root page table in 2-level page table system
    pub fn configuration_pages(&self, level: PageTableLevel) -> usize {
        self.size_in_bytes(level) / PageSize::Size4KiB.in_bytes()
    }

    // returns the size of the entry in bytes
    pub fn entry_size(&self) -> usize {
        match self {
            PagingSystem::Sv57x4 => 8,
        }
    }

    pub fn size_in_bytes(&self, level: PageTableLevel) -> usize {
        self.entries(level) * self.entry_size()
    }

    // 2nd level page table's root is extended by 2 bits according to the spec.
    pub fn entries(&self, level: PageTableLevel) -> usize {
        match self {
            PagingSystem::Sv57x4 => match level {
                PageTableLevel::Level5 => 1 << 11,
                _ => 1 << 9,
            },
        }
    }

    pub fn vpn(&self, virtual_address: ConfidentialVmVirtualAddress, level: PageTableLevel) -> usize {
        match self {
            PagingSystem::Sv57x4 => match level {
                PageTableLevel::Level5 => (virtual_address.usize() >> 48) & 0x3ff,
                PageTableLevel::Level4 => (virtual_address.usize() >> 39) & 0x1ff,
                PageTableLevel::Level3 => (virtual_address.usize() >> 30) & 0x1ff,
                PageTableLevel::Level2 => (virtual_address.usize() >> 21) & 0x1ff,
                PageTableLevel::Level1 => (virtual_address.usize() >> 12) & 0x1ff,
            },
        }
    }

    pub fn page_size(&self, level: PageTableLevel) -> PageSize {
        match level {
            PageTableLevel::Level5 => PageSize::Size128TiB,
            PageTableLevel::Level4 => PageSize::Size512GiB,
            PageTableLevel::Level3 => PageSize::Size1GiB,
            PageTableLevel::Level2 => PageSize::Size2MiB,
            PageTableLevel::Level1 => PageSize::Size4KiB,
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
pub enum PageTableLevel {
    Level5,
    Level4,
    Level3,
    Level2,
    Level1,
}

impl PageTableLevel {
    pub fn lower(&self) -> Option<Self> {
        match self {
            Self::Level5 => Some(Self::Level4),
            Self::Level4 => Some(Self::Level3),
            Self::Level3 => Some(Self::Level2),
            Self::Level2 => Some(Self::Level1),
            Self::Level1 => None,
        }
    }
}
