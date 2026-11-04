//! Tabela de tarefas e escalonador preemptivo.
//!
//! Este modulo acrescenta a nocao de *processo/tarefa* que faltava ao kernel.
//! Cada tarefa tem PID, PID do pai, nome, estado e uma pilha de kernel
//! propria. O escalonador e *round-robin* e pode ser acionado de duas formas:
//! por `yield_now` (cooperativo) ou pelo tick do timer (preemptivo). A troca
//! de contexto salva os registros que a ABI SysV exige preservar (rbx, rbp,
//! r12..r15), o `rsp` e o `RFLAGS`, exatamente como um `switch_to` de kernel
//! pequeno.
//!
//! Isto ainda nao implementa `fork` nem espaco de enderecos por processo:
//! todas as tarefas compartilham o mesmo espaco de kernel. Ainda assim, a
//! tabela, os estados (`Ready`, `Running`, `Exited`), o PID, o PPID, a
//! politica round-robin e o *quantum* por ticks sao os mesmos conceitos
//! centrais que o Linux usa em `task_struct` e `schedule()`.

use alloc::alloc::{alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::SpinLock;

/// Numero maximo de tarefas simultaneas (incluindo a tarefa do shell).
pub const MAX_TASKS: usize = 16;
/// Tamanho da pilha de cada tarefa criada.
const STACK_SIZE: usize = 16 * 1024;
/// Ticks de timer que cada tarefa pode executar antes da preempcao.
const QUANTUM_TICKS: u64 = 5;

const NAME_MAX: usize = 24;

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Free,
    Ready,
    Running,
    /// Esperando um prazo do escalonador (equivalente a `TASK_INTERRUPTIBLE`
    /// com `schedule_timeout`). Nao consome CPU e nao e escolhida por
    /// `next_ready` ate o tick de despertar.
    Blocked,
    Exited,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Free => "livre",
            State::Ready => "pronta",
            State::Running => "rodando",
            State::Blocked => "dormindo",
            State::Exited => "encerrada",
        }
    }
}

#[derive(Clone, Copy)]
struct Task {
    state: State,
    pid: u64,
    ppid: u64,
    /// `rsp` salvo pela troca de contexto (ou preparado por `spawn`).
    rsp: u64,
    /// Base da pilha alocada no heap (nula para a tarefa do shell).
    stack: *mut u8,
    name: [u8; NAME_MAX],
    name_len: usize,
    entry: fn(),
    /// Falso enquanto a tarefa nunca rodou: o `rsp` ja aponta para o
    /// `task_trampoline`.
    started: bool,
    yields: u64,
    preemptions: u64,
    /// Tick absoluto do escalonador em que uma tarefa bloqueada deve acordar.
    /// `u64::MAX` significa "somente por evento" (`wait_event`).
    wake_at: u64,
    /// Endereco da fila de espera em que a tarefa esta bloqueada; 0 quando o
    /// bloqueio e apenas por prazo (`sleep`).
    wait_queue: u64,
}

impl Task {
    const fn free() -> Self {
        Self {
            state: State::Free,
            pid: 0,
            ppid: 0,
            rsp: 0,
            stack: core::ptr::null_mut(),
            name: [0; NAME_MAX],
            name_len: 0,
            entry: dummy_entry,
            started: false,
            yields: 0,
            preemptions: 0,
            wake_at: 0,
            wait_queue: 0,
        }
    }

    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }

    fn set_name(&mut self, name: &[u8]) {
        let len = name.len().min(NAME_MAX);
        self.name[..len].copy_from_slice(&name[..len]);
        self.name_len = len;
    }
}

// Os ponteiros de pilha sao acessados apenas com a tabela travada.
unsafe impl Send for Task {}

struct Table {
    tasks: [Task; MAX_TASKS],
    current: usize,
    next_pid: u64,
    spawned: u64,
    /// Ticks restantes no quantum da tarefa corrente.
    quantum_left: u64,
    /// Relogio do escalonador, incrementado a cada IRQ0.
    ticks: u64,
}

static TABLE: SpinLock<Table> = SpinLock::new(Table {
    tasks: [Task::free(); MAX_TASKS],
    current: 0,
    next_pid: 1,
    spawned: 0,
    quantum_left: QUANTUM_TICKS,
    ticks: 0,
});

/// Falso ate `init` instalar a tarefa do shell; o timer ignora esse periodo.
static SCHED_READY: AtomicBool = AtomicBool::new(false);

global_asm!(
    r#"
    .section .text
    .global task_switch
# task_switch(old_rsp: *mut u64, new_rsp: u64)
task_switch:
    pushfq
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rsp, (%rdi)
    movq %rsi, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    popfq
    ret
"#,
    options(att_syntax)
);

extern "C" {
    fn task_switch(old_rsp: *mut u64, new_rsp: u64);
}

fn dummy_entry() {}

/// Primeira funcao executada em uma tarefa nova. Le a entrada da tabela e a
/// chama; quando ela retorna, a tarefa encerra.
extern "C" fn task_trampoline() -> ! {
    let entry = {
        let table = TABLE.lock();
        table.tasks[table.current].entry
    };
    entry();
    exit_current()
}

fn kernel_stack_layout() -> Layout {
    // `expect` nao esta disponivel em no_std sem panic formatado; os valores
    // sao constantes validas.
    Layout::from_size_align(STACK_SIZE, 16).unwrap_or(Layout::new::<u8>())
}

/// Cria a tarefa do shell (PID 1). Deve ser chamada depois do heap e da
/// paginacao estarem prontos, mas antes de `shell::run`.
pub fn init() {
    let mut table = TABLE.lock();
    for task in table.tasks.iter_mut() {
        *task = Task::free();
    }

    let mut shell = Task::free();
    shell.state = State::Running;
    shell.pid = 1;
    shell.ppid = 0;
    shell.started = true;
    shell.set_name(b"shell");

    table.tasks[0] = shell;
    table.current = 0;
    table.next_pid = 2;
    table.spawned = 0;
    table.quantum_left = QUANTUM_TICKS;
    table.ticks = 0;
    SCHED_READY.store(true, Ordering::Release);
}

/// Cria uma tarefa pronta para rodar. A pilha vem do heap do kernel.
///
/// Slots de tarefas ja encerradas sao reaproveitados, como o `wait()` do Linux
/// liberando um zumbi.
pub fn spawn(name: &[u8], entry: fn()) -> Result<u64, &'static str> {
    let layout = kernel_stack_layout();

    let mut table = TABLE.lock();
    let slot = table
        .tasks
        .iter()
        .position(|task| task.state == State::Free || task.state == State::Exited)
        .ok_or("tabela de tarefas cheia")?;

    // Libera a pilha antiga, se o slot era de uma tarefa encerrada.
    let old_stack = table.tasks[slot].stack;
    if !old_stack.is_null() {
        unsafe { dealloc(old_stack, layout) };
    }

    let stack = unsafe { alloc_zeroed(layout) };
    if stack.is_null() {
        return Err("sem memoria para a pilha da tarefa");
    }

    // Prepara o frame inicial consumido por `task_switch`: seis registros
    // callee-saved, o `RFLAGS` salvo e o endereco de retorno para o trampolim.
    // O frame tem 64 bytes; deixamos 8 bytes de folga para que, depois do
    // `ret`, o `rsp` volte a ser 8 (mod 16), como exige a ABI SysV na entrada
    // de uma funcao. `RFLAGS` com IF = 1 permite que a nova tarefa seja
    // preemptida pelo timer.
    let top = (stack as usize + STACK_SIZE) & !15;
    let rsp = top - 8 * core::mem::size_of::<u64>() - 8;
    unsafe {
        let frame = rsp as *mut u64;
        for index in 0..6 {
            *frame.add(index) = 0;
        }
        *frame.add(6) = 0x202; // IF | bit reservado 1
        *frame.add(7) = task_trampoline as *const () as usize as u64;
    }

    let pid = table.next_pid;
    table.next_pid += 1;
    table.spawned += 1;
    let ppid = current_pid_locked(&table);

    let task = &mut table.tasks[slot];
    task.state = State::Ready;
    task.pid = pid;
    task.ppid = ppid;
    task.rsp = rsp as u64;
    task.stack = stack;
    task.entry = entry;
    task.started = false;
    task.yields = 0;
    task.preemptions = 0;
    task.wake_at = 0;
    task.wait_queue = 0;
    task.set_name(name);

    Ok(pid)
}

fn current_pid_locked(table: &Table) -> u64 {
    table.tasks[table.current].pid
}

/// PID da tarefa que esta rodando.
pub fn current_pid() -> u64 {
    let table = TABLE.lock();
    current_pid_locked(&table)
}

/// PPID da tarefa que esta rodando.
pub fn current_ppid() -> u64 {
    let table = TABLE.lock();
    table.tasks[table.current].ppid
}

fn next_ready(table: &Table) -> Option<usize> {
    let current = table.current;
    // `offset == MAX_TASKS` cairia de volta na tarefa corrente, que pode ter
    // acabado de ser marcada `Ready`; ela nao pode ser escolhida para uma
    // troca de contexto contra si mesma.
    for offset in 1..MAX_TASKS {
        let index = (current + offset) % MAX_TASKS;
        if table.tasks[index].state == State::Ready {
            return Some(index);
        }
    }
    None
}

/// Cede a CPU para a proxima tarefa pronta (round-robin).
pub fn yield_now() {
    // Desabilita interrupcoes durante a contabilidade e a troca: se o timer
    // disparasse entre elas, `table.current` ja apontaria para outra tarefa e
    // o `rsp` seria salvo no slot errado.
    let flags = crate::arch::irq_save();

    let (old_rsp, new_rsp, switched) = {
        let mut table = TABLE.lock();
        let current = table.current;
        table.tasks[current].yields += 1;

        if table.tasks[current].state == State::Running {
            table.tasks[current].state = State::Ready;
        }

        let next = match next_ready(&table) {
            Some(index) => index,
            // Sem outra tarefa pronta: continua a atual.
            None => current,
        };
        table.tasks[next].state = State::Running;
        table.current = next;
        table.quantum_left = QUANTUM_TICKS;

        (
            core::ptr::addr_of_mut!(table.tasks[current].rsp),
            table.tasks[next].rsp,
            next != current,
        )
    };

    if switched {
        unsafe { task_switch(old_rsp, new_rsp) };
    }
    crate::arch::irq_restore(flags);
}

/// Acao decidida por `sleep` a cada iteracao, fora da trava da tabela.
enum SleepAction {
    Done,
    Switch(*mut u64, u64),
    Idle,
}

/// Bloqueia a tarefa corrente por `ticks` ticks do escalonador (1 tick ~ 10 ms
/// a 100 Hz). Equivale ao `schedule_timeout` do Linux: a tarefa deixa a CPU e
/// volta ao estado `Ready` quando o prazo vence. Se nao houver outra tarefa
/// pronta, a CPU fica ociosa em `hlt` ate a proxima interrupcao, sem
/// *busy-wait*.
pub fn sleep(ticks: u64) {
    if ticks == 0 {
        yield_now();
        return;
    }

    let deadline = {
        let table = TABLE.lock();
        table.ticks.saturating_add(ticks)
    };

    loop {
        // Mantem as interrupcoes desligadas durante a contabilidade e a troca,
        // como em `yield_now`. O lock restaura IF = 0 porque `irq_save` ja
        // rodou antes; a tarefa que assume restaura o proprio IF no `popfq`.
        let flags = crate::arch::irq_save();
        let action = {
            let mut table = TABLE.lock();
            if table.ticks >= deadline {
                let current = table.current;
                table.tasks[current].state = State::Running;
                SleepAction::Done
            } else {
                let current = table.current;
                match next_ready(&table) {
                    Some(next) => {
                        table.tasks[current].state = State::Blocked;
                        table.tasks[current].wake_at = deadline;
                        table.tasks[current].wait_queue = 0;
                        table.tasks[next].state = State::Running;
                        table.current = next;
                        table.quantum_left = QUANTUM_TICKS;
                        SleepAction::Switch(
                            core::ptr::addr_of_mut!(table.tasks[current].rsp),
                            table.tasks[next].rsp,
                        )
                    }
                    // Ninguem para rodar: continua na CPU, mas dorme em `hlt`.
                    None => SleepAction::Idle,
                }
            }
        };

        match action {
            SleepAction::Done => {
                crate::arch::irq_restore(flags);
                return;
            }
            SleepAction::Switch(old_rsp, new_rsp) => {
                unsafe { task_switch(old_rsp, new_rsp) };
                // Ao voltar, o prazo pode ter vencido ou a tarefa pode ter
                // sido acordada antes; o laco reavalia.
                crate::arch::irq_restore(flags);
            }
            SleepAction::Idle => {
                // Devolve IF ao estado original e espera a proxima IRQ, em vez
                // de girar consumindo o quantum.
                crate::arch::irq_restore(flags);
                unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
            }
        }
    }
}

/// Valor de `wake_at` para bloqueios sem prazo: a tarefa so volta quando
/// alguem chama `wake_up` na fila em que ela dorme.
const NO_DEADLINE: u64 = u64::MAX;

/// Bloqueia a tarefa corrente em uma fila de espera. Retorna `true` se ela foi
/// acordada por um evento e `false` se o prazo venceu antes. Com
/// `deadline == NO_DEADLINE` a espera nao tem prazo (como
/// `wait_event_interruptible`); caso contrario assemelha-se a
/// `schedule_timeout`.
fn block_current(wait_queue: u64, deadline: u64) -> bool {
    // Verdadeiro depois que esta tarefa ja dormiu e voltou a rodar.
    let mut blocked = false;

    loop {
        // Mesmo protocolo de `sleep`: desliga IRQs durante a contabilidade e a
        // troca para o `rsp` nunca ser salvo no slot errado.
        let flags = crate::arch::irq_save();
        let (action, event) = {
            let mut table = TABLE.lock();
            let current = table.current;
            let timed_out = table.ticks >= deadline;

            if blocked || timed_out {
                // Acordou por evento se ja tinha dormido e o prazo nao venceu.
                let event = blocked && !timed_out;
                table.tasks[current].state = State::Running;
                table.tasks[current].wait_queue = 0;
                table.tasks[current].wake_at = 0;
                (SleepAction::Done, event)
            } else {
                match next_ready(&table) {
                    Some(next) => {
                        let task = &mut table.tasks[current];
                        task.state = State::Blocked;
                        task.wait_queue = wait_queue;
                        task.wake_at = deadline;
                        table.tasks[next].state = State::Running;
                        table.current = next;
                        table.quantum_left = QUANTUM_TICKS;
                        blocked = true;
                        (
                            SleepAction::Switch(
                                core::ptr::addr_of_mut!(table.tasks[current].rsp),
                                table.tasks[next].rsp,
                            ),
                            false,
                        )
                    }
                    // Ninguem para rodar: dorme em `hlt` ate a proxima IRQ e
                    // reavalia o prazo, em vez de girar.
                    None => (SleepAction::Idle, false),
                }
            }
        };

        match action {
            SleepAction::Done => {
                crate::arch::irq_restore(flags);
                return event;
            }
            SleepAction::Switch(old_rsp, new_rsp) => {
                unsafe { task_switch(old_rsp, new_rsp) };
                crate::arch::irq_restore(flags);
            }
            SleepAction::Idle => {
                crate::arch::irq_restore(flags);
                unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
            }
        }
    }
}

/// Fila de espera no estilo `wait_queue_head_t` do Linux.
///
/// A identidade da fila e o proprio endereco da estrutura, entao ela pode ser
/// declarada como `static` sem inicializacao dinamica. Os waiters ficam na
/// tabela de tarefas (com `wait_queue` apontando para a fila), e `wake_up`
/// varre a tabela marcando as tarefas correspondentes como prontas. E uma
/// versao simplificada das listas intrusivas de `include/linux/wait.h`, mas
/// mantem o contrato central: dormir ate uma condicao e ser acordado por
/// outro contexto.
pub struct WaitQueue {
    _private: (),
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self { _private: () }
    }

    fn key(&self) -> u64 {
        self as *const Self as u64
    }

    /// Numero de tarefas bloqueadas nesta fila.
    pub fn waiters(&self) -> usize {
        let key = self.key();
        let table = TABLE.lock();
        table
            .tasks
            .iter()
            .filter(|task| task.state == State::Blocked && task.wait_queue == key)
            .count()
    }

    /// Espera ate `condition` devolver `true` (equivalente a `wait_event`).
    /// A condicao e reavaliada apos cada despertar para tolerar wakeups
    /// espurios.
    pub fn wait_event<F: FnMut() -> bool>(&self, mut condition: F) {
        while !condition() {
            block_current(self.key(), NO_DEADLINE);
        }
    }

    /// Como `wait_event`, mas com prazo em ticks (`wait_event_timeout`).
    /// Devolve `true` se a condicao era verdadeira quando a espera terminou.
    pub fn wait_event_timeout<F: FnMut() -> bool>(&self, ticks: u64, mut condition: F) -> bool {
        let deadline = {
            let table = TABLE.lock();
            table.ticks.saturating_add(ticks)
        };
        while !condition() {
            if !block_current(self.key(), deadline) {
                break;
            }
        }
        condition()
    }

    /// Acorda todas as tarefas bloqueadas nesta fila (`wake_up_all`).
    /// Devolve quantas foram acordadas.
    pub fn wake_up_all(&self) -> usize {
        wake(self.key(), false)
    }

    /// Acorda apenas a primeira tarefa da fila (`wake_up`).
    pub fn wake_up_one(&self) -> usize {
        wake(self.key(), true)
    }
}

/// Marca como prontas as tarefas bloqueadas em `wait_queue`. Nao troca de
/// contexto: apenas torna a tarefa elegivel, como `wake_up` no Linux.
fn wake(wait_queue: u64, only_one: bool) -> usize {
    let flags = crate::arch::irq_save();
    let mut woken = 0;
    {
        let mut table = TABLE.lock();
        for task in table.tasks.iter_mut() {
            if task.state == State::Blocked && task.wait_queue == wait_queue {
                task.state = State::Ready;
                task.wait_queue = 0;
                task.wake_at = 0;
                woken += 1;
                if only_one {
                    break;
                }
            }
        }
    }
    crate::arch::irq_restore(flags);
    woken
}

/// Chamado a cada tick do timer. Quando o quantum termina, marca a tarefa
/// corrente como pronta e troca para a proxima, sem participacao da tarefa.
pub fn on_tick() {
    if !SCHED_READY.load(Ordering::Acquire) {
        return;
    }

    let user_active = crate::user::is_active();

    // Relogio do escalonador e despertar das tarefas dormindo. O tempo corre
    // mesmo durante um programa de ring 3, mas nesse caso nao trocamos de
    // tarefa logo abaixo.
    {
        let mut table = TABLE.lock();
        table.ticks = table.ticks.wrapping_add(1);
        let now = table.ticks;
        for task in table.tasks.iter_mut() {
            if task.state == State::Blocked && task.wake_at <= now {
                task.state = State::Ready;
                task.wait_queue = 0;
            }
        }
    }

    // Durante um programa de ring 3 a pilha de kernel e a pilha compartilhada
    // de interrupcao do TSS, nao a pilha de uma tarefa; nao preemptamos.
    if user_active {
        return;
    }

    let (old_rsp, new_rsp, switched) = {
        let mut table = TABLE.lock();
        if table.quantum_left > 1 {
            table.quantum_left -= 1;
            (core::ptr::null_mut::<u64>(), 0, false)
        } else {
            table.quantum_left = QUANTUM_TICKS;
            let current = table.current;
            let task = &mut table.tasks[current];
            if task.state != State::Running || task.pid == 0 {
                (core::ptr::null_mut::<u64>(), 0, false)
            } else {
                if task.state == State::Running {
                    task.state = State::Ready;
                }
                match next_ready(&table) {
                    Some(next) => {
                        table.tasks[current].preemptions += 1;
                        table.tasks[next].state = State::Running;
                        table.current = next;
                        (
                            core::ptr::addr_of_mut!(table.tasks[current].rsp),
                            table.tasks[next].rsp,
                            true,
                        )
                    }
                    None => {
                        table.tasks[current].state = State::Running;
                        (core::ptr::null_mut::<u64>(), 0, false)
                    }
                }
            }
        }
    };

    if switched {
        unsafe { task_switch(old_rsp, new_rsp) };
    }
}

/// Termina a tarefa atual e nunca retorna.
pub fn exit_current() -> ! {
    let flags = crate::arch::irq_save();

    let (old_rsp, new_rsp) = {
        let mut table = TABLE.lock();
        let current = table.current;
        table.tasks[current].state = State::Exited;

        // Prefere outra tarefa pronta; caso contrario volta para o shell.
        let next = match next_ready(&table) {
            Some(index) => index,
            None => 0,
        };
        table.tasks[next].state = State::Running;
        table.current = next;
        table.quantum_left = QUANTUM_TICKS;

        (
            core::ptr::addr_of_mut!(table.tasks[current].rsp),
            table.tasks[next].rsp,
        )
    };

    unsafe { task_switch(old_rsp, new_rsp) };

    // Uma tarefa encerrada nunca deve ser escalonada de novo.
    crate::arch::irq_restore(flags);
    crate::arch::halt_loop()
}

/// Ponto de entrada ocioso do shell. Bloqueia por um tick, deixando o timer
/// acordar tarefas prontas; sem outra tarefa, o `hlt` de `sleep` evita
/// *busy-wait* e o shell volta a pollar teclado/serial a cada ~10 ms.
pub fn idle() {
    sleep(1);
}

/// Uma linha de `ps` para cada tarefa da tabela.
pub fn for_each(mut visit: impl FnMut(u64, u64, State, &str, u64, u64, bool)) {
    let table = TABLE.lock();
    for (index, task) in table.tasks.iter().enumerate() {
        if task.state == State::Free {
            continue;
        }
        visit(
            task.pid,
            task.ppid,
            task.state,
            task.name(),
            task.yields,
            task.preemptions,
            index == table.current,
        );
    }
}

/// Resumo numerico para `info`/`mem`.
#[derive(Clone, Copy)]
pub struct Stats {
    pub total: usize,
    pub ready: usize,
    pub running: usize,
    pub blocked: usize,
    pub exited: usize,
    pub spawned: u64,
    pub preemptions: u64,
}

pub fn stats() -> Stats {
    let table = TABLE.lock();
    let mut stats = Stats {
        total: 0,
        ready: 0,
        running: 0,
        blocked: 0,
        exited: 0,
        spawned: table.spawned,
        preemptions: 0,
    };
    for task in table.tasks.iter() {
        match task.state {
            State::Free => continue,
            State::Ready => stats.ready += 1,
            State::Running => stats.running += 1,
            State::Blocked => stats.blocked += 1,
            State::Exited => stats.exited += 1,
        }
        stats.preemptions += task.preemptions;
        stats.total += 1;
    }
    stats
}
