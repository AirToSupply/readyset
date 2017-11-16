use channel::rpc::RpcClient;

use dataflow::prelude::*;
use dataflow::backlog::{self, ReadHandle};
use dataflow::{self, checktable, Readers};

use std::sync::Arc;
use std::net::SocketAddr;

use arrayvec::ArrayVec;

/// A request to read a specific key.
#[derive(Serialize, Deserialize)]
pub enum ReadQuery {
    /// Read normally
    Normal {
        /// Where to read from
        target: (NodeIndex, usize),
        /// Keys to read with
        keys: Vec<DataType>,
        /// Whether to block if a partial replay is triggered
        block: bool,
    },
    /// Read and also get a checktable token
    WithToken {
        /// Where to read from
        target: (NodeIndex, usize),
        /// Keys to read with
        keys: Vec<DataType>,
    },
    /// Size of reader
    Size {
        /// Where to read from
        target: (NodeIndex, usize),
    },
}

/// The contents of a specific key
#[derive(Serialize, Deserialize)]
pub enum ReadReply {
    /// Read normally
    Normal(Vec<Result<Datas, ()>>),
    /// Read and got checktable tokens
    WithToken(Vec<Result<(Datas, checktable::Token), ()>>),
    /// Size of reader
    Size(usize),
}

/// Serializeable version of a `RemoteGetter`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteGetterBuilder {
    pub(crate) node: NodeIndex,
    pub(crate) shards: Vec<SocketAddr>,
}

impl RemoteGetterBuilder {
    /// Build a `RemoteGetter` out of a `RemoteGetterBuilder`
    pub fn build(self) -> RemoteGetter {
        RemoteGetter {
            node: self.node,
            shards: self.shards
                .iter()
                .map(|addr| RpcClient::connect(addr).unwrap())
                .collect(),
        }
    }
}

/// Struct to query the contents of a materialized view.
pub struct RemoteGetter {
    node: NodeIndex,
    shards: Vec<RpcClient<ReadQuery, ReadReply>>,
}

impl RemoteGetter {
    /// Query the size of the reader
    pub fn len(&mut self) -> usize {
        let reply = self.shards[0]
                .send(&ReadQuery::Size {
                    target: (self.node, 0),
                })
                .unwrap();
        match reply {
            ReadReply::Size(size) => size,
            _ => unreachable!(),
        }
    }

    /// Query for the results for the given keys, optionally blocking if it is not yet available.
    pub fn multi_lookup(&mut self, keys: Vec<DataType>, block: bool) -> Vec<Result<Datas, ()>> {
        if self.shards.len() == 1 {
            let reply = self.shards[0]
                .send(&ReadQuery::Normal {
                    target: (self.node, 0),
                    keys,
                    block,
                })
                .unwrap();
            match reply {
                ReadReply::Normal(rows) => rows,
                _ => unreachable!(),
            }
        } else {
            let mut shard_queries = vec![Vec::new(); self.shards.len()];
            for key in keys {
                let shard = dataflow::shard_by(&key, self.shards.len());
                shard_queries[shard].push(key);
            }

            shard_queries
                .into_iter()
                .enumerate()
                .flat_map(|(shard, keys)| {
                    let reply = self.shards[shard]
                        .send(&ReadQuery::Normal {
                            target: (self.node, shard),
                            keys,
                            block,
                        })
                        .unwrap();

                    match reply {
                        ReadReply::Normal(rows) => rows,
                        _ => unreachable!(),
                    }
                })
                .collect()
        }
    }

    /// Query for the results for the given keys, optionally blocking if it is not yet available.
    pub fn transactional_multi_lookup(
        &mut self,
        keys: Vec<DataType>,
    ) -> Vec<Result<(Datas, checktable::Token), ()>> {
        if self.shards.len() == 1 {
            let reply = self.shards[0]
                .send(&ReadQuery::WithToken {
                    target: (self.node, 0),
                    keys,
                })
                .unwrap();
            match reply {
                ReadReply::WithToken(rows) => rows,
                _ => unreachable!(),
            }
        } else {
            let mut shard_queries = vec![Vec::new(); self.shards.len()];
            for key in keys {
                let shard = dataflow::shard_by(&key, self.shards.len());
                shard_queries[shard].push(key);
            }

            shard_queries
                .into_iter()
                .enumerate()
                .flat_map(|(shard, keys)| {
                    let reply = self.shards[shard]
                        .send(&ReadQuery::WithToken {
                            target: (self.node, shard),
                            keys,
                        })
                        .unwrap();

                    match reply {
                        ReadReply::WithToken(rows) => rows,
                        _ => unreachable!(),
                    }
                })
                .collect()
        }
    }

    /// Lookup a single key.
    pub fn lookup(&mut self, key: &DataType, block: bool) -> Result<Datas, ()> {
        // TODO: Optimized version of this function?
        self.multi_lookup(vec![key.clone()], block)
            .into_iter()
            .next()
            .unwrap()
    }

    /// Do a transactional lookup for a single key.
    pub fn transactional_lookup(
        &mut self,
        key: &DataType,
    ) -> Result<(Datas, checktable::Token), ()> {
        // TODO: Optimized version of this function?
        self.transactional_multi_lookup(vec![key.clone()])
            .into_iter()
            .next()
            .unwrap()
    }
}

/// A handle for looking up results in a materialized view.
pub struct Getter {
    pub(crate) generator: Option<checktable::TokenGenerator>,
    pub(crate) handle: backlog::ReadHandle,
    last_ts: i64,
}

impl Getter {
    pub(crate) fn new(
        node: NodeIndex,
        sharded: bool,
        readers: &Readers,
        ingredients: &Graph,
    ) -> Option<Self> {
        let rh = if sharded {
            let vr = readers.lock().unwrap();

            let mut array = ArrayVec::new();
            for shard in 0..dataflow::SHARDS {
                match vr.get(&(node, shard)).cloned() {
                    Some((rh, _)) => array.push(Some(rh)),
                    None => return None,
                }
            }
            ReadHandle::Sharded(array)
        } else {
            let vr = readers.lock().unwrap();
            match vr.get(&(node, 0)).cloned() {
                Some((rh, _)) => ReadHandle::Singleton(Some(rh)),
                None => return None,
            }
        };

        let gen = ingredients[node]
            .with_reader(|r| r)
            .and_then(|r| r.token_generator().cloned());
        assert_eq!(ingredients[node].is_transactional(), gen.is_some());
        Some(Getter {
            generator: gen,
            handle: rh,
            last_ts: i64::min_value(),
        })
    }

    /// Returns the number of populated keys
    pub fn len(&self) -> usize {
        self.handle.len()
    }

    /// Returns true if this getter supports transactional reads.
    pub fn supports_transactions(&self) -> bool {
        self.generator.is_some()
    }

    /// Query for the results for the given key, and apply the given callback to matching rows.
    ///
    /// If `block` is `true`, this function will block if the results for the given key are not yet
    /// available.
    ///
    /// If you need to clone values out of the returned rows, make sure to use
    /// `DataType::deep_clone` to avoid contention on internally de-duplicated strings!
    pub fn lookup_map<F, T>(&self, q: &DataType, mut f: F, block: bool) -> Result<Option<T>, ()>
    where
        F: FnMut(&[Arc<Vec<DataType>>]) -> T,
    {
        self.handle.find_and(q, |rs| f(&rs[..]), block).map(|r| r.0)
    }

    /// Query for the results for the given key, optionally blocking if it is not yet available.
    pub fn lookup(&self, q: &DataType, block: bool) -> Result<Datas, ()> {
        self.lookup_map(
            q,
            |rs| {
                rs.into_iter()
                    .map(|r| r.iter().map(|v| v.deep_clone()).collect())
                    .collect()
            },
            block,
        ).map(|r| r.unwrap_or_else(Vec::new))
    }

    /// Transactionally query for the given key, blocking if it is not yet available.
    pub fn transactional_lookup(&mut self, q: &DataType) -> Result<(Datas, checktable::Token), ()> {
        match self.generator {
            None => Err(()),
            Some(ref g) => {
                loop {
                    let res = self.handle.find_and(
                        q,
                        |rs| {
                            rs.into_iter()
                                .map(|v| (&**v).into_iter().map(|v| v.deep_clone()).collect())
                                .collect()
                        },
                        true,
                    );
                    match res {
                        Ok((_, ts)) if ts < self.last_ts => {
                            // we must have read from a different shard that is not yet up-to-date
                            // to our last read. this is *extremely* unlikely: you would have to
                            // issue two reads to different shards *between* the barrier and swap
                            // inside Reader nodes, which is only a span of a handful of
                            // instructions. But it is *possible*.
                        }
                        Ok((res, ts)) => {
                            self.last_ts = ts;
                            let token = g.generate(ts, q.clone());
                            break Ok((res.unwrap_or_else(Vec::new), token));
                        }
                        Err(e) => break Err(e),
                    }
                }
            }
        }
    }
}
