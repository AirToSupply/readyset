use nom_sql::{Column, Operator, OrderType};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::{Error, Formatter, Debug, Display};
use std::rc::Rc;

use flow::Migration;
use flow::core::{NodeAddress, DataType};
use ops;
use ops::grouped::aggregate::Aggregation as AggregationKind;
use ops::grouped::extremum::Extremum as ExtremumKind;
use ops::join::{Join, JoinType};
use ops::latest::Latest;
use ops::project::Project;
use ops::permute::Permute;
use sql::QueryFlowParts;

mod rewrite;
mod optimize;

#[derive(Clone, Debug)]
pub enum FlowNode {
    New(NodeAddress),
    Existing(NodeAddress),
}

impl FlowNode {
    pub fn address(&self) -> NodeAddress {
        match *self {
            FlowNode::New(na) |
            FlowNode::Existing(na) => na,
        }
    }
}

/// Helper enum to avoid having separate `make_aggregation_node` and `make_extremum_node` functions
pub enum GroupedNodeType {
    Aggregation(ops::grouped::aggregate::Aggregation),
    Extremum(ops::grouped::extremum::Extremum),
    GroupConcat(String),
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
        use std::collections::VecDeque;

        let mut new_nodes = Vec::new();
        let mut reused_nodes = Vec::new();

        // starting at the roots, add nodes in topological order
        let mut node_queue = VecDeque::new();
        node_queue.extend(self.roots.iter().cloned());
        let mut in_edge_counts = HashMap::new();
        for n in &node_queue {
            in_edge_counts.insert(n.borrow().versioned_name(), 0);
        }
        while !node_queue.is_empty() {
            let n = node_queue.pop_front().unwrap();
            assert_eq!(in_edge_counts[&n.borrow().versioned_name()], 0);

            let flow_node = n.borrow_mut().into_flow_parts(mig);
            match flow_node {
                FlowNode::New(na) => new_nodes.push(na),
                FlowNode::Existing(na) => reused_nodes.push(na),
            }

            for child in n.borrow().children.iter() {
                let nd = child.borrow().versioned_name();
                let in_edges = if in_edge_counts.contains_key(&nd) {
                    in_edge_counts[&nd]
                } else {
                    child.borrow().ancestors.len()
                };
                assert!(in_edges >= 1);
                if in_edges == 1 {
                    // last edge removed
                    node_queue.push_back(child.clone());
                }
                in_edge_counts.insert(nd, in_edges - 1);
            }
        }

        let leaf_na = self.leaf
            .borrow()
            .flow_node
            .as_ref()
            .expect("Leaf must have FlowNode by now")
            .address();

        QueryFlowParts {
            name: self.name.clone(),
            new_nodes: new_nodes,
            reused_nodes: reused_nodes,
            query_leaf: leaf_na,
        }
    }

    pub fn optimize(mut self) -> MirQuery {
        rewrite::pull_required_base_columns(&mut self);
        optimize::optimize(self)
    }
}

impl Display for MirQuery {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        use std::collections::VecDeque;

        // starting at the roots, print nodes in topological order
        let mut node_queue = VecDeque::new();
        node_queue.extend(self.roots.iter().cloned());
        let mut in_edge_counts = HashMap::new();
        for n in &node_queue {
            in_edge_counts.insert(n.borrow().versioned_name(), 0);
        }

        writeln!(f, "digraph {{")?;
        writeln!(f, "node [shape=record, fontsize=10]")?;

        while !node_queue.is_empty() {
            let n = node_queue.pop_front().unwrap();
            assert_eq!(in_edge_counts[&n.borrow().versioned_name()], 0);

            let vn = n.borrow().versioned_name();
            writeln!(f,
                     "\"{}\" [label=\"{{ {} | {} }}\"]",
                     vn,
                     vn,
                     n.borrow()
                         .columns()
                         .iter()
                         .map(|c| c.name.as_str())
                         .collect::<Vec<_>>()
                         .join(", \\n"))?;

            for child in n.borrow().children.iter() {
                let nd = child.borrow().versioned_name();
                writeln!(f, "\"{}\" -> \"{}\"", n.borrow().versioned_name(), nd)?;
                let in_edges = if in_edge_counts.contains_key(&nd) {
                    in_edge_counts[&nd]
                } else {
                    child.borrow().ancestors.len()
                };
                assert!(in_edges >= 1);
                if in_edges == 1 {
                    // last edge removed
                    node_queue.push_back(child.clone());
                }
                in_edge_counts.insert(nd, in_edges - 1);
            }
        }
        write!(f, "}}")?;

        Ok(())
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
               -> MirNodeRef {
        let mn = MirNode {
            name: String::from(name),
            from_version: v,
            columns: columns,
            inner: inner,
            ancestors: ancestors.clone(),
            children: children.clone(),
            flow_node: None,
        };

        let rc_mn = Rc::new(RefCell::new(mn));

        // register as child on ancestors
        for ref ancestor in ancestors {
            ancestor.borrow_mut().add_child(rc_mn.clone());
        }

        rc_mn
    }

    pub fn reuse(node: MirNodeRef, v: usize) -> MirNodeRef {
        let rcn = node.clone();

        let mn = MirNode {
            name: node.borrow().name.clone(),
            from_version: v,
            columns: node.borrow().columns.clone(),
            inner: MirNodeType::Reuse { node: rcn },
            ancestors: node.borrow().ancestors.clone(),
            children: node.borrow().children.clone(),
            flow_node: None, // will be set in `into_flow_parts`
        };

        let rc_mn = Rc::new(RefCell::new(mn));

        // register as child on ancestors
        for ref ancestor in &node.borrow().ancestors {
            ancestor.borrow_mut().add_child(rc_mn.clone());
        }

        rc_mn
    }

    // currently unused
    #[allow(dead_code)]
    pub fn add_ancestor(&mut self, a: MirNodeRef) {
        self.ancestors.push(a)
    }

    pub fn add_child(&mut self, c: MirNodeRef) {
        self.children.push(c)
    }

    pub fn add_column(&mut self, c: Column) {
        self.columns.push(c.clone());
        self.inner.add_column(c);
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

    fn flow_node_addr(&self) -> Result<NodeAddress, String> {
        match self.flow_node {
            Some(FlowNode::New(na)) |
            Some(FlowNode::Existing(na)) => Ok(na),
            None => {
                Err(format!("MIR node \"{}\" does not have an associated FlowNode",
                            self.versioned_name()))
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn referenced_columns(&self) -> Vec<Column> {
        // all projected columns, minus those with aliases, which we add with their original names
        // below. This is important because we'll otherwise end up searching (and fail to find)
        // aliased columns further up in the MIR graph.
        let mut columns: Vec<Column> = self.columns
            .iter()
            .filter(|c| c.alias.is_none() || c.function.is_some()) // alias ok if computed column
            .cloned()
            .collect();

        // + any parent columns referenced internally by the operator
        match self.inner {
            MirNodeType::Aggregation { ref on, .. } |
            MirNodeType::Extremum { ref on, .. } |
            MirNodeType::GroupConcat { ref on, .. } => {
                // need the "over" column
                if !columns.contains(on) {
                    columns.push(on.clone());
                }
            }
            MirNodeType::Filter { .. } => {
                let parent = self.ancestors.iter().next().unwrap();
                // need all parent columns
                for c in parent.borrow().columns() {
                    if !columns.contains(&c) {
                        columns.push(c.clone());
                    }
                }
            }
            MirNodeType::Project { ref emit, .. } |
            MirNodeType::Permute { ref emit } => {
                for c in emit {
                    if !columns.contains(&c) {
                        columns.push(c.clone());
                    }
                }
            }
            _ => (),
        }
        columns
    }

    pub fn versioned_name(&self) -> String {
        format!("{}_v{}", self.name, self.from_version)
    }

    /// Produce a compact, human-readable description of this node; analogous to the method of the
    /// same name on `Ingredient`.
    fn description(&self) -> String {
        format!("{}: {} / {} columns",
                self.versioned_name(),
                self.inner.description(),
                self.columns.len())
    }

    fn into_flow_parts(&mut self, mig: &mut Migration) -> FlowNode {
        let name = self.name.clone();
        match self.flow_node {
            None => {
                let flow_node = match self.inner {
                    MirNodeType::Aggregation {
                        ref on,
                        ref group_by,
                        ref kind,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_grouped_node(&name,
                                          parent,
                                          self.columns.as_slice(),
                                          on,
                                          group_by,
                                          GroupedNodeType::Aggregation(kind.clone()),
                                          mig)
                    }
                    MirNodeType::Base { ref keys } => {
                        make_base_node(&name, self.columns.as_slice(), keys, mig)
                    }
                    MirNodeType::Extremum {
                        ref on,
                        ref group_by,
                        ref kind,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_grouped_node(&name,
                                          parent,
                                          self.columns.as_slice(),
                                          on,
                                          group_by,
                                          GroupedNodeType::Extremum(kind.clone()),
                                          mig)
                    }
                    MirNodeType::Filter { ref conditions } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_filter_node(&name, parent, self.columns.as_slice(), conditions, mig)
                    }
                    MirNodeType::GroupConcat {
                        ref on,
                        ref separator,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        let group_cols = parent.borrow().columns().iter().cloned().collect();
                        make_grouped_node(&name,
                                          parent,
                                          self.columns.as_slice(),
                                          on,
                                          &group_cols,
                                          GroupedNodeType::GroupConcat(separator.to_string()),
                                          mig)
                    }
                    MirNodeType::Identity => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_identity_node(&name, parent, self.columns.as_slice(), mig)
                    }
                    MirNodeType::Join {
                        ref on_left,
                        ref on_right,
                        ref project,
                    } => {
                        assert_eq!(self.ancestors.len(), 2);
                        let left = self.ancestors[0].clone();
                        let right = self.ancestors[1].clone();
                        make_join_node(&name,
                                       left,
                                       right,
                                       self.columns.as_slice(),
                                       on_left,
                                       on_right,
                                       project,
                                       JoinType::Inner,
                                       mig)
                    }
                    MirNodeType::Latest { ref group_by } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_latest_node(&name, parent, self.columns.as_slice(), group_by, mig)
                    }
                    MirNodeType::Leaf { ref keys, .. } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        materialize_leaf_node(&parent, keys, mig);
                        // TODO(malte): below is yucky, but required to satisfy the type system:
                        // each match arm must return a `FlowNode`, so we use the parent's one
                        // here.
                        let node = match *parent.borrow().flow_node.as_ref().unwrap() {
                            FlowNode::New(na) => FlowNode::Existing(na),
                            ref n @ FlowNode::Existing(..) => n.clone(),
                        };
                        node
                    }
                    MirNodeType::LeftJoin {
                        ref on_left,
                        ref on_right,
                        ref project,
                    } => {
                        assert_eq!(self.ancestors.len(), 2);
                        let left = self.ancestors[0].clone();
                        let right = self.ancestors[1].clone();
                        make_join_node(&name,
                                       left,
                                       right,
                                       self.columns.as_slice(),
                                       on_left,
                                       on_right,
                                       project,
                                       JoinType::Left,
                                       mig)
                    }
                    MirNodeType::Project {
                        ref emit,
                        ref literals,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_project_node(&name,
                                          parent,
                                          self.columns.as_slice(),
                                          emit,
                                          literals,
                                          mig)
                    }
                    MirNodeType::Permute { ref emit } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_permute_node(&name, parent, self.columns.as_slice(), emit, mig)
                    }
                    MirNodeType::Reuse { ref node } => {
                        match *node.borrow()
                           .flow_node
                           .as_ref()
                           .expect("Reused MirNode must have FlowNode") {
                               // "New" => flow node was originally created for the node that we
                               // are reusing
                               FlowNode::New(na) |
                               // "Existing" => flow node was already reused from some other
                               // MIR node
                               FlowNode::Existing(na) => FlowNode::Existing(na),
                        }
                    }
                    MirNodeType::Union { .. } => {
                        assert_eq!(self.ancestors.len(), 2);
                        let _left = self.ancestors[0].clone();
                        let _right = self.ancestors[1].clone();
                        // XXX(malte): fix
                        unimplemented!()
                    }
                    MirNodeType::TopK {
                        ref order,
                        ref group_by,
                        ref k,
                        ref offset,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_topk_node(&name,
                                       parent,
                                       self.columns.as_slice(),
                                       order,
                                       group_by,
                                       *k,
                                       *offset,
                                       mig)
                    }
                };

                // any new flow nodes have been instantiated by now, so we replace them with
                // existing ones, but still return `FlowNode::New` below in order to notify higher
                // layers of the new nodes.
                self.flow_node = match flow_node {
                    FlowNode::New(na) => Some(FlowNode::Existing(na)),
                    ref n @ FlowNode::Existing(..) => Some(n.clone()),
                };
                flow_node
            }
            Some(ref flow_node) => flow_node.clone(),
        }
    }
}

pub enum MirNodeType {
    /// over column, group_by columns
    Aggregation {
        on: Column,
        group_by: Vec<Column>,
        kind: AggregationKind,
    },
    /// columns, keys (non-compound)
    Base { keys: Vec<Column> },
    /// over column, group_by columns
    Extremum {
        on: Column,
        group_by: Vec<Column>,
        kind: ExtremumKind,
    },
    /// filter conditions (one for each parent column)
    Filter { conditions: Vec<Option<(Operator, DataType)>>, },
    /// over column, separator
    GroupConcat { on: Column, separator: String },
    /// no extra info required
    Identity,
    /// left node, right node, on left columns, on right columns, emit columns
    Join {
        on_left: Vec<Column>,
        on_right: Vec<Column>,
        project: Vec<Column>,
    },
    /// on left column, on right column, emit columns
    // currently unused
    #[allow(dead_code)]
    LeftJoin {
        on_left: Vec<Column>,
        on_right: Vec<Column>,
        project: Vec<Column>,
    },
    /// group columns
    // currently unused
    #[allow(dead_code)]
    Latest { group_by: Vec<Column> },
    /// emit columns
    Project {
        emit: Vec<Column>,
        literals: Vec<(String, DataType)>,
    },
    /// emit columns
    // currently unused
    #[allow(dead_code)]
    Permute { emit: Vec<Column> },
    /// emit columns left, emit columns right
    // currently unused
    #[allow(dead_code)]
    Union {
        emit_left: Vec<Column>,
        emit_right: Vec<Column>,
    },
    /// order function, group columns, k
    TopK {
        order: Option<Vec<(Column, OrderType)>>,
        group_by: Vec<Column>,
        k: usize,
        offset: usize,
    },
    /// reuse another node
    Reuse { node: MirNodeRef },
    /// leaf (reader) node, keys
    Leaf { node: MirNodeRef, keys: Vec<Column> },
}

impl MirNodeType {
    fn description(&self) -> String {
        format!("{:?}", self)
    }

    fn add_column(&mut self, c: Column) {
        match *self {
            MirNodeType::Aggregation { ref mut group_by, .. } => {
                group_by.push(c);
            }
            MirNodeType::Base { .. } => panic!("can't add columns to base nodes!"),
            MirNodeType::Extremum { ref mut group_by, .. } => {
                group_by.push(c);
            }
            MirNodeType::Filter { ref mut conditions } => {
                conditions.push(None);
            }
            MirNodeType::Join { ref mut project, .. } |
            MirNodeType::LeftJoin { ref mut project, .. } => {
                project.push(c);
            }
            MirNodeType::Project { ref mut emit, .. } |
            MirNodeType::Permute { ref mut emit } => {
                emit.push(c);
            }
            MirNodeType::Union { .. } => unimplemented!(),
            MirNodeType::TopK { ref mut group_by, .. } => {
                group_by.push(c);
            }
            _ => (),
        }
    }
}

impl Debug for MirNode {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f,
               "{}, {} ancestors, {} children",
               self.description(),
               self.ancestors.len(),
               self.children.len())
    }
}

impl Debug for MirNodeType {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        match *self {
            MirNodeType::Aggregation {
                ref on,
                ref group_by,
                ref kind,
            } => {
                let op_string = match *kind {
                    AggregationKind::COUNT => "|*|".into(),
                    AggregationKind::SUM => format!("𝛴({})", on.name.as_str()),
                };
                let group_cols = group_by
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{} γ[{}]", op_string, group_cols)

            }
            MirNodeType::Base { ref keys } => {
                write!(f,
                       "B [⚷: {}]",
                       keys.iter()
                           .map(|c| c.name.as_str())
                           .collect::<Vec<_>>()
                           .join(", "))
            }
            MirNodeType::Extremum {
                ref on,
                ref group_by,
                ref kind,
            } => {
                let op_string = match *kind {
                    ExtremumKind::MIN => format!("min({})", on.name.as_str()),
                    ExtremumKind::MAX => format!("max({})", on.name.as_str()),
                };
                let group_cols = group_by
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{} γ[{}]", op_string, group_cols)

            }
            MirNodeType::Filter { ref conditions } => {
                use regex::Regex;

                let escape = |s: &str| Regex::new("([<>])").unwrap().replace_all(s, "\\$1");
                write!(f,
                       "σ[{}]",
                       conditions
                           .iter()
                           .enumerate()
                           .filter_map(|(i, ref e)| match e.as_ref() {
                                           Some(&(ref op, ref x)) => {
                                               Some(format!("f{} {} {}",
                                                            i,
                                                            escape(&format!("{}", op)),
                                                            x))
                                           }
                                           None => None,
                                       })
                           .collect::<Vec<_>>()
                           .as_slice()
                           .join(", "))
            }
            MirNodeType::GroupConcat {
                ref on,
                ref separator,
            } => write!(f, "||([{}], \"{}\")", on.name, separator),
            MirNodeType::Identity => write!(f, "≡"),
            MirNodeType::Join {
                ref on_left,
                ref on_right,
                ref project,
            } => {
                let jc = on_left
                    .iter()
                    .zip(on_right)
                    .map(|(l, r)| format!("{}:{}", l.name, r.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f,
                       "⋈ [{} on {}]",
                       project
                           .iter()
                           .map(|c| c.name.as_str())
                           .collect::<Vec<_>>()
                           .join(", "),
                       jc)
            }
            MirNodeType::Leaf { ref keys, .. } => {
                let key_cols = keys.iter()
                    .map(|k| k.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "Leaf [⚷: {}]", key_cols)
            }
            MirNodeType::LeftJoin {
                ref on_left,
                ref on_right,
                ref project,
            } => {
                let jc = on_left
                    .iter()
                    .zip(on_right)
                    .map(|(l, r)| format!("{}:{}", l.name, r.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f,
                       "⋉ [{} on {}]",
                       project
                           .iter()
                           .map(|c| c.name.as_str())
                           .collect::<Vec<_>>()
                           .join(", "),
                       jc)
            }
            MirNodeType::Latest { ref group_by } => {
                let key_cols = group_by
                    .iter()
                    .map(|k| k.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "⧖ γ[{}]", key_cols)
            }
            MirNodeType::Permute { ref emit } => {
                write!(f,
                       "π [{}]",
                       emit.iter()
                           .map(|c| c.name.as_str())
                           .collect::<Vec<_>>()
                           .join(", "))
            }
            MirNodeType::Project {
                ref emit,
                ref literals,
            } => {
                write!(f,
                       "π [{}{}]",
                       emit.iter()
                           .map(|c| c.name.as_str())
                           .collect::<Vec<_>>()
                           .join(", "),
                       if literals.len() > 0 {
                           format!(", lit: {}",
                                   literals
                                       .iter()
                                       .map(|&(ref n, ref v)| format!("{}: {}", n, v))
                                       .collect::<Vec<_>>()
                                       .join(", "))
                       } else {
                           format!("")
                       })
            }
            MirNodeType::Reuse { ref node } => write!(f, "Reuse [{:#?}]", node),
            MirNodeType::TopK { ref order, ref k, .. } => write!(f, "TopK [k: {}, {:?}]", k, order),
            MirNodeType::Union {
                ref emit_left,
                ref emit_right,
            } => {
                let cols_left = emit_left
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                let cols_right = emit_right
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{} ⋃ {}", cols_left, cols_right)
            }
        }
    }
}

fn make_base_node(name: &str,
                  columns: &[Column],
                  pkey_columns: &Vec<Column>,
                  mut mig: &mut Migration)
                  -> FlowNode {
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let node = if pkey_columns.len() > 0 {
        let pkey_column_ids = pkey_columns
            .iter()
            .map(|pkc| {
                     //assert_eq!(pkc.table.as_ref().unwrap(), name);
                     columns.iter().position(|c| c == pkc).unwrap()
                 })
            .collect();
        mig.add_ingredient(name,
                           column_names.as_slice(),
                           ops::base::Base::new(pkey_column_ids))
    } else {
        mig.add_ingredient(name, column_names.as_slice(), ops::base::Base::default())
    };
    FlowNode::New(node)
}

fn make_filter_node(name: &str,
                    parent: MirNodeRef,
                    columns: &[Column],
                    conditions: &Vec<Option<(Operator, DataType)>>,
                    mut mig: &mut Migration)
                    -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let node =
        mig.add_ingredient(String::from(name),
                           column_names.as_slice(),
                           ops::filter::Filter::new(parent_na, conditions.as_slice()));
    FlowNode::New(node)
}

fn make_grouped_node(name: &str,
                     parent: MirNodeRef,
                     columns: &[Column],
                     on: &Column,
                     group_by: &Vec<Column>,
                     kind: GroupedNodeType,
                     mut mig: &mut Migration)
                     -> FlowNode {
    assert!(group_by.len() > 0);
    assert!(group_by.len() <= 4,
            format!("can't have >4 group columns due to compound key restrictions, {} needs {}",
                    name,
                    group_by.len()));

    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let over_col_indx = parent
        .borrow()
        .columns()
        .iter()
        .position(|c| c == on)
        .expect(&format!("\"over\" column {:?} not found in parent, which has {:?}",
                         on,
                         parent.borrow().columns()));
    let group_col_indx = group_by
        .iter()
        .map(|c| {
                 parent
                     .borrow()
                     .columns()
                     .iter()
                     .position(|pc| pc == c)
                     .unwrap()
             })
        .collect::<Vec<_>>();

    assert!(group_col_indx.len() > 0);

    let na = match kind {
        GroupedNodeType::Aggregation(agg) => {
            mig.add_ingredient(String::from(name),
                               column_names.as_slice(),
                               agg.over(parent_na, over_col_indx, group_col_indx.as_slice()))
        }
        GroupedNodeType::Extremum(extr) => {
            mig.add_ingredient(String::from(name),
                               column_names.as_slice(),
                               extr.over(parent_na, over_col_indx, group_col_indx.as_slice()))
        }
        GroupedNodeType::GroupConcat(sep) => {
            use ops::grouped::concat::{GroupConcat, TextComponent};

            let gc = GroupConcat::new(parent_na, vec![TextComponent::Column(over_col_indx)], sep);
            mig.add_ingredient(String::from(name), column_names.as_slice(), gc)
        }
    };
    FlowNode::New(na)
}


fn make_identity_node(name: &str,
                      parent: MirNodeRef,
                      columns: &[Column],
                      mut mig: &mut Migration)
                      -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let node = mig.add_ingredient(String::from(name),
                                  column_names.as_slice(),
                                  ops::identity::Identity::new(parent_na));
    FlowNode::New(node)
}

fn make_join_node(name: &str,
                  left: MirNodeRef,
                  right: MirNodeRef,
                  columns: &[Column],
                  on_left: &Vec<Column>,
                  on_right: &Vec<Column>,
                  proj_cols: &Vec<Column>,
                  kind: JoinType,
                  mut mig: &mut Migration)
                  -> FlowNode {
    use ops::join::JoinSource;

    assert_eq!(on_left.len(), on_right.len());

    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let projected_cols_left: Vec<Column> = left.borrow()
        .columns
        .iter()
        .filter(|c| proj_cols.contains(c))
        .cloned()
        .collect();
    let projected_cols_right: Vec<Column> = right
        .borrow()
        .columns
        .iter()
        .filter(|c| proj_cols.contains(c))
        .cloned()
        .collect();

    assert_eq!(projected_cols_left.len() + projected_cols_right.len(),
               proj_cols.len());

    let find_column_id = |n: &MirNodeRef, col: &Column| -> usize {
        n.borrow()
            .columns
            .iter()
            .position(|ref nc| *nc == col)
            .expect(&format!("column {:?} not found on {:?}!", col, n))
    };

    let join_config = proj_cols
        .iter()
        .map(|ref c| match on_left.iter().position(|ref lc| lc == c) {
                 Some(pos) => {
                     JoinSource::B(find_column_id(&left, c),
                                   find_column_id(&right, &on_right[pos]))
                 }
                 // WTF, rustfmt?
                 None => {
                     if projected_cols_left.contains(c) {
                         JoinSource::L(find_column_id(&left, c))
                     } else if projected_cols_right.contains(c) {
            JoinSource::R(find_column_id(&right, c))
        } else {
            panic!("Join column {:?} found in neither parent {:?} or {:?}?!",
                   c,
                   left,
                   right);
        }
                 }
             })
        .collect();

    let left_na = left.borrow().flow_node_addr().unwrap();
    let right_na = right.borrow().flow_node_addr().unwrap();

    let j = match kind {
        JoinType::Inner => Join::new(left_na, right_na, JoinType::Inner, join_config),
        JoinType::Left => Join::new(left_na, right_na, JoinType::Left, join_config),
    };
    let n = mig.add_ingredient(String::from(name), column_names.as_slice(), j);

    FlowNode::New(n)
}

fn make_latest_node(name: &str,
                    parent: MirNodeRef,
                    columns: &[Column],
                    group_by: &Vec<Column>,
                    mut mig: &mut Migration)
                    -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let group_col_indx = group_by
        .iter()
        .map(|c| {
                 parent
                     .borrow()
                     .columns()
                     .iter()
                     .position(|pc| pc == c)
                     .unwrap()
             })
        .collect::<Vec<_>>();

    let na = mig.add_ingredient(String::from(name),
                                column_names.as_slice(),
                                Latest::new(parent_na, group_col_indx));
    FlowNode::New(na)
}

fn make_permute_node(name: &str,
                     parent: MirNodeRef,
                     columns: &[Column],
                     emit: &Vec<Column>,
                     mut mig: &mut Migration)
                     -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let projected_column_ids = emit.iter()
        .map(|c| {
                 parent
                     .borrow()
                     .columns
                     .iter()
                     .position(|ref nc| *nc == c)
                     .unwrap()
             })
        .collect::<Vec<_>>();

    let n = mig.add_ingredient(String::from(name),
                               column_names.as_slice(),
                               Permute::new(parent_na, projected_column_ids.as_slice()));
    FlowNode::New(n)
}

fn make_project_node(name: &str,
                     parent: MirNodeRef,
                     columns: &[Column],
                     emit: &Vec<Column>,
                     literals: &Vec<(String, DataType)>,
                     mut mig: &mut Migration)
                     -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let projected_column_ids = emit.iter()
        .map(|c| {
            parent
                .borrow()
                .columns
                .iter()
                .position(|ref nc| *nc == c)
                .expect(&format!("column {:?} not found on {}",
                                 c,
                                 parent.borrow().versioned_name()))
        })
        .collect::<Vec<_>>();

    let (_, literal_values): (Vec<_>, Vec<_>) = literals.iter().cloned().unzip();

    let n = mig.add_ingredient(String::from(name),
                               column_names.as_slice(),
                               Project::new(parent_na,
                                            projected_column_ids.as_slice(),
                                            Some(literal_values)));
    FlowNode::New(n)
}

fn make_topk_node(name: &str,
                  parent: MirNodeRef,
                  columns: &[Column],
                  order: &Option<Vec<(Column, OrderType)>>,
                  group_by: &Vec<Column>,
                  k: usize,
                  offset: usize,
                  mut mig: &mut Migration)
                  -> FlowNode {
    use std::cmp::Ordering;
    use std::sync::Arc;

    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let group_by_indx = if group_by.is_empty() {
        // no query parameters, so we index on the first column
        vec![0 as usize]
    } else {
        group_by
            .iter()
            .map(|c| {
                     parent
                         .borrow()
                         .columns
                         .iter()
                         .position(|ref nc| *nc == c)
                         .unwrap()
                 })
            .collect::<Vec<_>>()
    };

    let cmp_rows = match *order {
        Some(ref o) => {
            assert_eq!(offset, 0); // Non-zero offset not supported

            let columns: Vec<_> = o.iter()
                .map(|&(ref c, ref order_type)| {
                    (order_type.clone(),
                     parent
                         .borrow()
                         .columns()
                         .iter()
                         .position(|pc| pc == c)
                         .unwrap())
                })
                .collect();

            Box::new(move |a: &&Arc<Vec<DataType>>, b: &&Arc<Vec<DataType>>| {
                let mut ret = Ordering::Equal;
                for &(ref o, c) in columns.iter() {
                    ret = match *o {
                        OrderType::OrderAscending => a[c].cmp(&b[c]),
                        OrderType::OrderDescending => b[c].cmp(&a[c]),
                    };
                    if ret != Ordering::Equal {
                        return ret;
                    }
                }
                ret
            }) as Box<Fn(&&_, &&_) -> Ordering + Send + 'static>
        }
        None => {
            Box::new(|_: &&_, _: &&_| Ordering::Equal) as
            Box<Fn(&&_, &&_) -> Ordering + Send + 'static>
        }
    };

    // make the new operator and record its metadata
    let na =
        mig.add_ingredient(String::from(name),
                           column_names.as_slice(),
                           ops::topk::TopK::new(parent_na, cmp_rows, group_by_indx, k));
    FlowNode::New(na)
}

fn materialize_leaf_node(node: &MirNodeRef, key_cols: &Vec<Column>, mut mig: &mut Migration) {
    let na = node.borrow().flow_node_addr().unwrap();

    // we must add a new reader for this query. This also requires adding an identity node (at
    // least currently), since a node can only have a single associated reader. However, the
    // identity node exists at the MIR level, so we don't need to consider it here, as it has
    // already been added.

    // TODO(malte): consider the case when the projected columns need reordering

    if !key_cols.is_empty() {
        // TODO(malte): this does not yet cover the case when there are multiple query
        // parameters, which requires compound key support on Reader nodes.
        //assert_eq!(key_cols.len(), 1);
        let first_key_col_id = node.borrow()
            .columns()
            .iter()
            .position(|pc| pc == key_cols.iter().next().unwrap())
            .unwrap();
        mig.maintain(na, first_key_col_id);
    } else {
        // if no key specified, default to the first column
        mig.maintain(na, 0);
    }
}
