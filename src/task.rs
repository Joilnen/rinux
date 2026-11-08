//! Tabela de tarefas e escalonador preemptivo.
//!
//! Este modulo acrescenta a nocao de *processo/tarefa* que faltava ao kernel.
//! Cada tarefa tem PID, PID do pai, nome, estado, prioridade (`nice`), peso e
//! `vruntime` (tempo virtual de execucao) e uma pilha de kernel propria. O
//! escalonador e *weighted fair* no estilo do CFS do Linux e pode ser acionado
//! de duas formas: por `yield_now`/bloqueio (cooperativo) ou pelo tick do
//! timer (preemptivo). A troca de contexto salva os registros que a ABI SysV
//! exige preservar (rbx, rbp, r12..r15), o `rsp` e o `RFLAGS`, exatamente como
//! um `switch_to` de kernel pequeno.
//!
//! A cada tick a tarefa corrente acumula `vruntime` proporcional ao inverso do
//! seu peso; a proxima tarefa escolhida e sempre a de menor `vruntime` entre
//! as prontas. Tarefas com `nice` menor tem peso maior e, portanto, acumulam
//! `vruntime` mais devagar e recebem mais CPU. E uma versao didatica do
//! `sched/fair.c`: nao ha arvore rubro-negra, *lag* nem `EEVDF`, mas os
//! conceitos centrais de peso, prioridade e tempo virtual sao os mesmos.
//!
//! Isto ainda nao implementa `fork` nem espaco de enderecos por processo:
//! todas as tarefas compartilham o mesmo espaco de kernel. Ainda assim, a
//! tabela, os estados (`Ready`, `Running`, `Exited`), o PID, o PPID, o `nice`
//! e o `vruntime` sao os mesmos conceitos centrais que o Linux usa em
//! `task_struct` e `schedule()`.

use alloc::alloc::{alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::SpinLock;

/// Numero maximo de tarefas simultaneas (incluindo a tarefa do shell).
pub const MAX_TASKS: usize = 16;
/// Tamanho da pilha de cada tarefa criada.
const STACK_SIZE: usize = 16 * 1024;
/// Peso de uma tarefa com `nice` 0, igual ao `sched_prio_to_weight[20]` do
/// Linux. Os demais pesos vem da mesma tabela.
const NICE0_WEIGHT: u64 = 1024;
/// Fator de escala do `vruntime`. A cada tick a tarefa corrente soma
/// `(NICE0_WEIGHT << VRUNTIME_SHIFT) / weight`; com `nice` 0 isso vale 1024.
const VRUNTIME_SHIFT: u32 = 10;
/// Limite inferior de `nice` (maior prioridade).
pub const NICE_MIN: i8 = -20;
/// Limite superior de `nice` (menor prioridade).
pub const NICE_MAX: i8 = 19;

/// Quantidade de sinais suportados (1..=31, como nos sinais classicos Unix).
pub const SIGNAL_COUNT: usize = 32;

pub const SIGHUP: u8 = 1;
pub const SIGINT: u8 = 2;
pub const SIGQUIT: u8 = 3;
pub const SIGKILL: u8 = 9;
pub const SIGUSR1: u8 = 10;
pub const SIGUSR2: u8 = 12;
pub const SIGALRM: u8 = 14;
pub const SIGTERM: u8 = 15;
pub const SIGCHLD: u8 = 17;
pub const SIGCONT: u8 = 18;
pub const SIGSTOP: u8 = 19;

/// Acao associada a um sinal, no estilo de `struct sigaction`.
#[derive(Clone, Copy)]
pub enum SignalAction {
    /// Acao padrao do Unix (terminar, parar, continuar ou ignorar).
    Default,
    /// Descarta o sinal explicitamente (`SIG_IGN`).
    Ignore,
    /// Executa um handler de kernel no ponto seguro do proprio processo.
    Handler(fn(i32)),
}

impl SignalAction {
    fn is_default(self) -> bool {
        matches!(self, SignalAction::Default)
    }
}

/// Mascara de bits correspondente ao sinal `sig` (1..=31).
fn signal_mask(sig: u8) -> u32 {
    1u32 << (sig - 1)
}

/// Nome canonico (`SIGTERM`, `SIGUSR1`, ...) usado pelos applets e `/proc`.
pub fn signal_name(sig: u8) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        16 => "SIGSTKFLT",
        17 => "SIGCHLD",
        18 => "SIGCONT",
        19 => "SIGSTOP",
        20 => "SIGTSTP",
        21 => "SIGTTIN",
        22 => "SIGTTOU",
        23 => "SIGURG",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        26 => "SIGVTALRM",
        27 => "SIGPROF",
        28 => "SIGWINCH",
        29 => "SIGIO",
        30 => "SIGPWR",
        31 => "SIGSYS",
        _ => "SIG?",
    }
}

/// Converte um nome (`TERM`, `SIGTERM`, `term`, ...) ou um numero decimal em
/// um numero de sinal. `0` e aceito porque `kill(pid, 0)` apenas verifica a
/// existencia da tarefa.
pub fn parse_signal(arg: &[u8]) -> Option<u8> {
    if arg.is_empty() {
        return None;
    }
    if arg.iter().all(|byte| byte.is_ascii_digit()) {
        let mut value = 0u8;
        for &byte in arg {
            value = value.checked_mul(10)?.checked_add(byte - b'0')?;
        }
        return if value <= 31 { Some(value) } else { None };
    }

    let name = if arg.len() > 3 && arg[..3].eq_ignore_ascii_case(b"SIG") {
        &arg[3..]
    } else {
        arg
    };
    for sig in 1..=31u8 {
        let canonical = signal_name(sig);
        if canonical.as_bytes()[3..].eq_ignore_ascii_case(name) {
            return Some(sig);
        }
    }
    None
}

/// Sinais cuja acao padrao e parar o processo.
fn default_stops(sig: u8) -> bool {
    matches!(sig, SIGSTOP | 20 | 21 | 22) // TSTP, TTIN, TTOU
}

/// Sinais cuja acao padrao e terminar o processo.
fn default_terminates(sig: u8) -> bool {
    match sig {
        SIGCONT | SIGCHLD | 23 | 28 => false, // continuar ou ignorar
        SIGSTOP | 20 | 21 | 22 => false,      // TSTP, TTIN, TTOU
        SIGHUP | SIGINT | SIGQUIT | SIGKILL | SIGUSR1 | SIGUSR2 | SIGALRM | SIGTERM => true,
        // Os demais sinais classicos tambem terminam por padrao.
        _ => true,
    }
}

/// `sched_prio_to_weight` do Linux para `nice` -20..=19. Peso maior significa
/// mais CPU: cada tarefa recebe uma fatia proporcional ao seu peso.
const NICE_WEIGHTS: [u64; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100,
    4904, 3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172,
    137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];

/// Peso associado a um `nice` (-20..=19). Valores fora da faixa sao presos
/// aos extremos, como `set_user_nice` faz no Linux.
fn weight_for_nice(nice: i8) -> u64 {
    let clamped = nice.clamp(NICE_MIN, NICE_MAX);
    let index = (clamped as i16 + 20) as usize;
    NICE_WEIGHTS[index]
}

/// Quanto de `vruntime` a tarefa acumula ao executar um tick.
fn vruntime_delta(weight: u64) -> u64 {
    (NICE0_WEIGHT << VRUNTIME_SHIFT) / weight.max(1)
}

const NAME_MAX: usize = 24;

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Free,
    Ready,
    Running,
    /// Esperando um prazo do escalonador (equivalente a `TASK_INTERRUPTIBLE`
    /// com `schedule_timeout`). Nao consome CPU e nao e escolhida por
    /// `pick_ready` ate o tick de despertar.
    Blocked,
    /// Parada por `SIGSTOP`/`SIGTSTP`; volta apenas com `SIGCONT`.
    Stopped,
    Exited,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Free => "livre",
            State::Ready => "pronta",
            State::Running => "rodando",
            State::Blocked => "dormindo",
            State::Stopped => "parada",
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
    /// Ticks de CPU realmente executados pela tarefa (independe do `nice`).
    cpu_ticks: u64,
    /// Prioridade no estilo Unix (`-20` = mais CPU, `19` = menos CPU).
    nice: i8,
    /// Tempo virtual de execucao; a tarefa de menor `vruntime` e escolhida.
    vruntime: u64,
    /// Tick absoluto do escalonador em que uma tarefa bloqueada deve acordar.
    /// `u64::MAX` significa "somente por evento" (`wait_event`).
    wake_at: u64,
    /// Endereco da fila de espera em que a tarefa esta bloqueada; 0 quando o
    /// bloqueio e apenas por prazo (`sleep`).
    wait_queue: u64,
    /// Sinais entregues mas ainda nao tratados (mascara de 1..=31).
    pending: u32,
    /// Sinais mascarados pelo processo (`sigprocmask`).
    blocked: u32,
    /// Disposicao de cada sinal (`sigaction`). O indice e o proprio numero.
    actions: [SignalAction; SIGNAL_COUNT],
    /// Falso enquanto o handler de um sinal esta rodando; impede reentrada e
    /// entregas aninhadas no mesmo processo.
    in_handler: bool,
    /// Status passado para `exit` e recuperado pelo `wait` do pai.
    exit_status: i32,
    /// Sinal que terminou a tarefa (0 se saiu normalmente).
    exit_signal: u8,
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
            cpu_ticks: 0,
            nice: 0,
            vruntime: 0,
            wake_at: 0,
            wait_queue: 0,
            pending: 0,
            blocked: 0,
            actions: [SignalAction::Default; SIGNAL_COUNT],
            in_handler: false,
            exit_status: 0,
            exit_signal: 0,
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
    /// Relogio do escalonador, incrementado a cada IRQ0.
    ticks: u64,
}

static TABLE: SpinLock<Table> = SpinLock::new(Table {
    tasks: [Task::free(); MAX_TASKS],
    current: 0,
    next_pid: 1,
    spawned: 0,
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
    table.ticks = 0;
    SCHED_READY.store(true, Ordering::Release);
}

/// Cria uma tarefa pronta para rodar com a prioridade `nice` indicada. A pilha
/// vem do heap do kernel.
///
/// Apenas slots livres sao reaproveitados: uma tarefa encerrada permanece como
/// zumbi (`Exited`) ate o pai chamar `wait`, como no Linux. O `wait` libera o
/// slot e a proxima `spawn` devolve a pilha antiga ao heap.
pub fn spawn(name: &[u8], entry: fn(), nice: i8) -> Result<u64, &'static str> {
    let layout = kernel_stack_layout();

    let mut table = TABLE.lock();
    let slot = table
        .tasks
        .iter()
        .position(|task| task.state == State::Free)
        .ok_or("tabela de tarefas cheia (chame wait para colher zumbis)")?;

    // Libera a pilha antiga, se o slot era de uma tarefa ja colhida.
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
    // Tarefas novas entram na disputa com o menor `vruntime` atual, e nao com
    // zero: assim uma tarefa recem-criada nao monopoliza a CPU ate "alcancar"
    // as antigas (o mesmo espirito do `place_entity` do CFS).
    let floor = min_vruntime_locked(&table);

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
    task.cpu_ticks = 0;
    task.nice = nice.clamp(NICE_MIN, NICE_MAX);
    task.vruntime = floor;
    task.wake_at = 0;
    task.wait_queue = 0;
    task.pending = 0;
    task.blocked = 0;
    task.actions = [SignalAction::Default; SIGNAL_COUNT];
    task.in_handler = false;
    task.exit_status = 0;
    task.exit_signal = 0;
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

/// Menor `vruntime` entre as tarefas prontas ou rodando. Usado para inserir
/// tarefas novas e acordadas no mesmo patamar das demais.
fn min_vruntime_locked(table: &Table) -> u64 {
    let mut min = u64::MAX;
    for task in table.tasks.iter() {
        if task.state == State::Ready || task.state == State::Running {
            min = min.min(task.vruntime);
        }
    }
    if min == u64::MAX {
        0
    } else {
        min
    }
}

/// Escolhe a tarefa `Ready` de menor `vruntime`. Usado quando a tarefa corrente
/// esta prestes a bloquear ou encerrar e, portanto, nao deve ser escolhida.
fn pick_ready(table: &Table) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_vruntime = u64::MAX;
    for (index, task) in table.tasks.iter().enumerate() {
        if task.state != State::Ready {
            continue;
        }
        if best.is_none() || task.vruntime < best_vruntime {
            best_vruntime = task.vruntime;
            best = Some(index);
        }
    }
    best
}

/// Escolhe a tarefa de menor `vruntime` entre as que podem rodar. O empate
/// fica com a tarefa corrente quando ela tambem e elegivel, evitando uma troca
/// de contexto desnecessaria.
fn pick_next(table: &Table) -> Option<usize> {
    let current = table.current;
    let mut best: Option<usize> = None;
    let mut best_vruntime = u64::MAX;
    for (index, task) in table.tasks.iter().enumerate() {
        if task.state != State::Ready && task.state != State::Running {
            continue;
        }
        let tie = task.vruntime == best_vruntime && index == current;
        if task.vruntime < best_vruntime || tie || best.is_none() {
            best_vruntime = task.vruntime;
            best = Some(index);
        }
    }
    best
}

/// Cede a CPU para a tarefa pronta de menor `vruntime`. Se a propria tarefa
/// corrente ainda for a mais "atrasada", ela continua rodando (como no CFS).
pub fn yield_now() {
    // Seguro para entrega de sinais: `check_signals` pode executar um handler,
    // parar ou encerrar a tarefa antes de ela ceder a CPU.
    check_signals();

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

        let next = match pick_next(&table) {
            Some(index) => index,
            // Sem outra tarefa pronta: continua a atual.
            None => current,
        };
        table.tasks[next].state = State::Running;
        table.current = next;

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
        // Um sinal pendente acorda a espera: ele e entregue antes de a tarefa
        // voltar a dormir, como em `wait_event_interruptible`.
        check_signals();

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
                match pick_ready(&table) {
                    Some(next) => {
                        table.tasks[current].state = State::Blocked;
                        table.tasks[current].wake_at = deadline;
                        table.tasks[current].wait_queue = 0;
                        table.tasks[next].state = State::Running;
                        table.current = next;
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
                // de girar consumindo CPU.
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
        // Mesmo protocolo de `sleep`: um sinal pendente pode interromper a
        // espera e ser entregue antes de voltar ao bloqueio.
        check_signals();

        // Desliga IRQs durante a contabilidade e a
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
                match pick_ready(&table) {
                    Some(next) => {
                        let task = &mut table.tasks[current];
                        task.state = State::Blocked;
                        task.wait_queue = wait_queue;
                        task.wake_at = deadline;
                        table.tasks[next].state = State::Running;
                        table.current = next;
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

/// Fila em que o pai dorme esperando um filho encerrar (`wait`). A identidade
/// da fila e global; cada pai reavalia a propria condicao ao acordar.
static CHILD_QUEUE: WaitQueue = WaitQueue::new();

/// Marca como prontas as tarefas bloqueadas em `wait_queue`. Nao troca de
/// contexto: apenas torna a tarefa elegivel, como `wake_up` no Linux.
fn wake(wait_queue: u64, only_one: bool) -> usize {
    let flags = crate::arch::irq_save();
    let mut woken = 0;
    {
        let mut table = TABLE.lock();
        // Mesmo tratamento de `place_entity`: a tarefa acordada nao volta com
        // `vruntime` atrasado, senao monopolizaria a CPU por muito tempo.
        let floor = min_vruntime_locked(&table);
        for task in table.tasks.iter_mut() {
            if task.state == State::Blocked && task.wait_queue == wait_queue {
                task.state = State::Ready;
                task.vruntime = task.vruntime.max(floor);
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

/// Chamado a cada tick do timer. Contabiliza `vruntime` da tarefa corrente,
/// acorda tarefas com prazo vencido e preempta se houver outra tarefa pronta
/// com `vruntime` menor.
pub fn on_tick() {
    if !SCHED_READY.load(Ordering::Acquire) {
        return;
    }

    let user_active = crate::user::is_active();

    let (old_rsp, new_rsp, switched) = {
        let mut table = TABLE.lock();
        table.ticks = table.ticks.wrapping_add(1);
        let now = table.ticks;

        // A tarefa que consumiu o ultimo tick acumula tempo virtual de acordo
        // com o proprio peso: quanto maior o peso, menor o incremento.
        let current = table.current;
        let task = &mut table.tasks[current];
        if task.state == State::Running && task.pid != 0 {
            task.cpu_ticks = task.cpu_ticks.wrapping_add(1);
            let delta = vruntime_delta(weight_for_nice(task.nice));
            task.vruntime = task.vruntime.saturating_add(delta);
        }

        // Tarefas com prazo vencido voltam como prontas e entram na disputa
        // pelo menor `vruntime` (nunca atrasadas em relacao as demais).
        let floor = min_vruntime_locked(&table);
        for task in table.tasks.iter_mut() {
            if task.state == State::Blocked && task.wake_at <= now {
                task.state = State::Ready;
                task.vruntime = task.vruntime.max(floor);
                task.wait_queue = 0;
            }
        }

        // Durante um programa de ring 3 a pilha de kernel e a pilha
        // compartilhada de interrupcao do TSS, nao a pilha de uma tarefa; nao
        // preemptamos, mas o relogio e o `vruntime` continuam correndo.
        if user_active {
            (core::ptr::null_mut::<u64>(), 0, false)
        } else {
            let current = table.current;
            match pick_next(&table) {
                Some(next) if next != current => {
                    if table.tasks[current].state == State::Running {
                        table.tasks[current].state = State::Ready;
                    }
                    table.tasks[current].preemptions += 1;
                    table.tasks[next].state = State::Running;
                    table.current = next;
                    (
                        core::ptr::addr_of_mut!(table.tasks[current].rsp),
                        table.tasks[next].rsp,
                        true,
                    )
                }
                _ => {
                    if table.tasks[current].state == State::Ready {
                        table.tasks[current].state = State::Running;
                    }
                    (core::ptr::null_mut::<u64>(), 0, false)
                }
            }
        }
    };

    if switched {
        unsafe { task_switch(old_rsp, new_rsp) };
    }
}

/// Marca a tarefa `index` como encerrada, guarda o status e entrega `SIGCHLD`
/// ao pai. Deve ser chamada com a tabela travada.
fn terminate_locked(table: &mut Table, index: usize, status: i32, signal: u8) {
    let ppid = table.tasks[index].ppid;
    let floor = min_vruntime_locked(table);
    let task = &mut table.tasks[index];
    task.state = State::Exited;
    task.exit_status = status;
    task.exit_signal = signal;
    task.pending = 0;
    task.blocked = 0;
    task.wait_queue = 0;
    task.wake_at = 0;

    if ppid != 0 {
        if let Some(parent) = table
            .tasks
            .iter_mut()
            .find(|parent| parent.pid == ppid && parent.state != State::Free)
        {
            parent.pending |= signal_mask(SIGCHLD);
            // Um pai bloqueado em `wait` volta a rodar para colher o filho.
            if parent.state == State::Blocked {
                parent.state = State::Ready;
                parent.vruntime = parent.vruntime.max(floor);
                parent.wait_queue = 0;
                parent.wake_at = 0;
            }
        }
    }
}

/// Para a tarefa corrente (`SIGSTOP`). Quando ha outra tarefa pronta, troca de
/// contexto e so retorna depois de um `SIGCONT`. Sem outra tarefa elegivel, a
/// parada e adiada para nao deixar a CPU sem um contexto valido.
fn stop_current() {
    let flags = crate::arch::irq_save();
    let (old_rsp, new_rsp, switched) = {
        let mut table = TABLE.lock();
        let current = table.current;
        match pick_ready(&table) {
            Some(next) => {
                let task = &mut table.tasks[current];
                task.state = State::Stopped;
                task.wait_queue = 0;
                task.wake_at = 0;
                table.tasks[next].state = State::Running;
                table.current = next;
                (
                    core::ptr::addr_of_mut!(table.tasks[current].rsp),
                    table.tasks[next].rsp,
                    true,
                )
            }
            None => (core::ptr::null_mut::<u64>(), 0, false),
        }
    };

    if switched {
        unsafe { task_switch(old_rsp, new_rsp) };
    }
    crate::arch::irq_restore(flags);
}

/// Entrega os sinais pendentes que nao estao bloqueados. Deve ser chamada
/// apenas em pontos seguros (nunca dentro do tratador de IRQ), pois um handler
/// pode dormir, ceder a CPU, parar ou encerrar a tarefa.
pub fn check_signals() {
    loop {
        let (sig, action, current) = {
            let mut table = TABLE.lock();
            let current = table.current;
            let task = &mut table.tasks[current];
            if task.in_handler {
                return;
            }
            // Bits 0..=30 correspondem aos sinais 1..=31.
            let ready = task.pending & !task.blocked & 0x7FFF_FFFF;
            if ready == 0 {
                return;
            }
            let sig = ready.trailing_zeros() as u8 + 1;
            task.pending &= !signal_mask(sig);
            (sig, task.actions[sig as usize], current)
        };

        match action {
            SignalAction::Default => {
                if sig == SIGCONT {
                    // Continuar uma tarefa que ja estava rodando e no-op.
                } else if default_stops(sig) {
                    stop_current();
                } else if default_terminates(sig) {
                    exit_current_with_signal(128 + sig as i32, sig);
                }
            }
            SignalAction::Ignore => {}
            SignalAction::Handler(handler) => {
                {
                    let mut table = TABLE.lock();
                    if let Some(task) = table.tasks.get_mut(current) {
                        task.in_handler = true;
                    }
                }
                handler(sig as i32);
                {
                    let mut table = TABLE.lock();
                    if let Some(task) = table.tasks.get_mut(current) {
                        task.in_handler = false;
                    }
                }
            }
        }
    }
}

/// Encerra a tarefa corrente com um status de saida proprio (`exit(status)`).
pub fn exit_current_with_status(status: i32) -> ! {
    exit_current_with_signal(status, 0)
}

/// Encerra a tarefa corrente registrando tambem o sinal fatal, se houver.
fn exit_current_with_signal(status: i32, signal: u8) -> ! {
    let flags = crate::arch::irq_save();

    let (old_rsp, new_rsp, halt) = {
        let mut table = TABLE.lock();
        let current = table.current;
        terminate_locked(&mut table, current, status, signal);

        // Prefere a tarefa pronta de menor `vruntime`; caso contrario volta
        // para o shell (PID 1), que nunca encerra por sinal.
        let next = match pick_ready(&table) {
            Some(index) => index,
            None if current != 0 => 0,
            None => current,
        };

        if next == current {
            (core::ptr::null_mut::<u64>(), 0, true)
        } else {
            table.tasks[next].state = State::Running;
            table.current = next;
            (
                core::ptr::addr_of_mut!(table.tasks[current].rsp),
                table.tasks[next].rsp,
                false,
            )
        }
    };

    // O filho pode ter acordado o pai em `wait`, mesmo sem trocar para ele
    // agora: a fila e sinalizada fora da trava.
    CHILD_QUEUE.wake_up_all();

    if halt {
        crate::arch::irq_restore(flags);
        crate::arch::halt_loop()
    }

    unsafe { task_switch(old_rsp, new_rsp) };

    // Uma tarefa encerrada nunca deve ser escalonada de novo.
    crate::arch::irq_restore(flags);
    crate::arch::halt_loop()
}

/// Termina a tarefa atual e nunca retorna.
pub fn exit_current() -> ! {
    exit_current_with_status(0)
}

/// Envia `sig` para `pid`, como `kill(2)`. `sig == 0` apenas verifica se a
/// tarefa existe.
///
/// Sinais com acao padrao sao aplicados imediatamente quando o alvo nao e a
/// tarefa corrente (ela nao esta executando, entao nao ha contexto a salvar).
/// Sinais com handler ficam pendentes e a tarefa e acordada para executa-lo em
/// um ponto seguro. `SIGKILL` nao pode ser bloqueado, ignorado ou tratado.
pub fn kill(pid: u64, sig: u8) -> Result<(), &'static str> {
    if sig > 31 {
        return Err("sinal invalido");
    }

    let mut wake_children = false;

    {
        let mut table = TABLE.lock();
        let index = match table
            .tasks
            .iter()
            .position(|task| task.pid == pid && task.state != State::Free)
        {
            Some(index) => index,
            None => return Err("tarefa nao encontrada"),
        };

        if sig == 0 {
            return Ok(());
        }
        // Um zumbi ainda existe para efeito de `kill`, mas nao executa mais
        // nada: o sinal e simplesmente descartado, como no Linux.
        if table.tasks[index].state == State::Exited {
            return Ok(());
        }
        // PID 1 e o "init": ignora sinais cuja acao padrao mataria ou pararia
        // o sistema inteiro, como faz o Linux com o processo 1.
        if pid == 1 && (default_terminates(sig) || default_stops(sig)) {
            return Ok(());
        }

        if index == table.current {
            // A propria tarefa entrega o sinal no proximo ponto seguro.
            table.tasks[index].pending |= signal_mask(sig);
            return Ok(());
        }

        if sig == SIGKILL {
            terminate_locked(&mut table, index, 128 + sig as i32, sig);
            wake_children = true;
        } else if sig == SIGCONT {
            let floor = min_vruntime_locked(&table);
            let action = table.tasks[index].actions[sig as usize];
            let task = &mut table.tasks[index];
            // SIGCONT sempre tira a tarefa do estado parado, mesmo que o
            // sinal esteja bloqueado.
            if task.state == State::Stopped {
                task.state = State::Ready;
                task.vruntime = task.vruntime.max(floor);
                task.wait_queue = 0;
                task.wake_at = 0;
            }
            if !action.is_default() {
                task.pending |= signal_mask(sig);
            }
        } else {
            let action = table.tasks[index].actions[sig as usize];
            let blocked = table.tasks[index].blocked & signal_mask(sig) != 0;
            let floor = min_vruntime_locked(&table);
            match action {
                SignalAction::Default => {
                    if blocked {
                        table.tasks[index].pending |= signal_mask(sig);
                    } else if default_terminates(sig) {
                        terminate_locked(&mut table, index, 128 + sig as i32, sig);
                        wake_children = true;
                    } else if default_stops(sig) {
                        let task = &mut table.tasks[index];
                        task.state = State::Stopped;
                        task.wait_queue = 0;
                        task.wake_at = 0;
                    }
                }
                SignalAction::Ignore => {}
                SignalAction::Handler(_) => {
                    let task = &mut table.tasks[index];
                    task.pending |= signal_mask(sig);
                    if !blocked && task.state == State::Blocked {
                        task.state = State::Ready;
                        task.vruntime = task.vruntime.max(floor);
                        task.wait_queue = 0;
                        task.wake_at = 0;
                    }
                }
            }
        }
    }

    if wake_children {
        CHILD_QUEUE.wake_up_all();
    }
    Ok(())
}

/// Instala uma disposicao para `sig` em `pid` (`sigaction(2)`). Devolve a acao
/// anterior. `SIGKILL` e `SIGSTOP` nao podem ser alterados.
pub fn set_signal_action(
    pid: u64,
    sig: u8,
    action: SignalAction,
) -> Result<SignalAction, &'static str> {
    if sig == 0 || sig > 31 {
        return Err("sinal invalido");
    }
    if sig == SIGKILL || sig == SIGSTOP {
        return Err("SIGKILL/SIGSTOP nao podem ser alterados");
    }

    let mut table = TABLE.lock();
    let floor = min_vruntime_locked(&table);
    let index = match table
        .tasks
        .iter()
        .position(|task| task.pid == pid && task.state != State::Free)
    {
        Some(index) => index,
        None => return Err("tarefa nao encontrada"),
    };

    let old = table.tasks[index].actions[sig as usize];
    table.tasks[index].actions[sig as usize] = action;

    // Se ja havia um sinal pendente, acorda a tarefa para trata-lo.
    let task = &mut table.tasks[index];
    if !matches!(action, SignalAction::Ignore)
        && task.pending & signal_mask(sig) != 0
        && task.state == State::Blocked
    {
        task.state = State::Ready;
        task.vruntime = task.vruntime.max(floor);
        task.wait_queue = 0;
        task.wake_at = 0;
    }
    Ok(old)
}

/// Disposicao atual de `sig` para `pid`, sem alterar nada.
pub fn signal_action(pid: u64, sig: u8) -> Option<SignalAction> {
    if sig == 0 || sig > 31 {
        return None;
    }
    let table = TABLE.lock();
    table
        .tasks
        .iter()
        .find(|task| task.pid == pid && task.state != State::Free)
        .map(|task| task.actions[sig as usize])
}

/// Bloqueia ou desbloqueia `sig` em `pid` (`sigprocmask`). Devolve a mascara
/// de bloqueio resultante. `SIGKILL` e `SIGSTOP` nao podem ser bloqueados.
pub fn set_signal_blocked(pid: u64, sig: u8, block: bool) -> Result<u32, &'static str> {
    if sig == 0 || sig > 31 {
        return Err("sinal invalido");
    }
    if sig == SIGKILL || sig == SIGSTOP {
        return Err("SIGKILL/SIGSTOP nao podem ser bloqueados");
    }

    let mut table = TABLE.lock();
    let floor = min_vruntime_locked(&table);
    let index = match table
        .tasks
        .iter()
        .position(|task| task.pid == pid && task.state != State::Free)
    {
        Some(index) => index,
        None => return Err("tarefa nao encontrada"),
    };

    let mask = signal_mask(sig);
    let task = &mut table.tasks[index];
    if block {
        task.blocked |= mask;
    } else {
        task.blocked &= !mask;
        // Desbloquear um sinal ja pendente volta a acordar a tarefa.
        if task.pending & mask != 0 && task.state == State::Blocked {
            task.state = State::Ready;
            task.vruntime = task.vruntime.max(floor);
            task.wait_queue = 0;
            task.wake_at = 0;
        }
    }
    Ok(task.blocked)
}

/// Resultado de `wait`: PID colhido, status de saida e sinal fatal (0 se o
/// encerramento foi normal).
#[derive(Clone, Copy)]
pub struct WaitResult {
    pub pid: u64,
    pub status: i32,
    pub signal: u8,
}

fn has_zombie_child(parent: u64, target: Option<u64>) -> bool {
    let table = TABLE.lock();
    table.tasks.iter().any(|task| {
        task.ppid == parent
            && task.state == State::Exited
            && target.map_or(true, |pid| pid == task.pid)
    })
}

/// Espera um filho encerrar e colhe o status (`wait`/`waitpid`). Sem `target`,
/// qualquer filho zumbi serve. O slot do filho e liberado para reutilizacao,
/// mas a pilha so e devolvida quando `spawn` reaproveita o slot.
pub fn wait_child(target: Option<u64>) -> Result<WaitResult, &'static str> {
    let parent = current_pid();

    loop {
        {
            let mut table = TABLE.lock();
            let index = table.tasks.iter().position(|task| {
                task.ppid == parent
                    && task.state == State::Exited
                    && target.map_or(true, |pid| pid == task.pid)
            });

            if let Some(index) = index {
                let task = &mut table.tasks[index];
                let result = WaitResult {
                    pid: task.pid,
                    status: task.exit_status,
                    signal: task.exit_signal,
                };
                task.state = State::Free;
                task.pid = 0;
                task.ppid = 0;
                task.pending = 0;
                task.blocked = 0;
                task.exit_status = 0;
                task.exit_signal = 0;
                return Ok(result);
            }

            let mut any_child = false;
            let mut target_found = false;
            for task in table.tasks.iter() {
                if task.ppid == parent && task.state != State::Free {
                    any_child = true;
                    if target.map_or(false, |pid| pid == task.pid) {
                        target_found = true;
                    }
                }
            }
            if !any_child {
                return Err("nenhum filho");
            }
            if target.is_some() && !target_found {
                return Err("filho nao encontrado");
            }
        }

        // O despertar normal vem de `CHILD_QUEUE`; o timeout cobre a janela
        // entre verificar a condicao e dormir, sem perder o evento.
        CHILD_QUEUE.wait_event_timeout(10, || has_zombie_child(parent, target));
        check_signals();
    }
}

/// Ponto de entrada ocioso do shell. Bloqueia por um tick, deixando o timer
/// acordar tarefas prontas; sem outra tarefa, o `hlt` de `sleep` evita
/// *busy-wait* e o shell volta a pollar teclado/serial a cada ~10 ms.
pub fn idle() {
    sleep(1);
}

/// Retrato de uma tarefa para `ps` e `/proc/ps`.
#[derive(Clone, Copy)]
pub struct TaskInfo<'a> {
    pub pid: u64,
    pub ppid: u64,
    pub state: State,
    pub name: &'a str,
    pub yields: u64,
    pub preemptions: u64,
    pub cpu_ticks: u64,
    pub nice: i8,
    pub vruntime: u64,
    pub current: bool,
    pub pending: u32,
    pub blocked: u32,
    pub exit_status: i32,
    pub exit_signal: u8,
}

/// Visita cada tarefa ocupando um slot da tabela.
pub fn for_each(mut visit: impl for<'a> FnMut(TaskInfo<'a>)) {
    let table = TABLE.lock();
    for (index, task) in table.tasks.iter().enumerate() {
        if task.state == State::Free {
            continue;
        }
        visit(TaskInfo {
            pid: task.pid,
            ppid: task.ppid,
            state: task.state,
            name: task.name(),
            yields: task.yields,
            preemptions: task.preemptions,
            cpu_ticks: task.cpu_ticks,
            nice: task.nice,
            vruntime: task.vruntime,
            current: index == table.current,
            pending: task.pending,
            blocked: task.blocked,
            exit_status: task.exit_status,
            exit_signal: task.exit_signal,
        });
    }
}

/// Ajusta a prioridade `nice` de uma tarefa, equivalente a `setpriority` do
/// Linux. O valor e preso a `NICE_MIN..=NICE_MAX`.
pub fn set_nice(pid: u64, nice: i8) -> Result<i8, &'static str> {
    let mut table = TABLE.lock();
    for task in table.tasks.iter_mut() {
        if task.pid == pid && task.state != State::Free {
            task.nice = nice.clamp(NICE_MIN, NICE_MAX);
            return Ok(task.nice);
        }
    }
    Err("tarefa nao encontrada")
}

/// Prioridade `nice` de uma tarefa, se ela existir.
pub fn get_nice(pid: u64) -> Option<i8> {
    let table = TABLE.lock();
    table
        .tasks
        .iter()
        .find(|task| task.pid == pid && task.state != State::Free)
        .map(|task| task.nice)
}

/// Prioridade `nice` da tarefa corrente.
pub fn current_nice() -> i8 {
    let table = TABLE.lock();
    table.tasks[table.current].nice
}

/// Ticks de CPU ja executados pela tarefa corrente. Independe do `nice`: serve
/// para medir trabalho real e comparar a fatia de CPU entre tarefas.
pub fn current_cpu_ticks() -> u64 {
    let table = TABLE.lock();
    table.tasks[table.current].cpu_ticks
}

/// Resumo numerico para `info`/`mem`.
#[derive(Clone, Copy)]
pub struct Stats {
    pub total: usize,
    pub ready: usize,
    pub running: usize,
    pub blocked: usize,
    pub stopped: usize,
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
        stopped: 0,
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
            State::Stopped => stats.stopped += 1,
            State::Exited => stats.exited += 1,
        }
        stats.preemptions += task.preemptions;
        stats.total += 1;
    }
    stats
}
