use super::{
    frame_allocator,
    memory_set::MapArea,
    {Paddr, Vaddr},
};
use crate::{
    config::{ENTRY_COUNT, PAGE_SIZE, PHYS_OFFSET},
    vm::VmFrame,
};
use align_ext::AlignExt;
use alloc::{collections::BTreeMap, vec, vec::Vec};
use core::{fmt, panic};
use lazy_static::lazy_static;
use spin::Mutex;

lazy_static! {
    pub(crate) static ref ALL_MAPPED_PTE: Mutex<BTreeMap<usize, PageTableEntry>> =
        Mutex::new(BTreeMap::new());
}

bitflags::bitflags! {
  /// Possible flags for a page table entry.
  pub struct PageTableFlags: usize {
    /// Specifies whether the mapped frame or page table is loaded in memory.
    const PRESENT =         1 << 0;
    /// Controls whether writes to the mapped frames are allowed.
    const WRITABLE =        1 << 1;
    /// Controls whether accesses from userspace (i.e. ring 3) are permitted.
    const USER =            1 << 2;
    /// If this bit is set, a “write-through” policy is used for the cache, else a “write-back”
    /// policy is used.
    const WRITE_THROUGH =   1 << 3;
    /// Disables caching for the pointed entry is cacheable.
    const NO_CACHE =        1 << 4;
    /// Indicates that the mapping is present in all address spaces, so it isn't flushed from
    /// the TLB on an address space switch.
    const GLOBAL =          1 << 8;
    /// Forbid execute codes on the page. The NXE bits in EFER msr must be set.
    const NO_EXECUTE =      1 << 63;
  }
}

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(usize);

impl PageTableEntry {
    const PHYS_ADDR_MASK: usize = !(PAGE_SIZE - 1);

    pub const fn new_page(pa: Paddr, flags: PageTableFlags) -> Self {
        Self((pa & Self::PHYS_ADDR_MASK) | flags.bits)
    }
    const fn paddr(self) -> Paddr {
        self.0 as usize & Self::PHYS_ADDR_MASK
    }
    const fn flags(self) -> PageTableFlags {
        PageTableFlags::from_bits_truncate(self.0)
    }
    const fn is_unused(self) -> bool {
        self.0 == 0
    }
    const fn is_present(self) -> bool {
        (self.0 & PageTableFlags::PRESENT.bits) != 0
    }
}

impl fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut f = f.debug_struct("PageTableEntry");
        f.field("raw", &self.0)
            .field("paddr", &self.paddr())
            .field("flags", &self.flags())
            .finish()
    }
}

pub struct PageTable {
    pub root_pa: Paddr,
    /// store all the physical frame that the page table need to map all the frame e.g. the frame of the root_pa
    tables: Vec<VmFrame>,
}

impl PageTable {
    pub fn new() -> Self {
        let root_frame = frame_allocator::alloc_zero().unwrap();
        let p4 = table_of(root_frame.start_paddr()).unwrap();
        let map_pte = ALL_MAPPED_PTE.lock();
        for (index, pte) in map_pte.iter() {
            p4[*index] = *pte;
        }
        Self {
            root_pa: root_frame.start_paddr(),
            tables: vec![root_frame],
        }
    }

    pub fn map(&mut self, va: Vaddr, pa: Paddr, flags: PageTableFlags) {
        let entry = self.get_entry_or_create(va).unwrap();
        if !entry.is_unused() {
            panic!("{:#x?} is mapped before mapping", va);
        }
        *entry = PageTableEntry::new_page(pa.align_down(PAGE_SIZE), flags);
    }

    pub fn unmap(&mut self, va: Vaddr) {
        let entry = get_entry(self.root_pa, va).unwrap();
        if entry.is_unused() {
            panic!("{:#x?} is invalid before unmapping", va);
        }
        entry.0 = 0;
    }

    pub fn protect(&mut self, va: Vaddr, flags: PageTableFlags) {
        let entry = self.get_entry_or_create(va).unwrap();
        if entry.is_unused() || !entry.is_present() {
            panic!("{:#x?} is invalid before protect", va);
        }
        // clear old mask
        let clear_flags_mask = !PageTableFlags::all().bits;
        entry.0 &= clear_flags_mask;
        // set new mask
        entry.0 |= flags.bits;
    }

    pub fn map_area(&mut self, area: &MapArea) {
        for (va, pa) in area.mapper.iter() {
            assert!(pa.start_paddr() < PHYS_OFFSET);
            self.map(*va, pa.start_paddr(), area.flags);
        }
    }

    pub fn unmap_area(&mut self, area: &MapArea) {
        for (va, _) in area.mapper.iter() {
            self.unmap(*va);
        }
    }
}

impl PageTable {
    fn alloc_table(&mut self) -> Paddr {
        let frame = frame_allocator::alloc_zero().unwrap();
        let pa = frame.start_paddr();
        self.tables.push(frame);
        pa
    }

    fn get_entry_or_create(&mut self, va: Vaddr) -> Option<&mut PageTableEntry> {
        let p4 = table_of(self.root_pa).unwrap();
        let p4e = &mut p4[p4_index(va)];
        let p3 = next_table_or_create(p4e, || self.alloc_table())?;
        let p3e = &mut p3[p3_index(va)];
        let p2 = next_table_or_create(p3e, || self.alloc_table())?;
        let p2e = &mut p2[p2_index(va)];
        let p1 = next_table_or_create(p2e, || self.alloc_table())?;
        let p1e = &mut p1[p1_index(va)];
        Some(p1e)
    }
}

const fn p4_index(va: Vaddr) -> usize {
    (va >> (12 + 27)) & (ENTRY_COUNT - 1)
}

const fn p3_index(va: Vaddr) -> usize {
    (va >> (12 + 18)) & (ENTRY_COUNT - 1)
}

const fn p2_index(va: Vaddr) -> usize {
    (va >> (12 + 9)) & (ENTRY_COUNT - 1)
}

const fn p1_index(va: Vaddr) -> usize {
    (va >> 12) & (ENTRY_COUNT - 1)
}

fn get_entry(root_pa: Paddr, va: Vaddr) -> Option<&'static mut PageTableEntry> {
    let p4 = table_of(root_pa).unwrap();
    let p4e = &mut p4[p4_index(va)];
    let p3 = next_table(p4e)?;
    let p3e = &mut p3[p3_index(va)];
    let p2 = next_table(p3e)?;
    let p2e = &mut p2[p2_index(va)];
    let p1 = next_table(p2e)?;
    let p1e = &mut p1[p1_index(va)];
    Some(p1e)
}

fn table_of<'a>(pa: Paddr) -> Option<&'a mut [PageTableEntry]> {
    if pa == 0 {
        return None;
    }
    let ptr = super::paddr_to_vaddr(pa) as *mut _;
    unsafe { Some(core::slice::from_raw_parts_mut(ptr, ENTRY_COUNT)) }
}

fn next_table<'a>(entry: &PageTableEntry) -> Option<&'a mut [PageTableEntry]> {
    if entry.is_present() {
        Some(table_of(entry.paddr()).unwrap())
    } else {
        None
    }
}

fn next_table_or_create<'a>(
    entry: &mut PageTableEntry,
    mut alloc: impl FnMut() -> Paddr,
) -> Option<&'a mut [PageTableEntry]> {
    if entry.is_unused() {
        let pa = alloc();
        *entry = PageTableEntry::new_page(
            pa,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER,
        );
        Some(table_of(pa).unwrap())
    } else {
        next_table(entry)
    }
}

/// translate a virtual address to physical address which cannot use offset to get physical address
pub fn vaddr_to_paddr(virtual_address: Vaddr) -> Option<Paddr> {
    #[cfg(feature = "x86_64")]
    let (page_directory_base, _) = x86_64::registers::control::Cr3::read();

    let page_directory_base = page_directory_base.start_address().as_u64() as usize;
    let p4 = table_of(page_directory_base)?;

    let pte = p4[p4_index(virtual_address)];
    let p3 = table_of(pte.paddr())?;

    let pte = p3[p3_index(virtual_address)];
    let p2 = table_of(pte.paddr())?;

    let pte = p2[p2_index(virtual_address)];
    let p1 = table_of(pte.paddr())?;

    let pte = p1[p1_index(virtual_address)];
    Some((pte.paddr() & ((1 << 48) - 1)) + (virtual_address & ((1 << 12) - 1)))
}

pub(crate) fn init() {
    #[cfg(feature = "x86_64")]
    let (page_directory_base, _) = x86_64::registers::control::Cr3::read();
    let page_directory_base = page_directory_base.start_address().as_u64() as usize;

    let p4 = table_of(page_directory_base).unwrap();
    // Cancel mapping in lowest addresses.
    p4[0].0 = 0;
    let mut map_pte = ALL_MAPPED_PTE.lock();
    for i in 0..512 {
        if p4[i].flags().contains(PageTableFlags::PRESENT) {
            map_pte.insert(i, p4[i]);
        }
    }
}
