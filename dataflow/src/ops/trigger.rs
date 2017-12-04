use std::collections::HashMap;

use prelude::*;

/// Applies the identity operation to the view. Since the identity does nothing,
/// it is the simplest possible operation. Primary intended as a reference
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    src: IndexPair,
}

impl Trigger {
    /// Construct a new Trigger operator.
    pub fn new(src: NodeIndex) -> Trigger {
        Trigger { src: src.into() }
    }
}

impl Ingredient for Trigger {
    fn take(&mut self) -> NodeOperator {
        Clone::clone(self).into()
    }

    fn ancestors(&self) -> Vec<NodeIndex> {
        vec![self.src.as_global()]
    }

    fn on_connected(&mut self, _: &Graph) {}

    fn on_commit(&mut self, _: NodeIndex, remap: &HashMap<NodeIndex, IndexPair>) {
        self.src.remap(remap);
    }

    fn on_input(
        &mut self,
        _: LocalNodeIndex,
        rs: Records,
        _: &mut Tracer,
        _: Option<usize>,
        _: &DomainNodes,
        _: &StateMap,
    ) -> ProcessingResult {
        ProcessingResult {
            results: rs,
            misses: Vec::new(),
        }
    }

    fn suggest_indexes(&self, _: NodeIndex) -> HashMap<NodeIndex, (Vec<usize>, bool)> {
        HashMap::new()
    }

    fn resolve(&self, col: usize) -> Option<Vec<(NodeIndex, usize)>> {
        Some(vec![(self.src.as_global(), col)])
    }

    fn description(&self) -> String {
        "≡".into()
    }

    fn parent_columns(&self, column: usize) -> Vec<(NodeIndex, Option<usize>)> {
        vec![(self.src.as_global(), Some(column))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ops;

    fn setup(materialized: bool) -> ops::test::MockGraph {
        let mut g = ops::test::MockGraph::new();
        let s = g.add_base("source", &["x", "y", "z"]);
        g.set_op(
            "identity",
            &["x", "y", "z"],
            Trigger::new(s.as_global()),
            materialized,
        );
        g
    }

    #[test]
    fn it_forwards() {
        let mut g = setup(false);

        let left: Vec<DataType> = vec![1.into(), "a".into()];
        assert_eq!(g.narrow_one_row(left.clone(), false), vec![left].into());
    }

    #[test]
    fn it_suggests_indices() {
        let g = setup(false);
        let me = 1.into();
        let idx = g.node().suggest_indexes(me);
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn it_resolves() {
        let g = setup(false);
        assert_eq!(
            g.node().resolve(0),
            Some(vec![(g.narrow_base_id().as_global(), 0)])
        );
        assert_eq!(
            g.node().resolve(1),
            Some(vec![(g.narrow_base_id().as_global(), 1)])
        );
        assert_eq!(
            g.node().resolve(2),
            Some(vec![(g.narrow_base_id().as_global(), 2)])
        );
    }
}
