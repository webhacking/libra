// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counters,
    executor_proxy::ExecutorProxyTrait,
    peer_manager::{PeerManager, PeerScoreUpdateType},
    LedgerInfo, PeerId,
};
use failure::prelude::*;
use futures::{
    channel::{mpsc, oneshot},
    stream::{futures_unordered::FuturesUnordered, select_all},
    StreamExt,
};
use libra_config::config::RoleType;
use libra_config::config::StateSyncConfig;
use libra_logger::prelude::*;
use libra_types::crypto_proxies::ValidatorChangeEventWithProof;
use libra_types::{
    crypto_proxies::LedgerInfoWithSignatures, transaction::TransactionListWithProof,
};
use network::{
    proto::{GetChunkRequest, GetChunkResponse, StateSynchronizerMsg, StateSynchronizerMsg_oneof},
    validator_network::{Event, StateSynchronizerEvents, StateSynchronizerSender},
};
use std::{
    collections::HashMap,
    convert::TryInto,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::timer::Interval;

pub(crate) struct SyncRequest {
    // The Result value returned to the caller is Error in case the StateSynchronizer failed to
    // reach the target (the LI in the storage remains unchanged as if nothing happened).
    pub callback: oneshot::Sender<Result<()>>,
    pub target: LedgerInfoWithSignatures,
}

pub(crate) struct EpochRetrievalRequest {
    pub start_epoch: u64,
    pub callback: oneshot::Sender<Result<ValidatorChangeEventWithProof>>,
}

/// message used by StateSyncClient for communication with Coordinator
pub(crate) enum CoordinatorMessage {
    // used to initiate new sync
    Request(SyncRequest),
    // used to notify about new txn commit
    Commit(u64),
    GetState(oneshot::Sender<u64>),
    // used to generate epoch proof
    GetEpochProof(EpochRetrievalRequest),
}

/// used to coordinate synchronization process
/// handles external sync requests and drives synchronization with remote peers
pub(crate) struct SyncCoordinator<T> {
    // used to process client requests
    client_events: mpsc::UnboundedReceiver<CoordinatorMessage>,
    // last committed version that validator is aware of
    known_version: u64,
    // config
    config: StateSyncConfig,
    // role of node
    role: RoleType,
    // peers used for synchronization. TBD: value is meta information about peer sync quality
    peer_manager: PeerManager,
    // Optional sync request to be called when the target sync is reached
    sync_request: Option<SyncRequest>,
    // queue of incoming long polling requests
    // peer will be notified about new chunk of transactions if it's available before expiry time
    // value format is (expiration_time, known_version, limit)
    subscriptions: HashMap<PeerId, (SystemTime, u64, u64)>,
    executor_proxy: T,
}

impl<T: ExecutorProxyTrait> SyncCoordinator<T> {
    pub fn new(
        client_events: mpsc::UnboundedReceiver<CoordinatorMessage>,
        role: RoleType,
        config: StateSyncConfig,
        executor_proxy: T,
    ) -> Self {
        let upstream_peers: Vec<_> = config
            .upstream_peers
            .upstream_peers
            .iter()
            .map(|peer_id_str| {
                PeerId::from_str(peer_id_str).unwrap_or_else(|_| {
                    panic!("Failed to parse peer_id from string: {}", peer_id_str)
                })
            })
            .collect();
        Self {
            client_events,
            known_version: 0,
            config,
            role,
            peer_manager: PeerManager::new(upstream_peers),
            subscriptions: HashMap::new(),
            sync_request: None,
            executor_proxy,
        }
    }

    /// main routine. starts sync coordinator that listens for CoordinatorMsg
    pub async fn start(mut self, network: Vec<(StateSynchronizerSender, StateSynchronizerEvents)>) {
        self.known_version = self
            .executor_proxy
            .get_latest_version()
            .await
            .expect("[start sync] failed to fetch latest version from storage");

        let mut interval =
            Interval::new_interval(Duration::from_millis(self.config.tick_interval_ms)).fuse();

        let network_senders: Vec<StateSynchronizerSender> =
            network.iter().map(|t| t.0.clone()).collect();
        let events: Vec<_> = network
            .into_iter()
            .enumerate()
            .map(|(idx, t)| t.1.map(move |e| (idx, e)))
            .collect();
        let mut network_events = select_all(events).fuse();

        loop {
            ::futures::select! {
                msg = self.client_events.select_next_some() => {
                    match msg {
                        CoordinatorMessage::Request(request) => {
                            self.request_sync(request).await;
                        }
                        CoordinatorMessage::Commit(version) => {
                             self.commit(version).await;
                        }
                        CoordinatorMessage::GetState(callback) => {
                            self.get_state(callback);
                        }
                        CoordinatorMessage::GetEpochProof(request) => {
                            self.get_epoch_proof(request).await;
                        }
                    };
                },
                (idx, network_event) = network_events.select_next_some() => {
                    match network_event {
                        Ok(event) => {
                            match event {
                                Event::NewPeer(peer_id) => {
                                    debug!("[state sync] new peer {}", peer_id);
                                    self.peer_manager.enable_peer(peer_id, network_senders[idx].clone());
                                    self.check_progress().await;
                                }
                                Event::LostPeer(peer_id) => {
                                    debug!("[state sync] lost peer {}", peer_id);
                                    self.peer_manager.disable_peer(&peer_id);
                                }
                                Event::Message((peer_id, mut message)) => {
                                    match message.message.unwrap() {
                                        StateSynchronizerMsg_oneof::ChunkRequest(request) => {
                                            let known_version = request.known_version;
                                            if let Err(err) = self.process_chunk_request(peer_id, request).await {
                                                error!("[state sync] failed to serve chunk request to {} with known version {}: {}", peer_id, known_version, err);
                                            }
                                        }
                                        StateSynchronizerMsg_oneof::ChunkResponse(response) => {
                                            if let Err(err) = self.process_chunk_response(&peer_id, response).await {
                                                error!("[state sync] failed to process chunk response from {}: {}", peer_id, err);
                                                counters::APPLY_CHUNK_FAILURE.with_label_values(&[&*peer_id.to_string()]).inc();
                                            } else {
                                                self.peer_manager.update_score(&peer_id, PeerScoreUpdateType::Success);
                                                counters::APPLY_CHUNK_SUCCESS.with_label_values(&[&*peer_id.to_string()]).inc();
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        },
                        Err(err) => { error!("[state sync] network error {}", err); },
                    }
                },
                _ = interval.select_next_some() => {
                    self.check_progress().await;
                }
            }
        }
    }

    async fn request_sync(&mut self, request: SyncRequest) {
        self.known_version = self
            .executor_proxy
            .get_latest_version()
            .await
            .expect("[state sync] failed to fetch latest version from storage");
        let target_version = request.target.ledger_info().version();
        counters::TARGET_VERSION.set(target_version as i64);
        debug!(
            "[state sync] sync requested. Known version: {}, requested_version: {}",
            self.known_version, target_version
        );

        if target_version <= self.known_version {
            debug!("[state sync] sync contains only empty blocks");
            self.store_transactions(
                TransactionListWithProof::new_empty(),
                request.target.clone(),
            )
            .await
            .expect("[state sync] failed to execute empty blocks");
            if request.callback.send(Ok(())).is_err() {
                error!("[state sync] coordinator failed to notify subscriber");
            }
            return;
        }

        let peers = request.target.signatures().keys().copied().collect();
        self.peer_manager.set_peers(peers);
        self.sync_request = Some(request);
        self.request_next_chunk(0).await;
    }

    async fn commit(&mut self, version: u64) {
        debug!(
            "[state sync] commit. Known version: {}, version: {}",
            self.known_version, version
        );
        let is_update = version > self.known_version;
        self.known_version = std::cmp::max(version, self.known_version);
        if is_update {
            if let Some(last_request_tst) =
                self.peer_manager.get_request_time(self.known_version + 1)
            {
                if let Ok(duration) = SystemTime::now().duration_since(last_request_tst) {
                    counters::SYNC_PROGRESS_DURATION.observe_duration(duration);
                }
            }
            if let Err(err) = self.check_subscriptions().await {
                error!("[state sync] failed to check subscriptions: {}", err);
            }
        }
        let sync_request_complete = self.sync_request.as_ref().map_or(false, |sync_req| {
            sync_req.target.ledger_info().version() == self.known_version
        });

        if sync_request_complete {
            debug!(
                "[state sync] synchronization to {} is finished",
                self.known_version
            );
            if let Some(sync_request) = self.sync_request.take() {
                if sync_request.callback.send(Ok(())).is_err() {
                    error!("[state sync] failed to notify subscriber");
                }
            }
        }
        self.peer_manager.remove_requests(version);
        counters::COMMITTED_VERSION.set(version as i64);
    }

    fn get_state(&self, callback: oneshot::Sender<u64>) {
        if callback.send(self.known_version).is_err() {
            error!("[state sync] failed to fetch internal state");
        }
    }

    /// Get a batch of transactions
    async fn process_chunk_request(
        &mut self,
        peer_id: PeerId,
        mut request: GetChunkRequest,
    ) -> Result<()> {
        if request.timeout > self.config.max_timeout_ms
            || request.limit > self.config.max_chunk_limit
        {
            return Err(format_err!(
                "[state sync] timeout: {}, chunk limit: {}, but timeout must not exceed {} ms, and chunk limit must not exceed {}",
                request.timeout,
                request.limit,
                self.config.max_timeout_ms,
                self.config.max_chunk_limit
            ));
        }

        let latest_ledger_info = self.executor_proxy.get_latest_ledger_info().await?;
        let target = match request
            .ledger_info_with_sigs
            .take()
            .map(TryInto::try_into)
            .transpose()
        {
            Ok(Some(x)) => x,
            _ => latest_ledger_info.clone(),
        };

        debug!("[state sync] chunk request: peer_id: {}, known_version: {}, latest_ledger_info: {}, target: {}", peer_id, request.known_version, latest_ledger_info.ledger_info().version(), target.ledger_info().version());

        // if upstream synchronizer doesn't have new data and request timeout is set
        // add peer request into subscription queue
        if self.known_version <= request.known_version && request.timeout > 0 {
            let expiration_time =
                SystemTime::now().checked_add(Duration::from_millis(request.timeout));
            if let Some(time) = expiration_time {
                self.subscriptions
                    .insert(peer_id, (time, request.known_version, request.limit));
            }
            Ok(())
        } else {
            match self.peer_manager.get_network_sender(&peer_id) {
                Some(sender) => {
                    self.deliver_chunk(
                        peer_id,
                        request.known_version,
                        request.limit,
                        target,
                        sender,
                    )
                    .await
                }
                None => Err(format_err!(
                    "[state sync] failed to find network for peer {}",
                    peer_id
                )),
            }
        }
    }

    async fn deliver_chunk(
        &self,
        peer_id: PeerId,
        known_version: u64,
        limit: u64,
        target: LedgerInfo,
        mut network_sender: StateSynchronizerSender,
    ) -> Result<()> {
        let response = self
            .executor_proxy
            .get_chunk(known_version, limit, target)
            .await?;
        let msg = StateSynchronizerMsg {
            message: Some(StateSynchronizerMsg_oneof::ChunkResponse(response)),
        };
        if network_sender.send_to(peer_id, msg).await.is_err() {
            error!("[state sync] failed to send p2p message");
        }
        Ok(())
    }

    /// processes batch of transactions downloaded from peer
    /// executes transactions, updates progress state, calls callback if some sync is finished
    async fn process_chunk_response(
        &mut self,
        peer_id: &PeerId,
        response: GetChunkResponse,
    ) -> Result<()> {
        counters::RESPONSES_RECEIVED
            .with_label_values(&[&*peer_id.to_string()])
            .inc();
        let txn_list_with_proof: TransactionListWithProof = response
            .txn_list_with_proof
            .ok_or_else(|| format_err!("Missing txn_list_with_proof"))?
            .try_into()?;

        if let Some(version) = txn_list_with_proof.first_transaction_version {
            let has_requested = self.peer_manager.has_requested(version, *peer_id);
            // node has received a response from peer, so remove peer entry from requests map
            self.peer_manager.process_response(version, *peer_id);

            if version != self.known_version + 1 {
                // version was not requested, or version was requested from a different peer,
                // so need to penalize peer for maliciously sending chunk
                if has_requested {
                    self.peer_manager
                        .update_score(&peer_id, PeerScoreUpdateType::InvalidChunk)
                }
                return Err(format_err!(
                    "[state sync] non sequential chunk. Known version: {}, received: {}",
                    self.known_version,
                    version,
                ));
            }
        }

        let previous_version = self.known_version;
        let chunk_size = txn_list_with_proof.len();
        let target: LedgerInfo = response
            .ledger_info_with_sigs
            .ok_or_else(|| format_err!("Missing ledger_info_with_sigs"))?
            .try_into()?;

        let result = self
            .validate_and_store_chunk(txn_list_with_proof, target.clone())
            .await;
        let latest_version = self.executor_proxy.get_latest_version().await?;
        if latest_version <= previous_version {
            self.peer_manager
                .update_score(peer_id, PeerScoreUpdateType::InvalidChunk);
        } else {
            self.commit(latest_version).await;
        }
        debug!(
            "[state sync] applied chunk. Previous version: {}, new version: {}, chunk size: {}",
            previous_version, self.known_version, chunk_size
        );

        result
    }

    async fn validate_and_store_chunk(
        &mut self,
        txn_list_with_proof: TransactionListWithProof,
        target: LedgerInfo,
    ) -> Result<()> {
        // optimistically fetch next chunk
        let chunk_size = txn_list_with_proof.len() as u64;
        self.request_next_chunk(chunk_size).await;
        debug!(
            "[state sync] process chunk response. chunk_size: {}",
            chunk_size
        );

        self.executor_proxy.validate_ledger_info(&target)?;

        self.store_transactions(txn_list_with_proof, target).await?;

        counters::STATE_SYNC_TXN_REPLAYED.inc_by(chunk_size as i64);

        Ok(())
    }

    /// ensures that StateSynchronizer makes progress
    /// if peer is not responding, issues new sync request
    async fn check_progress(&mut self) {
        if !self.peer_manager.is_empty()
            && (self.role == RoleType::FullNode || self.sync_request.is_some())
        {
            let last_request_tst = self
                .peer_manager
                .get_request_time(self.known_version + 1)
                .unwrap_or(UNIX_EPOCH);
            let timeout = match self.role {
                RoleType::FullNode => {
                    self.config.tick_interval_ms + self.config.long_poll_timeout_ms
                }
                RoleType::Validator => 2 * self.config.tick_interval_ms,
            };

            // if coordinator didn't make progress by expected time, issue new request
            if let Some(tst) = last_request_tst.checked_add(Duration::from_millis(timeout)) {
                if SystemTime::now().duration_since(tst).is_ok() {
                    self.peer_manager
                        .process_timeout(self.known_version + 1, self.role == RoleType::Validator);
                    self.request_next_chunk(0).await;
                    counters::TIMEOUT.inc();
                }
            }
        }
    }

    async fn request_next_chunk(&mut self, offset: u64) {
        if self.role == RoleType::FullNode || self.sync_request.is_some() {
            if let Some((peer_id, mut sender)) = self.peer_manager.pick_peer() {
                let mut req = GetChunkRequest::default();
                req.known_version = self.known_version + offset;
                req.limit = self.config.chunk_limit;
                self.peer_manager
                    .process_request(self.known_version + offset + 1, peer_id);
                let timeout = match self.role {
                    RoleType::Validator => {
                        if let Some(sync_req) = &self.sync_request {
                            req.ledger_info_with_sigs = Some(sync_req.target.clone().into());
                        }
                        0
                    }
                    RoleType::FullNode => {
                        req.timeout = self.config.long_poll_timeout_ms;
                        self.config.long_poll_timeout_ms
                    }
                };
                debug!(
                    "[state sync] request next chunk. peer_id: {}, known_version: {}, timeout: {}",
                    peer_id,
                    self.known_version + offset,
                    timeout
                );

                let msg = StateSynchronizerMsg {
                    message: Some(StateSynchronizerMsg_oneof::ChunkRequest(req)),
                };

                if sender.send_to(peer_id, msg).await.is_err() {
                    error!("[state sync] failed to send p2p message");
                }
                counters::REQUESTS_SENT
                    .with_label_values(&[&*peer_id.to_string()])
                    .inc();
            }
        }
    }

    async fn store_transactions(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        ledger_info: LedgerInfoWithSignatures,
    ) -> Result<()> {
        self.executor_proxy
            .execute_chunk(txn_list_with_proof, ledger_info)
            .await
    }

    async fn check_subscriptions(&mut self) -> Result<()> {
        let ledger_info = self.executor_proxy.get_latest_ledger_info().await?;
        let committed_version = self.known_version;
        let mut ready = vec![];

        self.subscriptions
            .retain(|peer_id, (expiry, known_version, limit)| {
                // filter out expired peer requests
                if SystemTime::now().duration_since(expiry.clone()).is_ok() {
                    return false;
                }
                if *known_version < committed_version {
                    ready.push((*peer_id, *known_version, *limit));
                    false
                } else {
                    true
                }
            });

        let mut futures = FuturesUnordered::new();
        for (peer_id, known_version, limit) in ready {
            if let Some(sender) = self.peer_manager.get_network_sender(&peer_id) {
                futures.push(self.deliver_chunk(
                    peer_id,
                    known_version,
                    limit,
                    ledger_info.clone(),
                    sender,
                ));
            }
        }
        while let Some(res) = futures.next().await {
            if let Err(err) = res {
                error!("[state sync] failed to notify subscriber {}", err);
            }
        }
        Ok(())
    }

    async fn get_epoch_proof(&self, request: EpochRetrievalRequest) {
        if request
            .callback
            .send(self.executor_proxy.get_epoch_proof(request.start_epoch))
            .is_err()
        {
            error!("[state sync] coordinator failed to send back epoch proof");
        }
    }
}
