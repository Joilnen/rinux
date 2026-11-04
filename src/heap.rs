//! Alocador de heap do kernel.
//!
//! O heap vive em uma regiao virtual propria (`0xFFFFFFFFE0000000`) e cresce
//! paginando frames de 4 KiB sob demanda. Cada bloco livre tem um cabecalho
//! imediatamente antes do bloco seguinte, como em um heap classico de lista
//! encadeada:
//!
//! ```text
//! bloco livre: [ size | next | ......... espaco livre ......... ]
//! bloco usado: [ size | next | dados devolvidos ao chamador .... ]
//! ```
//!
//! O cabecalho tem 16 bytes e fica sempre imediatamente antes do ponteiro
//! devolvido, o que permite implementar `dealloc` sem conhecer o alinhamento
//! original. Blocos livres sao mantidos em ordem de endereco e coalescidos
//! quando vizinhos. Nao ha slab, buddy allocator nem NUMA: e um heap
//! didatico, equivalente em espirito ao `kmalloc` simples de um kernel
//! pequeno.
//!
//! O heap usa `alloc` apenas do proprio Rust (`core::alloc`); nao ha
//! dependencias externas.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::paging;
use crate::sync::SpinLock;

/// Base virtual do heap do kernel. Fica em um PDPT separado do kernel e da
/// regiao de demand paging, entao nao colide com nenhum mapeamento existente.
pub const HEAP_BASE: u64 = 0xFFFF_FFFF_E000_0000;
/// Tamanho maximo do heap: 1 MiB (256 paginas de 4 KiB).
pub const HEAP_MAX: usize = 256 * 4096;

const PAGE: usize = 4096;
const HEADER: usize = core::mem::size_of::<Block>();
const MIN_SPLIT: usize = HEADER + 16;

#[repr(C, align(16))]
struct Block {
    size: usize,
    next: *mut Block,
}

struct Heap {
    free: *mut Block,
    next_page: usize,
    mapped: usize,
    used: usize,
    allocs: usize,
}

// O acesso aos ponteiros e serializado pelo SpinLock.
unsafe impl Send for Heap {}

impl Heap {
    const fn new() -> Self {
        Self {
            free: ptr::null_mut(),
            next_page: 0,
            mapped: 0,
            used: 0,
            allocs: 0,
        }
    }

    fn grow(&mut self) -> Result<(), ()> {
        if self.mapped >= HEAP_MAX {
            return Err(());
        }
        if self.next_page == 0 {
            self.next_page = HEAP_BASE as usize;
        }

        let frame = paging::alloc_frame().ok_or(())?;
        let virt = self.next_page as u64;
        unsafe {
            paging::map_page(
                virt,
                frame,
                paging::PAGE_WRITE,
                false,
            )
            .map_err(|_| ())?;
        }

        let block = virt as *mut Block;
        unsafe {
            ptr::write(
                block,
                Block {
                    size: PAGE,
                    next: ptr::null_mut(),
                },
            );
        }
        self.next_page += PAGE;
        self.mapped += PAGE;
        unsafe {
            self.insert_free(block);
        }
        Ok(())
    }

    unsafe fn insert_free(&mut self, block: *mut Block) {
        let mut prev: *mut Block = ptr::null_mut();
        let mut cur = self.free;

        while !cur.is_null() && cur < block {
            prev = cur;
            cur = (*cur).next;
        }

        (*block).next = cur;
        if prev.is_null() {
            self.free = block;
        } else {
            (*prev).next = block;
        }

        // Coalesce com o proximo.
        if !cur.is_null() && block as usize + (*block).size == cur as usize {
            (*block).size += (*cur).size;
            (*block).next = (*cur).next;
        }
        // Coalesce com o anterior.
        if !prev.is_null() && prev as usize + (*prev).size == block as usize {
            (*prev).size += (*block).size;
            (*prev).next = (*block).next;
        }
    }

    unsafe fn take(&mut self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(1);
        let align = layout.align().max(16);
        let mut prev: *mut Block = ptr::null_mut();
        let mut cur = self.free;

        while !cur.is_null() {
            let start = cur as usize;
            let end = start + (*cur).size;

            // O cabecalho precisa caber antes do ponteiro alinhado.
            let data = align_up(start + HEADER, align);
            let block_start = data - HEADER;
            let used_end = align_up(block_start + HEADER + size, 16);

            if block_start >= start && used_end <= end {
                let next = (*cur).next;

                // Remove o buraco inteiro da lista.
                if prev.is_null() {
                    self.free = next;
                } else {
                    (*prev).next = next;
                }

                // Devolve o prefixo que sobrou como bloco livre, se couber.
                let prefix = block_start - start;
                if prefix >= MIN_SPLIT {
                    let prefix_block = cur;
                    (*prefix_block).size = prefix;
                    (*prefix_block).next = ptr::null_mut();
                    self.insert_free(prefix_block);
                }

                // Devolve o sufixo que sobrou como bloco livre, se couber.
                let suffix = end - used_end;
                if suffix >= MIN_SPLIT {
                    let suffix_block = used_end as *mut Block;
                    (*suffix_block).size = suffix;
                    (*suffix_block).next = ptr::null_mut();
                    self.insert_free(suffix_block);
                }

                let used_block = block_start as *mut Block;
                let used_size = if suffix >= MIN_SPLIT {
                    used_end - block_start
                } else {
                    end - block_start
                };
                ptr::write(
                    used_block,
                    Block {
                        size: used_size,
                        next: ptr::null_mut(),
                    },
                );

                self.used += size;
                self.allocs += 1;
                return (block_start + HEADER) as *mut u8;
            }

            prev = cur;
            cur = (*cur).next;
        }

        ptr::null_mut()
    }

    unsafe fn give_back(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        let block = ptr.sub(HEADER) as *mut Block;
        self.used = self.used.saturating_sub((*block).size - HEADER);
        self.insert_free(block);
    }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap::new());

/// Alocador global registrado em `main.rs`.
pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = HEAP.lock();
        let ptr = heap.take(layout);
        if !ptr.is_null() {
            return ptr;
        }
        drop(heap);

        // Sem bloco livre: cresce o heap e tenta de novo. O lock e solto
        // durante `grow` para nao prender o heap enquanto o alocador de
        // frames cria tabelas de pagina.
        loop {
            {
                let mut heap = HEAP.lock();
                if heap.mapped >= HEAP_MAX {
                    return ptr::null_mut();
                }
                if heap.grow().is_err() {
                    return ptr::null_mut();
                }
                let ptr = heap.take(layout);
                if !ptr.is_null() {
                    return ptr;
                }
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        HEAP.lock().give_back(ptr);
    }
}

/// Estatisticas do heap para o comando `mem`/`heap`.
#[derive(Clone, Copy)]
pub struct Stats {
    pub mapped: usize,
    pub used: usize,
    pub allocs: usize,
}

pub fn stats() -> Stats {
    let heap = HEAP.lock();
    Stats {
        mapped: heap.mapped,
        used: heap.used,
        allocs: heap.allocs,
    }
}
