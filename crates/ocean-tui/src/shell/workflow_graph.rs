//! Read-only Observatory projection for the TUI workflow graph.
//!
//! This is display state only. Execution authority, topology admission, and
//! orchestration remain daemon/extension owned; the TUI reduces typed
//! Observatory snapshots and events into a stable spatial scene.

use std::collections::{HashMap, HashSet};

use ocean_observatory::{
    EventEnvelope, EventPayload, ExecutionPhase, ObservatorySnapshot, Producer, SnapshotEdge,
    SnapshotNode, TruthProvenance,
};

use crate::shell::spatial::{Camera, Vec3};

#[derive(Clone, Debug)]
pub struct WorkflowNode {
    pub execution_id: String,
    pub parent_execution_id: Option<String>,
    pub phase: ExecutionPhase,
    pub producer: Producer,
    pub truth: TruthProvenance,
    pub labels: Vec<String>,
    pub duration_millis: Option<u64>,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl WorkflowNode {
    fn from_snapshot(node: SnapshotNode) -> Self {
        let (x, y, z) = stable_position(&node.execution_id);
        Self {
            execution_id: node.execution_id,
            parent_execution_id: node.parent_execution_id,
            phase: node.phase,
            producer: node.producer,
            truth: node.truth,
            labels: node.labels,
            duration_millis: node.duration_millis,
            x,
            y,
            z,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            ExecutionPhase::Admitted | ExecutionPhase::Running
        )
    }

    pub fn title(&self) -> String {
        let base = self
            .labels
            .first()
            .filter(|label| !label.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| {
                let short = self.execution_id.chars().take(8).collect::<String>();
                if self.producer.id.trim().is_empty() {
                    short
                } else {
                    format!("{} · {short}", self.producer.id)
                }
            });
        let relation = self.parent_execution_id.as_ref().map(|_| "child");
        let provenance = match self.truth {
            TruthProvenance::HostObserved => None,
            TruthProvenance::ExtensionAttested => Some("attested"),
            TruthProvenance::Derived => Some("derived"),
        };
        [relation, provenance]
            .into_iter()
            .flatten()
            .fold(base, |title, suffix| format!("{title} · {suffix}"))
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowEdge {
    pub edge_id: String,
    pub parent_execution_id: String,
    pub child_execution_id: String,
}

impl From<SnapshotEdge> for WorkflowEdge {
    fn from(edge: SnapshotEdge) -> Self {
        Self {
            edge_id: edge.edge_id,
            parent_execution_id: edge.parent_execution_id,
            child_execution_id: edge.child_execution_id,
        }
    }
}

#[derive(Debug, Default)]
pub struct WorkflowGraph {
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
    pub selected: usize,
    pub camera: Camera,
    pub watermark: u64,
    pub daemon_instance_id: String,
    pub connected: bool,
    active_count_cache: usize,
    indexed_edges: Vec<(usize, usize)>,
    adjacency: Vec<Vec<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkflowGraphCommand {
    MoveSelection(isize),
    Orbit { yaw: f64, pitch: f64 },
    Pan { right: f64, up: f64 },
    Zoom(f64),
    ResetView,
}

impl WorkflowGraph {
    /// Replace the projection with one authoritative snapshot. Returns true
    /// only for the inactive -> active transition used by right-rail reveal.
    pub fn replace_snapshot(&mut self, snapshot: ObservatorySnapshot) -> bool {
        let was_active = self.has_active();
        let selected_id = self
            .nodes
            .get(self.selected)
            .map(|node| node.execution_id.clone());
        self.watermark = snapshot.watermark_cursor.into_inner();
        self.daemon_instance_id = snapshot.daemon_instance_id;
        self.nodes = snapshot
            .nodes
            .into_iter()
            .filter(|node| !node.execution_id.trim().is_empty())
            .map(WorkflowNode::from_snapshot)
            .collect();
        self.nodes.sort_by(|a, b| {
            b.is_active()
                .cmp(&a.is_active())
                .then_with(|| a.execution_id.cmp(&b.execution_id))
        });
        self.edges = snapshot
            .edges
            .into_iter()
            .filter(|edge| {
                !edge.parent_execution_id.trim().is_empty()
                    && !edge.child_execution_id.trim().is_empty()
            })
            .map(WorkflowEdge::from)
            .collect();
        self.edges.sort_by(|a, b| a.edge_id.cmp(&b.edge_id));
        self.selected = selected_id
            .and_then(|id| self.nodes.iter().position(|node| node.execution_id == id))
            .unwrap_or(0)
            .min(self.nodes.len().saturating_sub(1));
        self.rebuild_topology();
        self.connected = true;
        !was_active && self.has_active()
    }

    /// Apply one cursor-ordered live event. Duplicate/old frames are ignored.
    /// A daemon-instance change is rejected so the client can rebaseline.
    pub fn apply_event(&mut self, event: EventEnvelope) -> ApplyEvent {
        if event.daemon_instance_id != self.daemon_instance_id {
            self.connected = false;
            return ApplyEvent::NeedsSnapshot;
        }
        let cursor = event.cursor.into_inner();
        if cursor <= self.watermark {
            return ApplyEvent::Ignored;
        }
        if cursor != self.watermark.saturating_add(1) {
            self.connected = false;
            return ApplyEvent::NeedsSnapshot;
        }

        let was_active = self.has_active();
        let execution_id = event.topology.execution_id.clone();
        if !execution_id.trim().is_empty() {
            let index = self
                .nodes
                .iter()
                .position(|node| node.execution_id == execution_id);
            match (&event.payload, index) {
                (EventPayload::ExecutionAdmitted { phase, labels }, None) => {
                    let (x, y, z) = stable_position(&execution_id);
                    self.nodes.push(WorkflowNode {
                        execution_id: execution_id.clone(),
                        parent_execution_id: event.topology.parent_execution_id.clone(),
                        phase: *phase,
                        producer: event.producer.clone(),
                        truth: event.truth,
                        labels: labels.clone(),
                        duration_millis: None,
                        x,
                        y,
                        z,
                    });
                }
                (EventPayload::ExecutionAdmitted { phase, labels }, Some(index)) => {
                    let node = &mut self.nodes[index];
                    node.phase = *phase;
                    node.labels = labels.clone();
                }
                (EventPayload::ExecutionPhaseChanged { to_phase, .. }, Some(index)) => {
                    self.nodes[index].phase = *to_phase;
                }
                (
                    EventPayload::ExecutionFinished {
                        phase,
                        duration_millis,
                        ..
                    },
                    Some(index),
                ) => {
                    self.nodes[index].phase = *phase;
                    self.nodes[index].duration_millis = Some(*duration_millis);
                }
                _ => {}
            }

            if let (Some(parent), Some(edge_id)) = (
                event.topology.parent_execution_id.as_ref(),
                event.topology.edge_id.as_ref(),
            ) {
                if !parent.is_empty()
                    && !edge_id.is_empty()
                    && !self.edges.iter().any(|edge| edge.edge_id == *edge_id)
                {
                    self.edges.push(WorkflowEdge {
                        edge_id: edge_id.clone(),
                        parent_execution_id: parent.clone(),
                        child_execution_id: execution_id,
                    });
                }
            }
        }
        let selected_id = self
            .nodes
            .get(self.selected)
            .map(|node| node.execution_id.clone());
        self.nodes.sort_by(|a, b| {
            b.is_active()
                .cmp(&a.is_active())
                .then_with(|| a.execution_id.cmp(&b.execution_id))
        });
        self.selected = selected_id
            .and_then(|id| self.nodes.iter().position(|node| node.execution_id == id))
            .unwrap_or(0)
            .min(self.nodes.len().saturating_sub(1));
        self.rebuild_topology();
        self.watermark = cursor;
        self.connected = true;
        ApplyEvent::Applied {
            became_active: !was_active && self.has_active(),
        }
    }

    pub fn mark_disconnected(&mut self) {
        self.connected = false;
    }

    pub const fn active_count(&self) -> usize {
        self.active_count_cache
    }

    pub fn has_active(&self) -> bool {
        self.active_count() > 0
    }

    pub fn apply_command(&mut self, command: WorkflowGraphCommand) {
        match command {
            WorkflowGraphCommand::MoveSelection(delta) => {
                if self.nodes.is_empty() {
                    self.selected = 0;
                } else {
                    self.selected = (self.selected as isize + delta)
                        .rem_euclid(self.nodes.len() as isize)
                        as usize;
                }
            }
            WorkflowGraphCommand::Orbit { yaw, pitch } => self.camera.orbit(yaw, pitch),
            WorkflowGraphCommand::Pan { right, up } => self.camera.pan(right, up),
            WorkflowGraphCommand::Zoom(factor) => self.camera.zoom(factor),
            WorkflowGraphCommand::ResetView => self.camera.reset(),
        }
    }

    pub fn node_world(&self, index: usize) -> Option<Vec3> {
        self.nodes
            .get(index)
            .map(|node| Vec3::new(node.x as f64, node.y as f64, node.z as f64))
    }

    /// Build only edges between the bounded visible-node set. Pairwise checks
    /// are capped by the renderer's node limit and use binary search in cached
    /// adjacency, so frame cost never scales with retained history size or a
    /// pathological selected-node degree.
    pub fn render_edges(&self, visible: &[usize], limit: usize) -> Vec<(usize, usize)> {
        let mut edges = Vec::new();
        for (offset, &left) in visible.iter().enumerate() {
            let Some(neighbors) = self.adjacency.get(left) else {
                continue;
            };
            for &right in &visible[offset + 1..] {
                if neighbors.binary_search(&right).is_ok() {
                    edges.push((left, right));
                    if edges.len() >= limit {
                        return edges;
                    }
                }
            }
        }
        edges
    }

    pub fn visible_neighbors_of_selected(&self, visible: &[usize]) -> HashSet<usize> {
        let Some(neighbors) = self.adjacency.get(self.selected) else {
            return HashSet::new();
        };
        visible
            .iter()
            .copied()
            .filter(|index| neighbors.binary_search(index).is_ok())
            .collect()
    }

    fn rebuild_topology(&mut self) {
        self.active_count_cache = self.nodes.iter().filter(|node| node.is_active()).count();
        let indexes: HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.execution_id.as_str(), index))
            .collect();
        self.indexed_edges = self
            .edges
            .iter()
            .filter_map(|edge| {
                Some((
                    *indexes.get(edge.parent_execution_id.as_str())?,
                    *indexes.get(edge.child_execution_id.as_str())?,
                ))
            })
            .collect();
        self.adjacency = vec![Vec::new(); self.nodes.len()];
        for &(parent, child) in &self.indexed_edges {
            self.adjacency[parent].push(child);
            self.adjacency[child].push(parent);
        }
        for neighbors in &mut self.adjacency {
            neighbors.sort_unstable();
            neighbors.dedup();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyEvent {
    Applied { became_active: bool },
    Ignored,
    NeedsSnapshot,
}

/// Stable, bounded placement based only on immutable execution identity.
/// Nodes never drift as siblings arrive; edges carry the topology.
fn stable_position(id: &str) -> (f32, f32, f32) {
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }
    fn axis(hash: u64) -> f64 {
        ((hash & 0xffff) as f64 / 32767.5) - 1.0
    }
    let bytes = id.as_bytes();
    let mut x = axis(fnv(0xcbf29ce484222325, bytes));
    let mut y = axis(fnv(0x84222325cbf29ce4, bytes));
    let mut z = axis(fnv(0x9e3779b97f4a7c15, bytes));
    let length = (x * x + y * y + z * z).sqrt().max(0.001);
    x /= length;
    y /= length;
    z /= length;
    let radius_hash = fnv(0x517cc1b727220a95, bytes);
    let radius = 0.35 + ((radius_hash >> 16) & 0xffff) as f64 / 100_824.0;
    (
        (x * radius) as f32,
        (y * radius) as f32,
        (z * radius) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_observatory::{Correlation, Cursor, EventKind, ProducerKind, Topology, Visibility};

    fn node(id: &str, phase: ExecutionPhase) -> SnapshotNode {
        SnapshotNode {
            execution_id: id.into(),
            root_execution_id: id.into(),
            parent_execution_id: None,
            session_id: String::new(),
            turn_id: String::new(),
            request_id: String::new(),
            phase,
            producer: Producer {
                kind: ProducerKind::Extension,
                id: "crew".into(),
            },
            truth: TruthProvenance::ExtensionAttested,
            started_at: "now".into(),
            last_activity_at: "now".into(),
            labels: vec!["worker".into()],
            duration_millis: None,
        }
    }

    fn snapshot(nodes: Vec<SnapshotNode>) -> ObservatorySnapshot {
        ObservatorySnapshot {
            watermark_cursor: Cursor::new(3),
            earliest_available_cursor: Cursor::new(1),
            observatory_id: "obs".into(),
            daemon_instance_id: "boot".into(),
            nodes,
            edges: Vec::new(),
            attention: Vec::new(),
        }
    }

    fn phase_event(cursor: u64, boot: &str, phase: ExecutionPhase) -> EventEnvelope {
        EventEnvelope {
            schema_version: 1,
            cursor: Cursor::new(cursor),
            event_id: format!("event-{cursor}"),
            observatory_id: "obs".into(),
            daemon_instance_id: boot.into(),
            occurred_at: "now".into(),
            recorded_at: "now".into(),
            kind: EventKind::ExecutionPhaseChanged,
            truth: TruthProvenance::ExtensionAttested,
            producer: Producer {
                kind: ProducerKind::Extension,
                id: "crew".into(),
            },
            topology: Topology {
                execution_id: "a".into(),
                root_execution_id: "a".into(),
                parent_execution_id: None,
                edge_id: None,
                session_id: String::new(),
                turn_id: String::new(),
                request_id: String::new(),
            },
            correlation: Correlation {
                tool_call_id: None,
                permission_id: None,
            },
            visibility: Visibility::Metadata,
            payload: EventPayload::ExecutionPhaseChanged {
                from_phase: ExecutionPhase::Running,
                to_phase: phase,
            },
        }
    }

    #[test]
    fn snapshot_activation_is_edge_triggered_and_terminal_nodes_remain() {
        let mut graph = WorkflowGraph::default();
        assert!(graph.replace_snapshot(snapshot(vec![node("a", ExecutionPhase::Running)])));
        assert!(!graph.replace_snapshot(snapshot(vec![node("a", ExecutionPhase::Finished)])));
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.active_count(), 0);
    }

    #[test]
    fn live_events_require_cursor_and_daemon_instance_continuity() {
        let mut graph = WorkflowGraph::default();
        graph.replace_snapshot(snapshot(vec![node("a", ExecutionPhase::Running)]));
        assert_eq!(
            graph.apply_event(phase_event(4, "boot", ExecutionPhase::Finished)),
            ApplyEvent::Applied {
                became_active: false
            }
        );
        assert_eq!(graph.nodes[0].phase, ExecutionPhase::Finished);
        assert_eq!(
            graph.apply_event(phase_event(6, "boot", ExecutionPhase::Running)),
            ApplyEvent::NeedsSnapshot,
            "cursor gaps must rebaseline"
        );

        graph.replace_snapshot(snapshot(vec![node("a", ExecutionPhase::Running)]));
        assert_eq!(
            graph.apply_event(phase_event(4, "new-boot", ExecutionPhase::Finished)),
            ApplyEvent::NeedsSnapshot,
            "daemon restart must rebaseline"
        );
    }

    #[test]
    fn render_projection_is_bounded_independently_from_retained_history() {
        let mut graph = WorkflowGraph {
            nodes: (0..1_000)
                .map(|index| {
                    WorkflowNode::from_snapshot(node(
                        &format!("node-{index}"),
                        ExecutionPhase::Finished,
                    ))
                })
                .collect(),
            edges: (1..1_000)
                .map(|index| WorkflowEdge {
                    edge_id: format!("edge-{index}"),
                    parent_execution_id: "node-0".into(),
                    child_execution_id: format!("node-{index}"),
                })
                .collect(),
            ..WorkflowGraph::default()
        };
        graph.rebuild_topology();
        let visible: Vec<usize> = (0..256).collect();
        assert_eq!(graph.render_edges(&visible, 32).len(), 32);
        assert!(graph.visible_neighbors_of_selected(&visible).len() <= 255);
        assert_eq!(graph.nodes.len(), 1_000, "authoritative state is retained");
    }

    #[test]
    fn stable_position_does_not_depend_on_other_nodes() {
        let before = stable_position("execution-a");
        let _ = stable_position("execution-b");
        assert_eq!(before, stable_position("execution-a"));
    }

    #[test]
    fn selection_survives_snapshot_reordering() {
        let mut graph = WorkflowGraph::default();
        graph.replace_snapshot(snapshot(vec![
            node("a", ExecutionPhase::Running),
            node("b", ExecutionPhase::Running),
        ]));
        graph.selected = graph
            .nodes
            .iter()
            .position(|node| node.execution_id == "b")
            .unwrap();
        graph.replace_snapshot(snapshot(vec![
            node("b", ExecutionPhase::Finished),
            node("a", ExecutionPhase::Running),
        ]));
        assert_eq!(graph.nodes[graph.selected].execution_id, "b");
    }
}
