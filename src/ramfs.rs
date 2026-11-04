//! Sistema de arquivos em memoria (ramfs) sem heap.
//!
//! A arvore e formada por nos de tamanho fixo. Cada no pode ser arquivo ou
//! diretorio; diretorios guardam os filhos em uma lista encadeada por
//! `first_child`/`next_sibling`. Nao ha alocacao dinamica: todos os nos vivem
//! em um array estatico protegido por um spinlock.
//!
//! O ramfs e didatico: nomes tem no maximo 31 bytes, arquivos cabem em 1024
//! bytes e a arvore inteira tem 48 nos. Nao ha permissoes, timestamps,
//! links simbolicos nem persistencia.

use crate::sync::{SpinLock, SpinLockGuard};

/// Tamanho maximo de um nome de arquivo ou diretorio.
pub const NAME_MAX: usize = 32;
/// Tamanho maximo do conteudo de um arquivo.
pub const DATA_MAX: usize = 1024;
/// Numero maximo de nos (arquivos + diretorios), incluindo a raiz.
pub const MAX_NODES: usize = 48;
/// Indice invalido, usado para encerrar as listas encadeadas.
pub const NO_NODE: usize = usize::MAX;

/// Um no do ramfs: arquivo ou diretorio.
#[derive(Clone, Copy)]
pub struct Node {
    used: bool,
    is_dir: bool,
    name: [u8; NAME_MAX],
    name_len: usize,
    parent: usize,
    first_child: usize,
    next_sibling: usize,
    data: [u8; DATA_MAX],
    len: usize,
}

impl Node {
    const fn empty() -> Self {
        Self {
            used: false,
            is_dir: false,
            name: [0; NAME_MAX],
            name_len: 0,
            parent: NO_NODE,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
            data: [0; DATA_MAX],
            len: 0,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    /// Tamanho do conteudo (zero para diretorios).
    pub fn size(&self) -> usize {
        self.len
    }

    pub fn first_child(&self) -> usize {
        self.first_child
    }

    pub fn next_sibling(&self) -> usize {
        self.next_sibling
    }
}

/// Estado do ramfs e diretorio corrente.
pub struct Fs {
    nodes: [Node; MAX_NODES],
    cwd: usize,
}

static FS: SpinLock<Fs> = SpinLock::new(Fs::new());

/// Inicializa a arvore e cria alguns arquivos de demonstracao.
pub fn init() {
    FS.lock().reset();
}

/// Adquire o ramfs. O chamador deve soltar o guarda antes de operacoes lentas
/// de console, quando possivel.
pub fn lock() -> SpinLockGuard<'static, Fs> {
    FS.lock()
}

impl Fs {
    const fn new() -> Self {
        Self {
            nodes: [Node::empty(); MAX_NODES],
            cwd: 0,
        }
    }

    fn reset(&mut self) {
        for node in self.nodes.iter_mut() {
            *node = Node::empty();
        }

        self.nodes[0].used = true;
        self.nodes[0].is_dir = true;
        self.cwd = 0;
        self.seed();
        self.cwd = 0;
    }

    fn seed(&mut self) {
        let _ = self.create(b"/etc", true);
        let _ = self.create(b"/home", true);
        let _ = self.create(b"/home/root", true);
        let _ = self.create(b"/proc", true);
        let _ = self.create(b"/README", false);
        let _ = self.write(
            b"/README",
            b"ramfs do rinux: arquivos em memoria, sem heap.\n\
              Use ls, cd, cat, write, mkdir, cp, mv e tree.\n",
            false,
        );
        let _ = self.create(b"/etc/motd", false);
        let _ = self.write(b"/etc/motd", b"Bem-vindo ao rinux!\n", false);
        let _ = self.create(b"/home/root/notes.txt", false);
        let _ = self.write(
            b"/home/root/notes.txt",
            b"O shell agora tem applets no estilo BusyBox.\n",
            false,
        );
    }

    pub fn cwd(&self) -> usize {
        self.cwd
    }

    pub fn set_cwd(&mut self, index: usize) {
        self.cwd = index;
    }

    pub fn node(&self, index: usize) -> &Node {
        &self.nodes[index]
    }

    /// Conta nos usados e bytes armazenados em arquivos.
    pub fn usage(&self) -> (usize, usize) {
        let mut nodes = 0;
        let mut bytes = 0;
        for node in &self.nodes {
            if node.used {
                nodes += 1;
                if !node.is_dir {
                    bytes += node.len;
                }
            }
        }
        (nodes, bytes)
    }

    /// Resolve um caminho absoluto ou relativo ao diretorio corrente.
    ///
    /// Componentes vazios e `.` sao ignorados; `..` sobe para o pai. O caminho
    /// final precisa existir.
    pub fn resolve(&self, path: &[u8]) -> Result<usize, &'static str> {
        let mut current = if path.first() == Some(&b'/') { 0 } else { self.cwd };
        let mut index = 0;

        while index < path.len() {
            while index < path.len() && path[index] == b'/' {
                index += 1;
            }
            if index >= path.len() {
                break;
            }

            let start = index;
            while index < path.len() && path[index] != b'/' {
                index += 1;
            }
            let component = &path[start..index];

            if component == b"." {
                continue;
            }
            if component == b".." {
                let parent = self.nodes[current].parent;
                current = if parent == NO_NODE { 0 } else { parent };
                continue;
            }

            if !self.nodes[current].is_dir {
                return Err("nao e um diretorio");
            }
            current = self.find_child(current, component).ok_or("arquivo nao encontrado")?;
        }

        if path.last() == Some(&b'/') && !self.nodes[current].is_dir {
            return Err("nao e um diretorio");
        }

        Ok(current)
    }

    fn find_child(&self, dir: usize, name: &[u8]) -> Option<usize> {
        if !self.nodes[dir].is_dir {
            return None;
        }

        let mut child = self.nodes[dir].first_child;
        while child != NO_NODE {
            if self.nodes[child].name() == name {
                return Some(child);
            }
            child = self.nodes[child].next_sibling;
        }
        None
    }

    fn alloc(&mut self) -> Option<usize> {
        (1..MAX_NODES).find(|&index| !self.nodes[index].used)
    }

    /// Descobre o diretorio pai e o nome final de um caminho.
    fn split_parent<'p>(&self, path: &'p [u8]) -> Result<(usize, &'p [u8]), &'static str> {
        if path.is_empty() {
            return Err("caminho vazio");
        }

        let last = match path.iter().rposition(|&byte| byte == b'/') {
            Some(index) => index,
            None => {
                if path == b"." || path == b".." {
                    return Err("nome invalido");
                }
                return Ok((self.cwd, path));
            }
        };

        let name = &path[last + 1..];
        if name.is_empty() || name == b"." || name == b".." {
            return Err("caminho invalido");
        }
        if name.len() > NAME_MAX {
            return Err("nome longo");
        }

        let parent_path = if last == 0 { b"/" as &[u8] } else { &path[..last] };
        let parent = self.resolve(parent_path)?;
        if !self.nodes[parent].is_dir {
            return Err("nao e um diretorio");
        }

        Ok((parent, name))
    }

    /// Cria um arquivo ou diretorio vazio. O pai precisa existir.
    pub fn create(&mut self, path: &[u8], is_dir: bool) -> Result<usize, &'static str> {
        let (parent, name) = self.split_parent(path)?;
        if self.find_child(parent, name).is_some() {
            return Err("ja existe");
        }

        let index = self.alloc().ok_or("sem espaco")?;
        let first_child = self.nodes[parent].first_child;
        {
            let node = &mut self.nodes[index];
            *node = Node::empty();
            node.used = true;
            node.is_dir = is_dir;
            node.name[..name.len()].copy_from_slice(name);
            node.name_len = name.len();
            node.parent = parent;
            node.next_sibling = first_child;
        }
        self.nodes[parent].first_child = index;
        Ok(index)
    }

    /// Escreve ou anexa dados em um arquivo, criando-o se necessario.
    pub fn write(
        &mut self,
        path: &[u8],
        data: &[u8],
        append: bool,
    ) -> Result<usize, &'static str> {
        let index = match self.resolve(path) {
            Ok(index) => index,
            Err("arquivo nao encontrado") => self.create(path, false)?,
            Err(reason) => return Err(reason),
        };

        let node = &mut self.nodes[index];
        if node.is_dir {
            return Err("e um diretorio");
        }

        let start = if append { node.len } else { 0 };
        if data.len() > DATA_MAX - start {
            return Err("arquivo cheio");
        }
        node.data[start..start + data.len()].copy_from_slice(data);
        node.len = start + data.len();
        Ok(data.len())
    }

    /// Le o conteudo de um arquivo para `out`. Devolve quantos bytes foram
    /// copiados (pode ser menor que o arquivo se `out` for pequeno).
    pub fn read(&self, path: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
        let index = self.resolve(path)?;
        self.read_at(index, 0, out)
    }

    /// Le a partir de um deslocamento, como um `read(2)` de arquivo regular.
    /// Um deslocamento no fim do arquivo devolve zero bytes.
    pub fn read_at(
        &self,
        index: usize,
        offset: usize,
        out: &mut [u8],
    ) -> Result<usize, &'static str> {
        if index >= MAX_NODES || !self.nodes[index].used {
            return Err("no invalido");
        }
        let node = &self.nodes[index];
        if node.is_dir {
            return Err("e um diretorio");
        }
        if offset >= node.len {
            return Ok(0);
        }

        let len = (node.len - offset).min(out.len());
        out[..len].copy_from_slice(&node.data[offset..offset + len]);
        Ok(len)
    }

    /// Escreve a partir de um deslocamento, sem redimensionar o arquivo para
    /// tras. Lacunas sao preenchidas com zero, como em um arquivo esparso.
    pub fn write_at(
        &mut self,
        index: usize,
        offset: usize,
        data: &[u8],
    ) -> Result<usize, &'static str> {
        if index >= MAX_NODES || !self.nodes[index].used {
            return Err("no invalido");
        }
        let node = &mut self.nodes[index];
        if node.is_dir {
            return Err("e um diretorio");
        }
        if offset > DATA_MAX || data.len() > DATA_MAX - offset {
            return Err("arquivo cheio");
        }
        if offset > node.len {
            for byte in &mut node.data[node.len..offset] {
                *byte = 0;
            }
            node.len = offset;
        }

        node.data[offset..offset + data.len()].copy_from_slice(data);
        if offset + data.len() > node.len {
            node.len = offset + data.len();
        }
        Ok(data.len())
    }

    /// Trunca um arquivo para zero bytes.
    pub fn truncate(&mut self, index: usize) -> Result<(), &'static str> {
        if index >= MAX_NODES || !self.nodes[index].used {
            return Err("no invalido");
        }
        let node = &mut self.nodes[index];
        if node.is_dir {
            return Err("e um diretorio");
        }
        node.len = 0;
        Ok(())
    }

    /// Remove um arquivo ou diretorio. Diretorios nao vazios exigem
    /// `recursive = true`.
    pub fn remove(&mut self, path: &[u8], recursive: bool) -> Result<(), &'static str> {
        let index = self.resolve(path)?;
        if index == 0 {
            return Err("nao posso remover /");
        }
        if self.nodes[index].is_dir
            && self.nodes[index].first_child != NO_NODE
            && !recursive
        {
            return Err("diretorio nao vazio");
        }

        if self.is_ancestor(index, self.cwd) {
            self.cwd = 0;
        }
        self.unlink(index);
        self.drop_subtree(index);
        Ok(())
    }

    /// Move ou renomeia um arquivo/diretorio. O destino pode sobrescrever um
    /// arquivo ou um diretorio vazio.
    pub fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), &'static str> {
        let source = self.resolve(from)?;
        if source == 0 {
            return Err("nao posso mover /");
        }

        let (new_parent, new_name) = self.split_parent(to)?;
        if self.nodes[source].is_dir && self.is_ancestor(source, new_parent) {
            return Err("nao posso mover para dentro de si mesmo");
        }

        if let Some(existing) = self.find_child(new_parent, new_name) {
            if existing == source {
                return Ok(());
            }
            if self.is_ancestor(existing, source) {
                return Err("destino invalido");
            }
            if self.nodes[existing].is_dir && self.nodes[existing].first_child != NO_NODE {
                return Err("destino nao vazio");
            }
            self.unlink(existing);
            self.drop_subtree(existing);
        }

        self.unlink(source);
        let first_child = self.nodes[new_parent].first_child;
        {
            let node = &mut self.nodes[source];
            node.parent = new_parent;
            node.name = [0; NAME_MAX];
            node.name[..new_name.len()].copy_from_slice(new_name);
            node.name_len = new_name.len();
            node.next_sibling = first_child;
        }
        self.nodes[new_parent].first_child = source;
        Ok(())
    }

    /// Monta o caminho absoluto de um no em `out`.
    pub fn path(&self, index: usize, out: &mut [u8]) -> Result<usize, &'static str> {
        if index >= MAX_NODES || !self.nodes[index].used {
            return Err("no invalido");
        }
        if index == 0 {
            if out.is_empty() {
                return Err("buffer pequeno");
            }
            out[0] = b'/';
            return Ok(1);
        }

        let mut stack = [0usize; MAX_NODES];
        let mut depth = 0;
        let mut current = index;
        while current != 0 {
            if depth >= MAX_NODES {
                return Err("caminho profundo");
            }
            stack[depth] = current;
            depth += 1;
            current = self.nodes[current].parent;
            if current == NO_NODE {
                return Err("caminho invalido");
            }
        }

        let mut written = 0;
        while depth > 0 {
            depth -= 1;
            let node = &self.nodes[stack[depth]];
            if written + 1 + node.name_len > out.len() {
                return Err("buffer pequeno");
            }
            out[written] = b'/';
            written += 1;
            out[written..written + node.name_len].copy_from_slice(node.name());
            written += node.name_len;
        }
        Ok(written)
    }

    fn unlink(&mut self, index: usize) {
        let parent = self.nodes[index].parent;
        if parent == NO_NODE {
            return;
        }

        let mut current = self.nodes[parent].first_child;
        if current == index {
            self.nodes[parent].first_child = self.nodes[index].next_sibling;
            return;
        }

        while current != NO_NODE {
            let next = self.nodes[current].next_sibling;
            if next == index {
                self.nodes[current].next_sibling = self.nodes[index].next_sibling;
                return;
            }
            current = next;
        }
    }

    fn drop_subtree(&mut self, index: usize) {
        let mut child = self.nodes[index].first_child;
        while child != NO_NODE {
            let next = self.nodes[child].next_sibling;
            self.drop_subtree(child);
            child = next;
        }
        self.nodes[index] = Node::empty();
    }

    fn is_ancestor(&self, ancestor: usize, mut node: usize) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            let parent = self.nodes[node].parent;
            if parent == NO_NODE {
                return false;
            }
            node = parent;
        }
    }
}
