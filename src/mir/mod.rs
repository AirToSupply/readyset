use nom_sql::{Column, Operator};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::{Error, Formatter, Debug};
use std::rc::Rc;

use flow::Migration;
use flow::core::{NodeAddress, DataType};
use ops;
use ops::topk::OrderedRecordComparator;
use sql::QueryFlowParts;

#[derive(Clone, Debug)]
pub enum FlowNode {
    New(NodeAddress),
    Existing(NodeAddress),
}

pub type MirNodeRef = Rc<RefCell<MirNode>>;

#[derive(Clone, Debug)]
pub struct MirQuery {
    pub name: String,
    pub roots: Vec<MirNodeRef>,
    pub leaf: MirNodeRef,
}

impl MirQuery {
    pub fn singleton(name: &str, node: MirNodeRef) -> MirQuery {
        MirQuery {
            name: String::from(name),
            roots: vec![node.clone()],
            leaf: node,
        }
    }

    pub fn into_flow_parts(&mut self, mut mig: &mut Migration) -> QueryFlowParts {
        let mut new_nodes = Vec::new();
        let mut reused_nodes = Vec::new();

        // starting at the roots, add nodes in topological order
        // XXX(malte): topo sort
        for n in self.roots.iter_mut() {
            let flow_node = n.borrow_mut().into_flow_parts(mig);
            match flow_node {
                FlowNode::New(na) => new_nodes.push(na),
                FlowNode::Existing(na) => reused_nodes.push(na),
            }
        }

        let leaf_na = match *self.leaf
                   .borrow()
                   .flow_node
                   .as_ref()
                   .expect("Leaf must have FlowNode by now") {
            FlowNode::New(na) |
            FlowNode::Existing(na) => na,
        };

        QueryFlowParts {
            name: self.name.clone(),
            new_nodes: new_nodes,
            reused_nodes: reused_nodes,
            query_leaf: leaf_na,
        }
    }

    pub fn optimize(self) -> MirQuery {
        // XXX(malte): currently a no-op
        self
    }
}

pub struct MirNode {
    name: String,
    from_version: usize,
    columns: Vec<Column>,
    inner: MirNodeType,
    ancestors: Vec<MirNodeRef>,
    children: Vec<MirNodeRef>,
    pub flow_node: Option<FlowNode>,
}

impl MirNode {
    pub fn new(name: &str,
               v: usize,
               columns: Vec<Column>,
               inner: MirNodeType,
               ancestors: Vec<MirNodeRef>,
               children: Vec<MirNodeRef>)
               -> Self {
        MirNode {
            name: String::from(name),
            from_version: 0,
            columns: columns,
            inner: inner,
            ancestors: ancestors.clone(),
            children: children.clone(),
            flow_node: None,
        }
    }

    pub fn reuse(node: MirNodeRef, v: usize) -> Self {
        let rcn = node.clone();

        MirNode {
            name: node.borrow().name.clone(),
            from_version: v,
            columns: node.borrow().columns.clone(),
            inner: MirNodeType::Reuse(rcn),
            ancestors: node.borrow().ancestors.clone(),
            children: node.borrow().children.clone(),
            flow_node: None, // will be set in `into_flow_parts`
        }
    }

    pub fn add_ancestor(&mut self, a: MirNodeRef) {
        self.ancestors.push(a)
    }

    pub fn add_child(&mut self, c: MirNodeRef) {
        self.children.push(c)
    }

    pub fn ancestors(&self) -> &[MirNodeRef] {
        self.ancestors.as_slice()
    }

    pub fn children(&self) -> &[MirNodeRef] {
        self.children.as_slice()
    }

    pub fn columns(&self) -> &[Column] {
        self.columns.as_slice()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn versioned_name(&self) -> String {
        format!("{}:v{}", self.name, self.from_version)
    }

    /// Produce a compact, human-readable description of this node; analogous to the method of the
    /// same name on `Ingredient`.
    ///
    ///  Symbol   Description
    /// --------|-------------
    ///    B    |  Base
    ///    ||   |  Concat
    ///    ⧖    |  Latest
    ///    γ    |  Group by
    ///   |*|   |  Count
    ///    𝛴    |  Sum
    ///    ⋈    |  Join
    ///    ⋉    |  Left join
    ///    ⋃    |  Union
    fn description(&self) -> String {
        format!("{}: {} / {} columns",
                self.versioned_name(),
                self.inner.description(),
                self.columns.len())
    }

    /// Translate a column in this ingredient into the corresponding column(s) in
    /// parent ingredients. None for the column means that the parent doesn't
    /// have an associated column. Similar to `resolve`, but does not depend on
    /// materialization, and returns results even for computed columns.
    fn parent_columns(&self, column: Column) -> Vec<(String, Option<Column>)> {
        unimplemented!()
    }

    /// Resolve where the given column originates from. If the view is materialized, or the value is
    /// otherwise created by this view, None should be returned.
    fn resolve_column(&self, column: Column) -> Option<Vec<(String, Column)>> {
        unimplemented!()
    }

    fn into_flow_parts(&mut self, mig: &mut Migration) -> FlowNode {
        let name = self.name.clone();
        match self.flow_node {
            None => {
                let flow_node = self.inner.into_flow_parts(&name, mig);
                self.flow_node = Some(flow_node.clone());
                flow_node
            }
            Some(ref flow_node) => flow_node.clone(),
        }
    }
}

pub enum MirNodeType {
    /// over column, group_by columns
    Aggregation(Column, Vec<Column>),
    /// columns, keys (non-compound)
    Base(Vec<Column>, Vec<Column>),
    /// over column, group_by columns
    Extremum(Column, Vec<Column>),
    /// filter conditions (one for each parent column)
    Filter(Vec<(Operator, DataType)>),
    /// over column, separator
    GroupConcat(Column, String),
    /// no extra info required
    Identity,
    /// on left columns, on right columns, emit columns
    Join(Vec<Column>, Vec<Column>, Vec<Column>),
    /// on left column, on right column, emit columns
    LeftJoin(Vec<Column>, Vec<Column>, Vec<Column>),
    /// group columns
    Latest(Vec<Column>),
    /// emit columns
    Project(Vec<Column>),
    /// emit columns
    Permute(Vec<Column>),
    /// emit columns left, emit columns right
    Union(Vec<Column>, Vec<Column>),
    /// order function, group columns, k
    TopK(Box<OrderedRecordComparator>, Vec<Column>, usize),
    /// reuse another node
    Reuse(MirNodeRef),
    /// leaf (reader) node, keys
    Leaf(MirNodeRef, Vec<Column>),
}

impl MirNodeType {
    fn description(&self) -> String {
        format!("{:?}", self)
    }

    fn into_flow_parts(&self, name: &str, mut mig: &mut Migration) -> FlowNode {
        match *self {
            MirNodeType::Base(ref cols, ref keys) => make_base_node(name, cols, keys, mig),
            MirNodeType::Join(ref left_cols, ref right_cols, ref proj_cols) => {
                make_join_node(name, left_cols, right_cols, proj_cols, mig)
            }
            MirNodeType::Permute(ref cols) |
            MirNodeType::Project(ref cols) => make_permute_node(name, cols, mig),
            MirNodeType::Reuse(ref node) => {
                match *node.borrow()
                           .flow_node
                           .as_ref()
                           .expect("Reused MirNode must have FlowNode") {
                    // "New" => flow node was originally created for the node that we
                    // are reusing
                    FlowNode::New(na) |
                    // "Existing" => flow node was already reused from some other MIR node
                    FlowNode::Existing(na) => FlowNode::Existing(na),
                }
            }
            _ => unimplemented!(),
        }
    }
}

impl Debug for MirNode {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{}", self.description())
    }
}

impl Debug for MirNodeType {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        match *self {
            MirNodeType::Base(_, ref keys) => {
                write!(f,
                       "B [⚷: {}]",
                       keys.iter()
                           .map(|c| c.name.as_str())
                           .collect::<Vec<_>>()
                           .join(","))
            }
            MirNodeType::Join(ref l_cols, ref r_cols, ref proj_cols) => write!(f, "⋈ []"),
            MirNodeType::Reuse(ref reused) => write!(f, "Reuse [{:#?}]", reused),
            MirNodeType::Permute(ref cols) => {
                write!(f,
                       "π [{}]",
                       cols.iter()
                           .map(|c| c.name.as_str())
                           .collect::<Vec<_>>()
                           .join(", "))
            }
            _ => unimplemented!(),
        }
    }
}

fn make_base_node(name: &str,
                  cols: &Vec<Column>,
                  keys: &Vec<Column>,
                  mut mig: &mut Migration)
                  -> FlowNode {
    let node = if keys.len() > 0 {
        let pkey_column_ids = keys.iter()
            .map(|pkc| {
                     //assert_eq!(pkc.table.as_ref().unwrap(), name);
                     cols.iter().position(|c| c == pkc).unwrap()
                 })
            .collect();
        mig.add_ingredient(name,
                           cols.iter()
                               .map(|c| &c.name)
                               .collect::<Vec<_>>()
                               .as_slice(),
                           ops::base::Base::new(pkey_column_ids))
    } else {
        mig.add_ingredient(name,
                           cols.iter()
                               .map(|c| &c.name)
                               .collect::<Vec<_>>()
                               .as_slice(),
                           ops::base::Base::default())
    };
    FlowNode::New(node)
}

fn make_join_node(name: &str,
                  left_cols: &Vec<Column>,
                  right_cols: &Vec<Column>,
                  proj_cols: &Vec<Column>,
                  mut mig: &mut Migration)
                  -> FlowNode {
    unimplemented!();
}

fn make_permute_node(name: &str, cols: &Vec<Column>, mut mig: &mut Migration) -> FlowNode {
    unimplemented!()
}
