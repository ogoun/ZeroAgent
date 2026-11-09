//! Граф узлов run (D2): переходы `Goto`, `Suspend`, `Done`.
//!
//! Узел получает изменяемое состояние и возвращает переход. `Goto` допустим только
//! по объявленному ребру, `Suspend` останавливает граф на прерывании, `Done`
//! завершает его. Доменных выходов в [`Exit`] нет: потребитель кладёт их в своё
//! состояние и возвращает `Done` (D31). Параллельные ветви `Fan` появятся в R2.

use std::collections::{BTreeMap, BTreeSet};

pub use za_llm::BoxFuture;

use crate::interrupt::Interrupt;

/// Имя узла; одновременно его id в рёбрах и хэше графа.
pub type NodeId = &'static str;

/// Переход, который возвращает узел.
#[derive(Debug)]
pub enum Transition {
    /// Перейти к узлу по объявленному ребру.
    Goto(NodeId),
    /// Остановиться на прерывании.
    Suspend(Interrupt),
    /// Граф завершён.
    Done,
}

/// Узел графа над состоянием `S` с ошибкой `E`.
pub trait Node<S, E>: Send + Sync {
    /// Id узла, уникальный в графе.
    fn id(&self) -> NodeId;
    /// Выполняет узел и возвращает переход.
    fn run<'a>(&'a self, st: &'a mut S) -> BoxFuture<'a, Result<Transition, E>>;
}

/// Итог [`Graph::run_from`].
#[derive(Debug)]
pub enum Exit {
    /// Узел вернул `Done`.
    Done,
    /// Узел вернул `Suspend`: run ждёт решения прерывания.
    Suspended(Interrupt),
}

/// Ошибка выполнения графа.
#[derive(Debug, thiserror::Error)]
pub enum GraphError<E> {
    /// Ошибка самого узла.
    #[error("узел: {0}")]
    Node(E),
    /// Переход к узлу, которого нет в графе.
    #[error("узел {0} не объявлен")]
    UnknownNode(NodeId),
    /// `Goto` по ребру, которое не объявлено.
    #[error("переход {from} -> {to} не объявлен")]
    UndeclaredEdge { from: NodeId, to: NodeId },
}

/// Граф: узлы, рёбра и вход.
pub struct Graph<S, E> {
    nodes: BTreeMap<NodeId, Box<dyn Node<S, E>>>,
    edges: BTreeSet<(NodeId, NodeId)>,
    entry: NodeId,
}

impl<S: Send, E: std::fmt::Display + std::fmt::Debug> Graph<S, E> {
    /// Пустой граф со входом `entry`.
    pub fn new(entry: NodeId) -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: BTreeSet::new(),
            entry,
        }
    }

    /// Добавляет узел; узел с тем же id заменяет прежний.
    pub fn node(mut self, n: Box<dyn Node<S, E>>) -> Self {
        self.nodes.insert(n.id(), n);
        self
    }

    /// Объявляет ребро `from → to`.
    pub fn edge(mut self, from: NodeId, to: NodeId) -> Self {
        self.edges.insert((from, to));
        self
    }

    /// BLAKE3 hex от `"za-graph-v1\0"`, входа, отсортированных id узлов и рёбер
    /// (CORE-008 в R2). Порядок вызовов [`Self::node`] и [`Self::edge`] не влияет.
    /// Каждая строка пишется с длиной (u64 LE), чтобы границы были однозначны.
    pub fn graph_hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"za-graph-v1\0");
        let mut put = |s: &str| {
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        };
        put(self.entry);
        put("nodes");
        for id in self.nodes.keys() {
            put(id);
        }
        put("edges");
        for (from, to) in &self.edges {
            put(from);
            put(to);
        }
        h.finalize().to_hex().to_string()
    }

    /// Выполняет граф с узла `at` до `Done` или `Suspend`.
    ///
    /// Неизвестный узел (в том числе цель объявленного ребра) —
    /// [`GraphError::UnknownNode`], `Goto` без ребра — [`GraphError::UndeclaredEdge`],
    /// ошибка узла — [`GraphError::Node`].
    pub async fn run_from(&self, at: NodeId, st: &mut S) -> Result<Exit, GraphError<E>> {
        let mut current = at;
        loop {
            let node = self
                .nodes
                .get(current)
                .ok_or(GraphError::UnknownNode(current))?;
            tracing::trace!(node = current, "узел графа");
            match node.run(st).await.map_err(GraphError::Node)? {
                Transition::Goto(to) => {
                    if !self.edges.contains(&(current, to)) {
                        return Err(GraphError::UndeclaredEdge { from: current, to });
                    }
                    current = to;
                }
                Transition::Suspend(i) => return Ok(Exit::Suspended(i)),
                Transition::Done => return Ok(Exit::Done),
            }
        }
    }
}
