use channel::rpc::RpcClient;

use dataflow::prelude::*;
use dataflow::backlog::{self, ReadHandle};
use dataflow::{self, checktable, LocalBypass, Readers};

use std::net::SocketAddr;

/// A request to read a specific key.
#[derive(Serialize, Deserialize, Debug)]
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
}

#[derive(Serialize, Deserialize)]
pub(crate) enum LocalOrNot<T> {
    Local(LocalBypass<T>),
    Not(T),
}

impl<T> LocalOrNot<T> {
    pub(crate) fn is_local(&self) -> bool {
        if let LocalOrNot::Local(..) = *self {
            true
        } else {
            false
        }
    }

    pub(crate) fn make(t: T, local: bool) -> Self {
        if local {
            LocalOrNot::Local(LocalBypass::make(Box::new(t)))
        } else {
            LocalOrNot::Not(t)
        }
    }

    pub(crate) unsafe fn take(self) -> T {
        match self {
            LocalOrNot::Local(l) => *l.take(),
            LocalOrNot::Not(t) => t,
        }
    }
}

/// The contents of a specific key
#[derive(Serialize, Deserialize, Debug)]
pub enum ReadReply {
    /// Read normally
    Normal(Vec<Result<Datas, ()>>),
    /// Read and got checktable tokens
    WithToken(Vec<Result<(Datas, checktable::Token), ()>>),
}

/// Serializeable version of a `RemoteGetter`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteGetterBuilder {
    pub(crate) node: NodeIndex,
    pub(crate) shards: Vec<(SocketAddr, bool)>,
}

impl RemoteGetterBuilder {
    /// Build a `RemoteGetter` out of a `RemoteGetterBuilder`
    pub fn build(self) -> RemoteGetter {
        RemoteGetter {
            node: self.node,
            shards: self.shards
                .iter()
                .map(|&(ref addr, is_local)| RpcClient::connect(addr, is_local).unwrap())
                .collect(),
        }
    }
}

/// Struct to query the contents of a materialized view.
pub struct RemoteGetter {
    node: NodeIndex,
    shards: Vec<RpcClient<LocalOrNot<ReadQuery>, LocalOrNot<ReadReply>>>,
}

impl RemoteGetter {
    /// Query for the results for the given keys, optionally blocking if it is not yet available.
    pub fn multi_lookup(&mut self, keys: Vec<DataType>, block: bool) -> Vec<Result<Datas, ()>> {
        if self.shards.len() == 1 {
            let shard = &mut self.shards[0];
            let is_local = shard.is_local();
            let reply = shard
                .send(&LocalOrNot::make(
                    ReadQuery::Normal {
                        target: (self.node, 0),
                        keys,
                        block,
                    },
                    is_local,
                ))
                .unwrap();
            match unsafe { reply.take() } {
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
                .filter(|&(_, ref keys)| !keys.is_empty())
                .flat_map(|(shardi, keys)| {
                    let shard = &mut self.shards[shardi];
                    let is_local = shard.is_local();
                    let reply = shard
                        .send(&LocalOrNot::make(
                            ReadQuery::Normal {
                                target: (self.node, shardi),
                                keys,
                                block,
                            },
                            is_local,
                        ))
                        .unwrap();

                    match unsafe { reply.take() } {
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
            let shard = &mut self.shards[0];
            let is_local = shard.is_local();
            let reply = shard
                .send(&LocalOrNot::make(
                    ReadQuery::WithToken {
                        target: (self.node, 0),
                        keys,
                    },
                    is_local,
                ))
                .unwrap();
            match unsafe { reply.take() } {
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
                .flat_map(|(shardi, keys)| {
                    let shard = &mut self.shards[shardi];
                    let is_local = shard.is_local();
                    let reply = shard
                        .send(&LocalOrNot::make(
                            ReadQuery::WithToken {
                                target: (self.node, shardi),
                                keys,
                            },
                            is_local,
                        ))
                        .unwrap();

                    match unsafe { reply.take() } {
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

#[allow(unused)]
impl Getter {
    pub(crate) fn new(
        node: NodeIndex,
        sharded: bool,
        readers: &Readers,
        ingredients: &Graph,
    ) -> Option<Self> {
        let rh = if sharded {
            let vr = readers.lock().unwrap();

            let shards = ingredients[node].sharded_by().shards();
            let mut getters = Vec::with_capacity(shards);
            for shard in 0..shards {
                match vr.get(&(node, shard)).cloned() {
                    Some((rh, _)) => getters.push(Some(rh)),
                    None => return None,
                }
            }
            ReadHandle::Sharded(getters)
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
        F: FnMut(&[Vec<DataType>]) -> T,
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
