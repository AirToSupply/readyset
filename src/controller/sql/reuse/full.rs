use controller::sql::reuse::{ReuseConfiguration, ReuseType};
use controller::sql::query_graph::QueryGraph;
use mir::query::MirQuery;

use std::vec::Vec;
use std::collections::HashMap;

/// Full reuse algorithm
/// Implementation of reuse algorithm that checks all available reuse options.
/// This algorithm yields maximum reuse, since it checks all possible options.
pub struct Full;

impl ReuseConfiguration for Full {
    fn reuse_candidates<'a>(
        _qg: &QueryGraph,
        query_graphs: &'a HashMap<u64, (QueryGraph, MirQuery)>,
    ) -> Vec<(ReuseType, (u64, &'a QueryGraph))> {
        // sort keys to make reuse deterministic
        let mut sorted_keys: Vec<u64> = query_graphs.keys().cloned().collect();
        sorted_keys.sort();
        sorted_keys
            .iter()
            .map(|k| (ReuseType::DirectExtension, (k.clone(), &query_graphs[k].0)))
            .collect()
    }
}
