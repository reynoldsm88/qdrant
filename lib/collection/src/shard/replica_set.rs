use std::cmp;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use async_recursion::async_recursion;
use futures::future::{try_join, try_join_all};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use segment::types::{
    ExtendedPointId, Filter, ScoredPoint, WithPayload, WithPayloadInterface, WithVector,
};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::RwLock;

use super::local_shard::{drop_and_delete_from_disk, LocalShard};
use super::remote_shard::RemoteShard;
use super::{create_shard_dir, CollectionId, PeerId, ShardId, ShardOperation};
use crate::config::CollectionConfig;
use crate::operations::types::{
    CollectionError, CollectionInfo, CollectionResult, CountRequest, CountResult, PointRequest,
    Record, SearchRequestBatch, UpdateResult,
};
use crate::operations::CollectionUpdateOperations;
use crate::save_on_disk::SaveOnDisk;
use crate::shard::forward_proxy_shard::ForwardProxyShard;
use crate::shard::Shard;
use crate::shard::Shard::{ForwardProxy, Local, Remote};

pub type IsActive = bool;
pub type OnPeerFailure =
    Arc<dyn Fn(PeerId, ShardId) -> Box<dyn Future<Output = ()> + Send> + Send + Sync>;

const READ_REMOTE_REPLICAS: u32 = 2;

const REPLICA_STATE_FILE: &str = "replica_state";

/// Represents a change in replica set, due to scaling of `replication_factor`
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Clone)]
pub enum Change {
    Add {
        shard: ShardId,
        to: PeerId,
        /// A peer which sends the shard data to the newly added peer
        from: PeerId,
    },
    Remove(ShardId, PeerId),
}

/// A set of shard replicas.
/// Handles operations so that the state is consistent across all the replicas of the shard.
/// Prefers local shard for read-only operations.
/// Perform updates on all replicas and report error if there is at least one failure.
///
/// `ReplicaSet` should always have >= 2 replicas.
///  If a user decreases replication factor to 1 - it should be converted to just `Local` or `Remote` shard.
pub struct ReplicaSet {
    pub(crate) shard_id: ShardId,
    pub(crate) this_peer_id: PeerId,
    pub(crate) local: Option<Box<Shard>>, // Abstract Shard to be able to use a Proxy during replication
    pub(crate) remotes: Vec<RemoteShard>,
    pub(crate) replica_state: SaveOnDisk<HashMap<PeerId, IsActive>>,
    /// Number of remote replicas to send read requests to.
    /// If actual number of peers is less than this, then read request will be sent to all of them.
    read_remote_replicas: u32,
    notify_peer_failure_cb: OnPeerFailure,
    outgoing_sync_count: u32,
}

impl ReplicaSet {
    #[allow(clippy::too_many_arguments)]
    pub async fn build(
        shard_id: ShardId,
        collection_id: CollectionId,
        this_peer_id: PeerId,
        local: bool,
        remotes: HashSet<PeerId>,
        on_peer_failure: OnPeerFailure,
        collection_path: &Path,
        shared_config: Arc<RwLock<CollectionConfig>>,
    ) -> CollectionResult<Self> {
        let shard_path = create_shard_dir(collection_path, shard_id).await?;
        let local = if local {
            let shard =
                LocalShard::build(shard_id, collection_id, &shard_path, shared_config.clone())
                    .await?;
            Some(Box::new(Local(shard)))
        } else {
            None
        };
        let mut replica_state: SaveOnDisk<HashMap<PeerId, IsActive>> =
            SaveOnDisk::load_or_init(shard_path.join(REPLICA_STATE_FILE))?;
        replica_state.write(|rs| {
            if local.is_some() {
                rs.insert(this_peer_id, true);
            }
            for peer in remotes {
                rs.insert(peer, true);
            }
        })?;
        Ok(Self {
            shard_id,
            this_peer_id,
            local,
            // TODO: Initialize remote shards
            // This requires logic to store several peer ids in remote shard file
            remotes: Vec::new(),
            replica_state,
            // TODO: move to collection config
            read_remote_replicas: READ_REMOTE_REPLICAS,
            notify_peer_failure_cb: on_peer_failure,
            outgoing_sync_count: 0,
        })
    }
    pub async fn notify_peer_failure(&self, peer_id: PeerId) {
        Box::into_pin(self.notify_peer_failure_cb.deref()(peer_id, self.shard_id)).await
    }

    pub fn peer_ids(&self) -> Vec<PeerId> {
        let mut peer_ids: Vec<_> = self.remotes.iter().map(|r| r.peer_id).collect();
        if self.local.is_some() {
            peer_ids.push(self.this_peer_id)
        }
        peer_ids
    }

    pub fn set_active(&mut self, peer_id: &PeerId, active: bool) -> CollectionResult<()> {
        self.replica_state.write_with_res(|rs| {
            *rs.get_mut(peer_id)
                .ok_or_else(|| CollectionError::NotFound {
                    what: format!("Shard {} replica on peer {peer_id}", self.shard_id),
                })? = active;
            Ok::<(), CollectionError>(())
        })?;
        Ok(())
    }

    pub fn add_inactive_replica_state(&mut self, peer_id: &PeerId) -> CollectionResult<()> {
        self.replica_state
            .write_with_res(|rs| match rs.insert(*peer_id, false) {
                Some(_) => Err(CollectionError::service_error(format!(
                    "replica for peer {} is already registered",
                    peer_id
                ))),
                None => Ok(()),
            })?;
        Ok(())
    }

    pub async fn apply_state(
        &mut self,
        replicas: HashMap<PeerId, IsActive>,
    ) -> CollectionResult<()> {
        let removed_peers = self
            .replica_state
            .keys()
            .filter(|peer_id| !replicas.contains_key(peer_id))
            .copied()
            .collect::<Vec<_>>();
        for peer_id in removed_peers {
            if peer_id == self.this_peer_id {
                if let Some(mut shard) = self.local.take() {
                    shard.before_drop().await;
                    match *shard {
                        Local(local) => drop_and_delete_from_disk(local).await?,
                        ForwardProxy(forward) => {
                            drop_and_delete_from_disk(forward.wrapped_shard).await?
                        }
                        Remote(_) | Shard::Proxy(_) | Shard::ReplicaSet(_) => {
                            return Err(CollectionError::service_error(
                                "Unexpected shard in replica set".to_string(),
                            ))
                        }
                    }
                } else {
                    debug_assert!(false, "inconsistent `replica_set` map with actual shards")
                }
            } else if let Some(_remote_shard) =
                &mut self.remotes.iter().find(|rs| rs.peer_id == peer_id)
            {
                todo!("remote_shard.remove_peer(peer_id)")
            }
            self.replica_state.remove(&peer_id);
        }
        for (peer_id, is_active) in replicas {
            if let Some(state) = self.replica_state.get_mut(&peer_id) {
                *state = is_active;
            } else if peer_id == self.this_peer_id {
                todo!("clone replica from another peer or log error that it should be cloned with normal operation")
            } else {
                todo!("Add remote replica")
            }
        }
        self.replica_state.save()?;
        Ok(())
    }

    /// Check whether a peer is registered as `active`.
    /// Unknown peers are not active.
    pub fn peer_is_active(&self, peer_id: &PeerId) -> bool {
        self.replica_state.get(peer_id) == Some(&true)
    }

    /// Execute read operation on replica set:
    /// 1 - Prefer local replica
    /// 2 - Otherwise uses `read_fan_out_ratio` to compute list of active remote shards.
    /// 3 - Fallbacks to all remaining shards if the optimisations fails.
    /// It does not report failing peer_ids to the consensus.
    pub async fn execute_read_operation<'a, F, Fut, Res>(&'a self, read: F) -> CollectionResult<Res>
    where
        F: Fn(&'a (dyn ShardOperation + Send + Sync)) -> Fut,
        Fut: Future<Output = CollectionResult<Res>>,
    {
        // 1 - prefer the local shard if it is active
        if let Some(local) = &self.local {
            if self.peer_is_active(&self.this_peer_id) {
                if let ok @ Ok(_) = read(local.get()).await {
                    return ok;
                }
            }
        }

        // 2 - try a subset of active remote shards in parallel for fast response
        let active_remote_shards: Vec<_> = self
            .remotes
            .iter()
            .filter(|rs| self.peer_is_active(&rs.peer_id))
            .collect();

        if active_remote_shards.is_empty() {
            return Err(CollectionError::service_error(format!(
                "The replica set for shard {} on peer {} has no active replica",
                self.shard_id, self.this_peer_id
            )));
        }

        let fan_out_selection = cmp::min(
            active_remote_shards.len(),
            self.read_remote_replicas as usize,
        );

        let mut futures = FuturesUnordered::new();
        for remote in &active_remote_shards[0..fan_out_selection] {
            let fut = read(*remote);
            futures.push(fut);
        }

        // shortcut at first successful result
        let mut captured_error = None;
        while let Some(result) = futures.next().await {
            match result {
                Ok(res) => return Ok(res),
                err @ Err(_) => captured_error = Some(err), // capture error for possible error reporting
            }
        }
        debug_assert!(
            captured_error.is_some(),
            "there must be at least one failure"
        );

        // 3 - fallback to remaining remote shards as last chance
        let mut futures = FuturesUnordered::new();
        for remote in &active_remote_shards[fan_out_selection..] {
            let fut = read(*remote);
            futures.push(fut);
        }

        // shortcut at first successful result
        while let Some(result) = futures.next().await {
            if let ok @ Ok(_) = result {
                return ok;
            }
        }
        captured_error.expect("at this point `captured_error` must be defined by construction")
    }

    pub async fn remove_replica(&mut self, peer_id: &PeerId) -> CollectionResult<()> {
        if peer_id == &self.this_peer_id {
            // remove local shard
            if let Some(mut local) = self.local.take() {
                local.before_drop().await;
                match *local {
                    Local(local) => drop_and_delete_from_disk(local).await?,
                    ForwardProxy(forward) => {
                        drop_and_delete_from_disk(forward.wrapped_shard).await?
                    }
                    Remote(_) | Shard::Proxy(_) | Shard::ReplicaSet(_) => {
                        return Err(CollectionError::service_error(
                            "Unexpected shard in replica set".to_string(),
                        ))
                    }
                }
                Ok(())
            } else {
                Err(CollectionError::service_error(format!(
                    "replica set {} should contain a local shard",
                    self.shard_id
                )))
            }
        } else {
            let remote_index = self.remotes.iter().position(|r| &r.peer_id == peer_id);
            if let Some(remote_index) = remote_index {
                self.remotes.remove(remote_index);
                Ok(())
            } else {
                Err(CollectionError::service_error(format!(
                    "replica set {} should contain a remote shard for peer {}",
                    self.shard_id, peer_id
                )))
            }
        }
    }

    pub(crate) async fn on_optimizer_config_update(&self) -> CollectionResult<()> {
        if let Some(_shard) = &self.local {
            // shard.on_optimizer_config_update().await
            todo!("optimize config update on local")
        } else {
            Ok(())
        }
    }

    #[async_recursion]
    pub(crate) async fn before_drop(&mut self) {
        if let Some(shard) = &mut self.local {
            shard.before_drop().await
        }
    }

    pub(crate) fn shard_distribution(&self) -> Vec<(ShardId, PeerId)> {
        self.local
            .iter()
            .map(|_shard| (self.shard_id, self.this_peer_id))
            .chain(
                self.remotes
                    .iter()
                    .map(|shard| (self.shard_id, shard.peer_id)),
            )
            .collect()
    }

    pub fn proxify_local(&mut self, remote_shard: RemoteShard) -> CollectionResult<()> {
        match self.local.take().map(|b| *b) {
            Some(Local(local)) => {
                let proxy_shard = ForwardProxyShard::new(local, remote_shard);
                let _ = self.local.insert(Box::new(ForwardProxy(proxy_shard)));
                Ok(())
            }
            Some(shard) => Err(CollectionError::service_error(format!(
                "cannot proxy {} shard",
                shard.variant_name()
            ))),
            None => Err(CollectionError::service_error(
                "cannot proxy absent local shard".to_string(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl ShardOperation for ReplicaSet {
    async fn update(
        &self,
        operation: CollectionUpdateOperations,
        wait: bool,
    ) -> CollectionResult<UpdateResult> {
        // target all remote peers that are active
        let active_remote_shards: Vec<_> = self
            .remotes
            .iter()
            .filter(|rs| self.peer_is_active(&rs.peer_id))
            .collect();

        // local is defined AND the peer itself is active
        let local_is_active = self.local.is_some() && self.peer_is_active(&self.this_peer_id);

        if active_remote_shards.is_empty() && !local_is_active {
            return Err(CollectionError::service_error(format!(
                "The replica set for shard {} on peer {} has no active replica",
                self.shard_id, self.this_peer_id
            )));
        }

        let mut remote_futures = Vec::new();
        for remote in active_remote_shards {
            let op = operation.clone();
            remote_futures.push(async move {
                remote
                    .update(op, wait)
                    .await
                    .map_err(|err| (remote.peer_id, err))
            });
        }

        let all_res = match &self.local {
            Some(local) if self.peer_is_active(&self.this_peer_id) => {
                let local_update = async move {
                    local
                        .get()
                        .update(operation.clone(), wait)
                        .await
                        .map_err(|err| (self.this_peer_id, err))
                };
                let remote_updates = try_join_all(remote_futures);

                // run local and remote shards read concurrently
                try_join(remote_updates, local_update)
                    .await
                    .map(|(remote_res, _local_res)| remote_res)
            }
            _ => try_join_all(remote_futures).await,
        };

        match all_res {
            Ok(results) => {
                // return first result
                match results.into_iter().next() {
                    None => Err(CollectionError::service_error(format!(
                        "None of the replicas replied for Replica set {} on peer {}",
                        self.shard_id, self.this_peer_id
                    ))),
                    Some(res) => Ok(res),
                }
            }
            Err((peer_id, err)) => {
                // report failing `peer_id`
                self.notify_peer_failure(peer_id).await;
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn scroll_by(
        &self,
        offset: Option<ExtendedPointId>,
        limit: usize,
        with_payload_interface: &WithPayloadInterface,
        with_vector: &WithVector,
        filter: Option<&Filter>,
    ) -> CollectionResult<Vec<Record>> {
        self.execute_read_operation(|shard| {
            shard.scroll_by(offset, limit, with_payload_interface, with_vector, filter)
        })
        .await
    }

    async fn info(&self) -> CollectionResult<CollectionInfo> {
        self.execute_read_operation(|shard| shard.info()).await
    }

    async fn search(
        &self,
        request: Arc<SearchRequestBatch>,
        search_runtime_handle: &Handle,
    ) -> CollectionResult<Vec<Vec<ScoredPoint>>> {
        self.execute_read_operation(|shard| shard.search(request.clone(), search_runtime_handle))
            .await
    }

    async fn count(&self, request: Arc<CountRequest>) -> CollectionResult<CountResult> {
        self.execute_read_operation(|shard| shard.count(request.clone()))
            .await
    }

    async fn retrieve(
        &self,
        request: Arc<PointRequest>,
        with_payload: &WithPayload,
        with_vector: &WithVector,
    ) -> CollectionResult<Vec<Record>> {
        self.execute_read_operation(|shard| {
            shard.retrieve(request.clone(), with_payload, with_vector)
        })
        .await
    }
}
