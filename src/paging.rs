//! Paginacao de 4 KiB, alocador de frames e paginas sob demanda.
//!
//! O stub de boot (`boot.rs`) deixa o primeiro GiB mapeado por identidade com
//! paginas gigantes de 2 MiB, e o kernel na metade alta. Este modulo acrescenta
//! o que falta para rodar codigo de usuario:
//!
//!   * um alocador de frames fisicos baseado no mapa de memoria do Multiboot2;
//!   * criacao de tabelas de pagina de 4 KiB sob demanda;
//!   * duas regioes de memoria virtual que so sao materializadas quando a CPU
//!     gera page fault (demand paging):
//!       - `USER_BASE`   : regiao do usuario, folhas com U/S = 1;
//!       - `KDEMAND_BASE`: regiao do kernel, folhas com U/S = 0.
//!
//! A protecao usuario/kernel vem dos bits U/S de *todos* os niveis da tabela:
//! o hardware so permite acesso em ring 3 quando PML4, PDPT, PD e PT tem
//! U/S = 1. As tabelas do kernel e o mapeamento de identidade ficam com
//! U/S = 0, portanto o usuario nao consegue ler, escrever nem executar o
//! kernel, mesmo conhecendo os enderecos.

use core::arch::asm;
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Base virtual do kernel (mesma de `linker.ld`).
pub const KERNEL_VMA: u64 = 0xFFFF_FFFF_8000_0000;

/// Maior endereco fisico que o alocador conhece. O stub so mapeia o primeiro
/// GiB por identidade, entao nao adianta entregar frames acima disso.
pub const MAX_PHYS: u64 = 1 << 30;

const FRAME_SIZE: u64 = 4096;
const MAX_FRAMES: usize = (MAX_PHYS / FRAME_SIZE) as usize;
const BITMAP_BYTES: usize = MAX_FRAMES / 8;

// Bits de entrada de tabela de pagina.
pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITE: u64 = 1 << 1;
pub const PAGE_USER: u64 = 1 << 2;
pub const PAGE_HUGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

// Bits do codigo de erro do page fault (empilhado pela CPU).
const PF_PRESENT: u64 = 1 << 0;
const PF_USER: u64 = 1 << 2;

/// Regiao virtual do usuario (PML4[1], 512 GiB em diante).
pub const USER_BASE: u64 = 0x0000_0080_0000_0000;
pub const USER_REGION_SIZE: u64 = 2 * 1024 * 1024;

/// Pagina de codigo do usuario, mapeada explicitamente por `user.rs`.
pub const USER_CODE: u64 = USER_BASE + 0x1000;
/// Pagina de dados usada para demonstrar demand paging em ring 3.
pub const USER_DATA: u64 = USER_BASE + 0x2000;
/// Topo da pilha do usuario. A pagina abaixo dele tambem e sob demanda.
pub const USER_STACK_TOP: u64 = USER_BASE + 0x8000;

/// Regiao de demand paging do kernel. Fica no PDPT[511] da metade alta,
/// logo depois do GiB que o stub mapeia com paginas de 2 MiB.
pub const KDEMAND_BASE: u64 = 0xFFFF_FFFF_C000_0000;
pub const KDEMAND_SIZE: u64 = 2 * 1024 * 1024;

/// Bitmap: 1 = frame em uso, 0 = frame livre. Comeca todo em uso; `init`
/// libera apenas as faixas realmente disponiveis informadas pelo Multiboot2.
static mut FRAME_BITMAP: [u8; BITMAP_BYTES] = [0xFF; BITMAP_BYTES];

static NEXT_FRAME: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_FRAMES: AtomicUsize = AtomicUsize::new(0);
static FREE_FRAMES: AtomicUsize = AtomicUsize::new(0);
static DEMAND_FAULTS: AtomicU64 = AtomicU64::new(0);

extern "C" {
    /// Simbolo definido no fim de `linker.ld` (endereco virtual alto).
    static __kernel_end: u8;
}

#[repr(C, align(4096))]
struct PageTable {
    entries: [u64; 512],
}

/// Estatisticas simples para o comando `mem`.
#[derive(Clone, Copy)]
pub struct Stats {
    pub allocated: usize,
    pub free: usize,
    pub faults: u64,
}

unsafe fn bitmap() -> *mut u8 {
    ptr::addr_of_mut!(FRAME_BITMAP).cast::<u8>()
}

fn frame_is_used(frame: usize) -> bool {
    unsafe { *bitmap().add(frame / 8) & (1 << (frame % 8)) != 0 }
}

fn mark_used(frame: usize) {
    unsafe {
        *bitmap().add(frame / 8) |= 1 << (frame % 8);
    }
}

fn mark_free(frame: usize) {
    if frame >= MAX_FRAMES || !frame_is_used(frame) {
        return;
    }
    unsafe {
        *bitmap().add(frame / 8) &= !(1 << (frame % 8));
    }
    FREE_FRAMES.fetch_add(1, Ordering::Relaxed);
}

fn free_range(start: u64, end: u64, reserve_end: u64) {
    let start = start.max(reserve_end);
    let end = end.min(MAX_PHYS);
    if start >= end {
        return;
    }

    let first = start.div_ceil(FRAME_SIZE) as usize;
    let last = (end / FRAME_SIZE) as usize;
    for frame in first..last.min(MAX_FRAMES) {
        mark_free(frame);
    }
}

fn reserve_range(start: u64, end: u64) {
    let first = (start / FRAME_SIZE) as usize;
    let last = end.div_ceil(FRAME_SIZE) as usize;
    for frame in first..last.min(MAX_FRAMES) {
        mark_used(frame);
    }
}

/// Inicializa o alocador de frames lendo o mapa de memoria do Multiboot2.
///
/// Tudo comeca reservado. Depois liberamos as faixas `type == 1`
/// (RAM disponivel) que ficam acima do fim do kernel.
pub fn init(multiboot_magic: u32, multiboot_info: u32) {
    unsafe {
        ptr::write_bytes(bitmap(), 0xFF, BITMAP_BYTES);
    }
    NEXT_FRAME.store(0, Ordering::Relaxed);
    ALLOCATED_FRAMES.store(0, Ordering::Relaxed);
    FREE_FRAMES.store(0, Ordering::Relaxed);
    DEMAND_FAULTS.store(0, Ordering::Relaxed);

    let kernel_end = {
        let end = ptr::addr_of!(__kernel_end) as u64;
        end.wrapping_sub(KERNEL_VMA)
    };
    let reserve_end = (kernel_end + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    // Nunca entregamos o primeiro MiB nem o stub/tabelas de boot.
    let reserve_end = reserve_end.max(2 * 1024 * 1024);

    let mut have_mmap = false;
    let mut mem_upper_kib = 0u32;

    if multiboot_magic == 0x36D7_6289 && multiboot_info != 0 {
        let info = multiboot_info as u64;
        let total = unsafe { ptr::read_unaligned(info as *const u32) } as usize;
        let total = total.min(1024 * 1024);
        let mut offset = 8usize;

        while offset + 8 <= total {
            let tag = info + offset as u64;
            let kind = unsafe { ptr::read_unaligned(tag as *const u32) };
            let size = unsafe { ptr::read_unaligned((tag + 4) as *const u32) } as usize;
            if size < 8 {
                break;
            }

            match kind {
                // Tag 4: memoria baixa/alta em KiB.
                4 if size >= 16 => {
                    mem_upper_kib =
                        unsafe { ptr::read_unaligned((tag + 8) as *const u32) };
                }
                // Tag 6: mapa de memoria (entradas de 24 bytes).
                6 if size >= 16 => {
                    have_mmap = true;
                    let entry_size = unsafe {
                        ptr::read_unaligned((tag + 8) as *const u32)
                    } as usize;
                    if entry_size >= 24 {
                        let entries = size - 16;
                        let mut pos = 0usize;
                        while pos + entry_size <= entries {
                            let entry = tag + 16 + pos as u64;
                            let base =
                                unsafe { ptr::read_unaligned(entry as *const u64) };
                            let length = unsafe {
                                ptr::read_unaligned((entry + 8) as *const u64)
                            };
                            let entry_type = unsafe {
                                ptr::read_unaligned((entry + 16) as *const u32)
                            };
                            if entry_type == 1 && length != 0 {
                                let end = base.saturating_add(length);
                                free_range(base, end, reserve_end);
                            }
                            pos += entry_size;
                        }
                    }
                }
                0 => break,
                _ => {}
            }

            offset += (size + 7) & !7;
        }

        // A propria estrutura do Multiboot2 ainda vai ser lida pelo kernel.
        let info_end = info + total as u64;
        reserve_range(info, info_end);
    }

    if !have_mmap {
        // Fallback didatico: usa a tag de memoria baixa/alta (sem mapa de
        // buracos). Se nao houver nenhuma informacao, limita a 16 MiB.
        let top = if mem_upper_kib == 0 {
            16 * 1024 * 1024
        } else {
            (1024 * 1024) + (mem_upper_kib as u64) * 1024
        };
        free_range(reserve_end, top.min(MAX_PHYS), reserve_end);
    }
}

/// Aloca um frame fisico zerado (ou `None` quando a RAM acabou).
pub fn alloc_frame() -> Option<u64> {
    let start = NEXT_FRAME.load(Ordering::Relaxed);
    let mut found = None;

    for pass in 0..2 {
        let (begin, end) = if pass == 0 {
            (start, MAX_FRAMES)
        } else {
            (0, start)
        };

        for frame in begin..end {
            if !frame_is_used(frame) {
                mark_used(frame);
                NEXT_FRAME.store(frame + 1, Ordering::Relaxed);
                ALLOCATED_FRAMES.fetch_add(1, Ordering::Relaxed);
                FREE_FRAMES.fetch_sub(1, Ordering::Relaxed);
                found = Some(frame as u64 * FRAME_SIZE);
                break;
            }
        }

        if found.is_some() {
            break;
        }
    }

    let frame = found?;
    // O primeiro GiB e mapeado por identidade no stub: escrever no endereco
    // fisico e o mesmo que escrever no endereco virtual baixo.
    unsafe {
        ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE as usize);
    }
    Some(frame)
}

pub fn free_frame(frame: u64) {
    if frame % FRAME_SIZE != 0 || frame >= MAX_PHYS {
        return;
    }
    let index = (frame / FRAME_SIZE) as usize;
    if frame_is_used(index) {
        mark_free(index);
        ALLOCATED_FRAMES.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn stats() -> Stats {
    Stats {
        allocated: ALLOCATED_FRAMES.load(Ordering::Relaxed),
        free: FREE_FRAMES.load(Ordering::Relaxed),
        faults: DEMAND_FAULTS.load(Ordering::Relaxed),
    }
}

pub fn demand_faults() -> u64 {
    DEMAND_FAULTS.load(Ordering::Relaxed)
}

fn read_cr3() -> u64 {
    let value: u64;
    unsafe {
        asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

unsafe fn invlpg(virt: u64) {
    asm!(
        "invlpg [{}]",
        in(reg) virt,
        options(nostack, preserves_flags)
    );
}

unsafe fn next_table(entry: *mut u64, user: bool) -> Result<*mut PageTable, &'static str> {
    if *entry & PAGE_PRESENT == 0 {
        let frame = alloc_frame().ok_or("sem frames fisicos")?;
        let mut flags = PAGE_PRESENT | PAGE_WRITE;
        if user {
            flags |= PAGE_USER;
        }
        *entry = frame | flags;
        Ok(frame as *mut PageTable)
    } else {
        if *entry & PAGE_HUGE != 0 {
            return Err("pagina gigante no caminho");
        }
        if user && *entry & PAGE_USER == 0 {
            // Nunca transforme uma tabela do kernel em tabela de usuario.
            return Err("tabela supervisor no caminho do usuario");
        }
        Ok((*entry & ADDR_MASK) as *mut PageTable)
    }
}

/// Mapeia uma pagina de 4 KiB. `user_tables` marca todos os niveis criados
/// para a regiao de usuario com U/S = 1.
///
/// # Safety
/// `virt` e `phys` devem estar alinhados a 4096 e o chamador deve garantir que
/// sobrescrever um mapeamento existente nao quebra o kernel.
pub unsafe fn map_page(
    virt: u64,
    phys: u64,
    flags: u64,
    user_tables: bool,
) -> Result<(), &'static str> {
    if virt & (FRAME_SIZE - 1) != 0 || phys & (FRAME_SIZE - 1) != 0 {
        return Err("endereco desalinhado");
    }

    let pml4 = (read_cr3() & ADDR_MASK) as *mut PageTable;
    let pml4e = ptr::addr_of_mut!((*pml4).entries[((virt >> 39) & 0x1FF) as usize]);
    let pdpt = next_table(pml4e, user_tables)?;
    let pdpte = ptr::addr_of_mut!((*pdpt).entries[((virt >> 30) & 0x1FF) as usize]);
    let pd = next_table(pdpte, user_tables)?;
    let pde = ptr::addr_of_mut!((*pd).entries[((virt >> 21) & 0x1FF) as usize]);
    let pt = next_table(pde, user_tables)?;
    let pte = ptr::addr_of_mut!((*pt).entries[((virt >> 12) & 0x1FF) as usize]);

    *pte = (phys & ADDR_MASK) | flags | PAGE_PRESENT;
    invlpg(virt);
    Ok(())
}

/// Devolve a entrada final que traduz `virt`, sem criar tabelas.
pub fn page_flags(virt: u64) -> Option<u64> {
    let pml4 = (read_cr3() & ADDR_MASK) as *const PageTable;
    let e4 = unsafe { ptr::read_unaligned(ptr::addr_of!((*pml4).entries[((virt >> 39) & 0x1FF) as usize])) };
    if e4 & PAGE_PRESENT == 0 {
        return None;
    }

    let pdpt = (e4 & ADDR_MASK) as *const PageTable;
    let e3 = unsafe { ptr::read_unaligned(ptr::addr_of!((*pdpt).entries[((virt >> 30) & 0x1FF) as usize])) };
    if e3 & PAGE_PRESENT == 0 {
        return None;
    }
    if e3 & PAGE_HUGE != 0 {
        return Some(e3);
    }

    let pd = (e3 & ADDR_MASK) as *const PageTable;
    let e2 = unsafe { ptr::read_unaligned(ptr::addr_of!((*pd).entries[((virt >> 21) & 0x1FF) as usize])) };
    if e2 & PAGE_PRESENT == 0 {
        return None;
    }
    if e2 & PAGE_HUGE != 0 {
        return Some(e2);
    }

    let pt = (e2 & ADDR_MASK) as *const PageTable;
    let e1 = unsafe { ptr::read_unaligned(ptr::addr_of!((*pt).entries[((virt >> 12) & 0x1FF) as usize])) };
    if e1 & PAGE_PRESENT == 0 {
        None
    } else {
        Some(e1)
    }
}

/// Remove um mapeamento de 4 KiB e libera o frame fisico.
pub fn unmap_page(virt: u64) -> Result<(), &'static str> {
    if virt & (FRAME_SIZE - 1) != 0 {
        return Err("endereco desalinhado");
    }

    let pml4 = (read_cr3() & ADDR_MASK) as *mut PageTable;
    let e4 = unsafe { ptr::read_unaligned(ptr::addr_of!((*pml4).entries[((virt >> 39) & 0x1FF) as usize])) };
    if e4 & PAGE_PRESENT == 0 {
        return Ok(());
    }
    let pdpt = (e4 & ADDR_MASK) as *mut PageTable;
    let e3 = unsafe { ptr::read_unaligned(ptr::addr_of!((*pdpt).entries[((virt >> 30) & 0x1FF) as usize])) };
    if e3 & PAGE_PRESENT == 0 || e3 & PAGE_HUGE != 0 {
        return Ok(());
    }
    let pd = (e3 & ADDR_MASK) as *mut PageTable;
    let e2 = unsafe { ptr::read_unaligned(ptr::addr_of!((*pd).entries[((virt >> 21) & 0x1FF) as usize])) };
    if e2 & PAGE_PRESENT == 0 || e2 & PAGE_HUGE != 0 {
        return Ok(());
    }
    let pt = (e2 & ADDR_MASK) as *mut PageTable;
    let pte = unsafe { ptr::addr_of_mut!((*pt).entries[((virt >> 12) & 0x1FF) as usize]) };

    let old = unsafe { ptr::read_unaligned(pte) };
    if old & PAGE_PRESENT != 0 {
        unsafe {
            ptr::write_unaligned(pte, 0);
            invlpg(virt);
        }
        free_frame(old & ADDR_MASK);
    }
    Ok(())
}

/// Descarta as paginas de pilha/dados criadas sob demanda para o demo de
/// usuario, permitindo que `usertest` gere page faults de novo.
pub fn reset_user_demand() {
    let _ = unmap_page(paging_page(USER_STACK_TOP - 1));
    let _ = unmap_page(USER_DATA);
}

fn paging_page(address: u64) -> u64 {
    address & !(FRAME_SIZE - 1)
}

/// Trata o page fault. Retorna `Ok` quando a pagina foi materializada.
pub fn handle_page_fault(addr: u64, error: u64) -> Result<(), &'static str> {
    if (USER_BASE..USER_BASE + USER_REGION_SIZE).contains(&addr) {
        if error & PF_PRESENT != 0 {
            return Err("violacao de protecao em pagina de usuario");
        }

        let frame = alloc_frame().ok_or("sem frames para o usuario")?;
        let result = unsafe {
            map_page(
                addr & !(FRAME_SIZE - 1),
                frame,
                PAGE_PRESENT | PAGE_WRITE | PAGE_USER,
                true,
            )
        };
        if let Err(reason) = result {
            free_frame(frame);
            return Err(reason);
        }
        DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    if (KDEMAND_BASE..KDEMAND_BASE + KDEMAND_SIZE).contains(&addr) {
        if error & PF_USER != 0 {
            return Err("usuario tentou acessar uma pagina do kernel");
        }
        if error & PF_PRESENT != 0 {
            return Err("violacao de protecao na demanda do kernel");
        }

        let frame = alloc_frame().ok_or("sem frames para o kernel")?;
        let result = unsafe {
            map_page(
                addr & !(FRAME_SIZE - 1),
                frame,
                PAGE_PRESENT | PAGE_WRITE,
                false,
            )
        };
        if let Err(reason) = result {
            free_frame(frame);
            return Err(reason);
        }
        DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    Err("endereco fora das regioes de demand paging")
}

/// Endereco de uma pagina da regiao de demand paging do kernel.
pub fn kernel_demand_addr(index: usize) -> u64 {
    KDEMAND_BASE + (index as u64) * FRAME_SIZE
}

/// Copia o programa do usuario para a pagina de codigo, criando-a na primeira
/// chamada. Paginas de codigo sao residentes; pilha e dados sao sob demanda.
pub fn install_user_code(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > FRAME_SIZE as usize {
        return Err("programa de usuario grande demais");
    }

    if page_flags(USER_CODE).is_none() {
        let frame = alloc_frame().ok_or("sem frames para o codigo do usuario")?;
        unsafe {
            map_page(
                USER_CODE,
                frame,
                PAGE_PRESENT | PAGE_WRITE | PAGE_USER,
                true,
            )?;
        }
    }

    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), USER_CODE as *mut u8, bytes.len());
    }
    Ok(())
}
