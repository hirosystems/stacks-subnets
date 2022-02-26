// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use burnchains::{
    db::{BurnchainBlockData, BurnchainDB},
    Address, Burnchain, BurnchainBlockHeader, Error as BurnchainError, Txid,
};
use chainstate::burn::{
    db::sortdb::SortitionDB, operations::leader_block_commit::RewardSetInfo,
    operations::BlockstackOperationType, BlockSnapshot, ConsensusHash,
};
use chainstate::coordinator::comm::{
    ArcCounterCoordinatorNotices, CoordinatorEvents, CoordinatorNotices, CoordinatorReceivers,
};
use chainstate::stacks::index::MarfTrieId;
use chainstate::stacks::{
    db::{
        accounts::MinerReward, ChainStateBootData, ClarityTx, MinerRewardInfo, StacksChainState,
        StacksHeaderInfo,
    },
    events::{StacksTransactionEvent, StacksTransactionReceipt, TransactionOrigin},
    Error as ChainstateError, StacksBlock, TransactionPayload,
};
use core::StacksEpoch;
use monitoring::{increment_contract_calls_processed, increment_stx_blocks_processed_counter};
use net::atlas::{AtlasConfig, AttachmentInstance};
use util_lib::db::Error as DBError;
use vm::{
    costs::ExecutionCost,
    types::{PrincipalData, QualifiedContractIdentifier},
    Value,
};

use crate::cost_estimates::{CostEstimator, FeeEstimator, PessimisticEstimator};
use crate::types::chainstate::{
    BlockHeaderHash, BurnchainHeaderHash, PoxId, SortitionId, StacksAddress, StacksBlockId,
};
use vm::database::BurnStateDB;

pub use self::comm::CoordinatorCommunication;

pub mod comm;
#[cfg(test)]
pub mod tests;

/// The 3 different states for the current
///  reward cycle's relationship to its PoX anchor
#[derive(Debug, PartialEq)]
pub enum PoxAnchorBlockStatus {
    SelectedAndKnown(BlockHeaderHash, Vec<StacksAddress>),
    SelectedAndUnknown(BlockHeaderHash),
    NotSelected,
}

pub trait BlockEventDispatcher {
    fn announce_block(
        &self,
        block: StacksBlock,
        metadata: StacksHeaderInfo,
        receipts: Vec<StacksTransactionReceipt>,
        parent: &StacksBlockId,
        winner_txid: Txid,
        matured_rewards: Vec<MinerReward>,
        matured_rewards_info: Option<MinerRewardInfo>,
        parent_burn_block_hash: BurnchainHeaderHash,
        parent_burn_block_height: u32,
        parent_burn_block_timestamp: u64,
        anchored_consumed: &ExecutionCost,
        mblock_confirmed_consumed: &ExecutionCost,
    );

    /// called whenever a burn block is about to be
    ///  processed for sortition. note, in the event
    ///  of PoX forks, this will be called _multiple_
    ///  times for the same burnchain header hash.
    fn announce_burn_block(
        &self,
        burn_block: &BurnchainHeaderHash,
        burn_block_height: u64,
        rewards: Vec<(StacksAddress, u64)>,
        burns: u64,
        reward_recipients: Vec<StacksAddress>,
    );

    fn dispatch_boot_receipts(&mut self, receipts: Vec<StacksTransactionReceipt>);
}

pub struct ChainsCoordinator<
    'a,
    T: BlockEventDispatcher,
    N: CoordinatorNotices,
    CE: CostEstimator + ?Sized,
    FE: FeeEstimator + ?Sized,
> {
    canonical_sortition_tip: Option<SortitionId>,
    canonical_chain_tip: Option<StacksBlockId>,
    canonical_pox_id: Option<PoxId>,
    burnchain_blocks_db: BurnchainDB,
    chain_state_db: StacksChainState,
    sortition_db: SortitionDB,
    burnchain: Burnchain,
    attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
    dispatcher: Option<&'a T>,
    cost_estimator: Option<&'a mut CE>,
    fee_estimator: Option<&'a mut FE>,
    notifier: N,
    atlas_config: AtlasConfig,
}

#[derive(Debug)]
pub enum Error {
    BurnchainBlockAlreadyProcessed,
    BurnchainError(BurnchainError),
    ChainstateError(ChainstateError),
    NonContiguousBurnchainBlock(BurnchainError),
    NoSortitions,
    FailedToProcessSortition(BurnchainError),
    DBError(DBError),
    NotPrepareEndBlock,
}

impl From<BurnchainError> for Error {
    fn from(o: BurnchainError) -> Error {
        Error::BurnchainError(o)
    }
}

impl From<ChainstateError> for Error {
    fn from(o: ChainstateError) -> Error {
        Error::ChainstateError(o)
    }
}

impl From<DBError> for Error {
    fn from(o: DBError) -> Error {
        Error::DBError(o)
    }
}

impl<'a, T: BlockEventDispatcher, CE: CostEstimator + ?Sized, FE: FeeEstimator + ?Sized>
    ChainsCoordinator<'a, T, ArcCounterCoordinatorNotices, CE, FE>
{
    pub fn run(
        chain_state_db: StacksChainState,
        burnchain: Burnchain,
        attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
        dispatcher: &'a mut T,
        comms: CoordinatorReceivers,
        atlas_config: AtlasConfig,
        cost_estimator: Option<&mut CE>,
        fee_estimator: Option<&mut FE>,
    ) where
        T: BlockEventDispatcher,
    {
        let stacks_blocks_processed = comms.stacks_blocks_processed.clone();
        let sortitions_processed = comms.sortitions_processed.clone();

        let sortition_db = SortitionDB::open(&burnchain.get_db_path(), true).unwrap();
        let burnchain_blocks_db =
            BurnchainDB::open(&burnchain.get_burnchaindb_path(), false).unwrap();

        let canonical_sortition_tip =
            SortitionDB::get_canonical_sortition_tip(sortition_db.conn()).unwrap();

        let arc_notices = ArcCounterCoordinatorNotices {
            stacks_blocks_processed,
            sortitions_processed,
        };

        let mut inst = ChainsCoordinator {
            canonical_chain_tip: None,
            canonical_sortition_tip: Some(canonical_sortition_tip),
            canonical_pox_id: None,
            burnchain_blocks_db,
            chain_state_db,
            sortition_db,
            burnchain,
            attachments_tx,
            dispatcher: Some(dispatcher),
            notifier: arc_notices,
            cost_estimator,
            fee_estimator,
            atlas_config,
        };

        loop {
            // timeout so that we handle Ctrl-C a little gracefully
            match comms.wait_on() {
                CoordinatorEvents::NEW_STACKS_BLOCK => {
                    debug!("Received new stacks block notice");
                    if let Err(e) = inst.handle_new_stacks_block() {
                        warn!("Error processing new stacks block: {:?}", e);
                    }
                }
                CoordinatorEvents::NEW_BURN_BLOCK => {
                    debug!("Received new burn block notice");
                    if let Err(e) = inst.handle_new_burnchain_block() {
                        warn!("Error processing new burn block: {:?}", e);
                    }
                }
                CoordinatorEvents::STOP => {
                    debug!("Received stop notice");
                    return;
                }
                CoordinatorEvents::TIMEOUT => {}
            }
        }
    }
}

impl<'a, T: BlockEventDispatcher> ChainsCoordinator<'a, T, (), (), ()> {
    #[cfg(test)]
    pub fn test_new(
        burnchain: &Burnchain,
        chain_id: u32,
        path: &str,
        attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
    ) -> ChainsCoordinator<'a, T, (), (), ()> {
        ChainsCoordinator::test_new_with_observer(
            burnchain,
            chain_id,
            path,
            attachments_tx,
            None,
        )
    }

    #[cfg(test)]
    pub fn test_new_with_observer(
        burnchain: &Burnchain,
        chain_id: u32,
        path: &str,
        attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
        dispatcher: Option<&'a T>,
    ) -> ChainsCoordinator<'a, T, (), (), ()> {
        let burnchain = burnchain.clone();

        let mut boot_data = ChainStateBootData::new(&burnchain, vec![], None);

        let sortition_db = SortitionDB::open(&burnchain.get_db_path(), true).unwrap();
        let burnchain_blocks_db =
            BurnchainDB::open(&burnchain.get_burnchaindb_path(), false).unwrap();
        let (chain_state_db, _) = StacksChainState::open_and_exec(
            false,
            chain_id,
            &format!("{}/chainstate/", path),
            Some(&mut boot_data),
        )
        .unwrap();
        let canonical_sortition_tip =
            SortitionDB::get_canonical_sortition_tip(sortition_db.conn()).unwrap();

        ChainsCoordinator {
            canonical_chain_tip: None,
            canonical_sortition_tip: Some(canonical_sortition_tip),
            canonical_pox_id: None,
            burnchain_blocks_db,
            chain_state_db,
            sortition_db,
            burnchain,
            dispatcher,
            cost_estimator: None,
            fee_estimator: None,
            notifier: (),
            attachments_tx,
            atlas_config: AtlasConfig::default(false),
        }
    }
}

struct PaidRewards {
    pox: Vec<(StacksAddress, u64)>,
    burns: u64,
}

fn calculate_paid_rewards(_ops: &[BlockstackOperationType]) -> PaidRewards {
    PaidRewards {
        pox: vec![],
        burns: 1,
    }
}

fn dispatcher_announce_burn_ops<T: BlockEventDispatcher>(
    dispatcher: &T,
    burn_header: &BurnchainBlockHeader,
    paid_rewards: PaidRewards,
) {
    let recipients = vec![] ;

    dispatcher.announce_burn_block(
        &burn_header.block_hash,
        burn_header.block_height,
        paid_rewards.pox,
        paid_rewards.burns,
        recipients,
    );
}

impl<
        'a,
        T: BlockEventDispatcher,
        N: CoordinatorNotices,
        CE: CostEstimator + ?Sized,
        FE: FeeEstimator + ?Sized,
    > ChainsCoordinator<'a, T, N, CE, FE>
{
    pub fn handle_new_stacks_block(&mut self) -> Result<(), Error> {
        panic!("not implemented");
        // DO NOT SUBMIT: what should this be?
        // self.process_ready_blocks()
    }

    pub fn handle_new_burnchain_block(&mut self) -> Result<(), Error> {
        // Retrieve canonical burnchain chain tip from the BurnchainBlocksDB
        let canonical_burnchain_tip = self.burnchain_blocks_db.get_canonical_chain_tip()?;
        debug!("Handle new canonical burnchain tip";
               "height" => %canonical_burnchain_tip.block_height,
               "block_hash" => %canonical_burnchain_tip.block_hash.to_string());

        // Retrieve all the direct ancestors of this block with an unprocessed sortition
        let mut cursor = canonical_burnchain_tip.block_hash.clone();
        let mut sortitions_to_process = VecDeque::new();

        // We halt the ancestry research as soon as we find a processed parent
        let mut last_processed_ancestor = loop {
            if let Some(found_sortition) = self.sortition_db.is_sortition_processed(&cursor)? {
                break found_sortition;
            }

            let current_block = self
                .burnchain_blocks_db
                .get_burnchain_block(&cursor)
                .map_err(|e| {
                    warn!(
                        "ChainsCoordinator: could not retrieve  block burnhash={}",
                        &cursor
                    );
                    Error::NonContiguousBurnchainBlock(e)
                })?;

            let parent = current_block.header.parent_block_hash.clone();
            sortitions_to_process.push_front(current_block);
            cursor = parent;
        };

        let burn_header_hashes: Vec<_> = sortitions_to_process
            .iter()
            .map(|block| block.header.block_hash.to_string())
            .collect();

        debug!(
            "Unprocessed burn chain blocks [{}]",
            burn_header_hashes.join(", ")
        );

        for unprocessed_block in sortitions_to_process.into_iter() {
            let BurnchainBlockData { header, ops } = unprocessed_block;

            // calculate paid rewards during this burnchain block if we announce
            //  to an events dispatcher
            let paid_rewards = if self.dispatcher.is_some() {
                calculate_paid_rewards(&ops)
            } else {
                PaidRewards {
                    pox: vec![],
                    burns: 0,
                }
            };

            // at this point, we need to figure out if the sortition we are
            //  about to process is the first block in reward cycle.
            let (next_snapshot, _) = self
                .sortition_db
                .evaluate_sortition(
                    &header,
                    ops,
                    &self.burnchain,
                    &last_processed_ancestor,
                )
                .map_err(|e| {
                    error!("ChainsCoordinator: unable to evaluate sortition {:?}", e);
                    Error::FailedToProcessSortition(e)
                })?;

            if let Some(dispatcher) = self.dispatcher {
                dispatcher_announce_burn_ops(dispatcher, &header, paid_rewards);
            }

            let sortition_id = next_snapshot.sortition_id;

            self.notifier.notify_sortition_processed();

            debug!(
                "Sortition processed";
                "sortition_id" => &sortition_id.to_string(),
                "burn_header_hash" => &next_snapshot.burn_header_hash.to_string(),
                "burn_height" => next_snapshot.block_height
            );

            // always bump canonical sortition tip:
            //   if this code path is invoked, the canonical burnchain tip
            //   has moved, so we should move our canonical sortition tip as well.
            self.canonical_sortition_tip = Some(sortition_id.clone());
            last_processed_ancestor = sortition_id;

            self.process_ready_blocks()?;
        }

        Ok(())
    }

    ///
    /// Process any ready staging blocks until there are either:
    ///   * there are no more to process
    ///   * a PoX anchor block is processed which invalidates the current PoX fork
    ///
    /// Returns Some(StacksBlockId) if such an anchor block is discovered,
    ///   otherwise returns None
    ///
    fn process_ready_blocks(&mut self) -> Result<Option<BlockHeaderHash>, Error> {
        let canonical_sortition_tip = self.canonical_sortition_tip.as_ref().expect(
            "FAIL: processing a new Stacks block, but don't have a canonical sortition tip",
        );

        let sortdb_handle = self.sortition_db.tx_handle_begin(canonical_sortition_tip)?;
        let mut processed_blocks = self.chain_state_db.process_blocks(sortdb_handle, 1)?;

        while let Some(block_result) = processed_blocks.pop() {
            if let (Some(block_receipt), _) = block_result {
                // only bump the coordinator's state if the processed block
                //   is in our sortition fork
                //  TODO: we should update the staging block logic to prevent
                //    blocks like these from getting processed at all.
                let in_sortition_set = self.sortition_db.is_stacks_block_in_sortition_set(
                    canonical_sortition_tip,
                    &block_receipt.header.anchored_header.block_hash(),
                )?;
                if in_sortition_set {
                    let new_canonical_block_snapshot = SortitionDB::get_block_snapshot(
                        self.sortition_db.conn(),
                        canonical_sortition_tip,
                    )?
                    .expect(&format!(
                        "FAIL: could not find data for the canonical sortition {}",
                        canonical_sortition_tip
                    ));
                    let new_canonical_stacks_block =
                        new_canonical_block_snapshot.get_canonical_stacks_block_id();
                    self.canonical_chain_tip = Some(new_canonical_stacks_block);
                    debug!("Bump blocks processed");
                    self.notifier.notify_stacks_block_processed();
                    increment_stx_blocks_processed_counter();

                    let block_hash = block_receipt.header.anchored_header.block_hash();

                    let mut attachments_instances = HashSet::new();
                    for receipt in block_receipt.tx_receipts.iter() {
                        if let TransactionOrigin::Stacks(ref transaction) = receipt.transaction {
                            if let TransactionPayload::ContractCall(ref contract_call) =
                                transaction.payload
                            {
                                let contract_id = contract_call.to_clarity_contract_id();
                                increment_contract_calls_processed();
                                if self.atlas_config.contracts.contains(&contract_id) {
                                    for event in receipt.events.iter() {
                                        if let StacksTransactionEvent::SmartContractEvent(
                                            ref event_data,
                                        ) = event
                                        {
                                            let res = AttachmentInstance::try_new_from_value(
                                                &event_data.value,
                                                &contract_id,
                                                block_receipt.header.index_block_hash(),
                                                block_receipt.header.block_height,
                                                receipt.transaction.txid(),
                                            );
                                            if let Some(attachment_instance) = res {
                                                attachments_instances.insert(attachment_instance);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if !attachments_instances.is_empty() {
                        info!(
                            "Atlas: {} attachment instances emitted from events",
                            attachments_instances.len()
                        );
                        match self.attachments_tx.send(attachments_instances) {
                            Ok(_) => {}
                            Err(e) => {
                                error!("Atlas: error dispatching attachments {}", e);
                            }
                        };
                    }

                    if let Some(ref mut estimator) = self.cost_estimator {
                        let stacks_epoch = self
                            .sortition_db
                            .index_conn()
                            .get_stacks_epoch_by_epoch_id(&block_receipt.evaluated_epoch)
                            .expect("Could not find a stacks epoch.");
                        estimator.notify_block(
                            &block_receipt.tx_receipts,
                            &stacks_epoch.block_limit,
                            &stacks_epoch.epoch_id,
                        );
                    }

                    if let Some(ref mut estimator) = self.fee_estimator {
                        let stacks_epoch = self
                            .sortition_db
                            .index_conn()
                            .get_stacks_epoch_by_epoch_id(&block_receipt.evaluated_epoch)
                            .expect("Could not find a stacks epoch.");
                        if let Err(e) =
                            estimator.notify_block(&block_receipt, &stacks_epoch.block_limit)
                        {
                            warn!("FeeEstimator failed to process block receipt";
                                  "stacks_block" => %block_hash,
                                  "stacks_height" => %block_receipt.header.block_height,
                                  "error" => %e);
                        }
                    }

                    if let Some(dispatcher) = self.dispatcher {
                        let metadata = &block_receipt.header;
                        let winner_txid = SortitionDB::get_block_snapshot_for_winning_stacks_block(
                            &self.sortition_db.index_conn(),
                            canonical_sortition_tip,
                            &block_hash,
                        )
                        .expect("FAIL: could not find block snapshot for winning block hash")
                        .expect("FAIL: could not find block snapshot for winning block hash")
                        .winning_block_txid;

                        let block: StacksBlock = {
                            let block_path = StacksChainState::get_block_path(
                                &self.chain_state_db.blocks_path,
                                &metadata.consensus_hash,
                                &block_hash,
                            )
                            .unwrap();
                            StacksChainState::consensus_load(&block_path).unwrap()
                        };
                        let stacks_block =
                            StacksBlockId::new(&metadata.consensus_hash, &block_hash);

                        let parent = self
                            .chain_state_db
                            .get_parent(&stacks_block)
                            .expect("BUG: failed to get parent for processed block");

                        dispatcher.announce_block(
                            block,
                            block_receipt.header,
                            block_receipt.tx_receipts,
                            &parent,
                            winner_txid,
                            block_receipt.matured_rewards,
                            block_receipt.matured_rewards_info,
                            block_receipt.parent_burn_block_hash,
                            block_receipt.parent_burn_block_height,
                            block_receipt.parent_burn_block_timestamp,
                            &block_receipt.anchored_block_cost,
                            &block_receipt.parent_microblocks_cost,
                        );
                    }

                    // if, just after processing the block, we _know_ that this block is a pox anchor, that means
                    //   that sortitions have already begun processing that didn't know about this pox anchor.
                    //   we need to trigger an unwind
                    if let Some(pox_anchor) = self
                        .sortition_db
                        .is_stacks_block_pox_anchor(&block_hash, canonical_sortition_tip)?
                    {
                        info!("Discovered an old anchor block: {}", &pox_anchor);
                        return Ok(Some(pox_anchor));
                    }
                }
            }
            // TODO: do something with a poison result

            let sortdb_handle = self.sortition_db.tx_handle_begin(canonical_sortition_tip)?;
            processed_blocks = self.chain_state_db.process_blocks(sortdb_handle, 1)?;
        }

        Ok(None)
    }
}

/// Determine whether or not the current chainstate databases are up-to-date with the current
/// epoch.
pub fn check_chainstate_db_versions(
    epochs: &[StacksEpoch],
    sortdb_path: &str,
    chainstate_path: &str,
) -> Result<bool, DBError> {
    let mut cur_epoch_opt = None;
    if fs::metadata(&sortdb_path).is_ok() {
        // check sortition DB and load up the current epoch
        let max_height = SortitionDB::get_highest_block_height_from_path(&sortdb_path)
            .expect("FATAL: could not query sortition DB for maximum block height");
        let cur_epoch_idx = StacksEpoch::find_epoch(epochs, max_height).expect(&format!(
            "FATAL: no epoch defined for burn height {}",
            max_height
        ));
        let cur_epoch = epochs[cur_epoch_idx].epoch_id;

        // save for later
        cur_epoch_opt = Some(cur_epoch.clone());
        let db_version = SortitionDB::get_db_version_from_path(&sortdb_path)?
            .expect("FATAL: could not load sortition DB version");

        if !SortitionDB::is_db_version_supported_in_epoch(cur_epoch, &db_version) {
            error!(
                "Sortition DB at {} does not support epoch {}",
                &sortdb_path, cur_epoch
            );
            return Ok(false);
        }
    } else {
        warn!("Sortition DB {} does not exist; assuming it will be instantiated with the correct version", sortdb_path);
    }

    if fs::metadata(&chainstate_path).is_ok() {
        let cur_epoch = cur_epoch_opt.expect(
            "FATAL: chainstate corruption: sortition DB does not exist, but chainstate does.",
        );
        let db_config = StacksChainState::get_db_config_from_path(&chainstate_path)?;
        if !db_config.supports_epoch(cur_epoch) {
            error!(
                "Chainstate DB at {} does not support epoch {}",
                &chainstate_path, cur_epoch
            );
            return Ok(false);
        }
    } else {
        warn!("Chainstate DB {} does not exist; assuming it will be instantiated with the correct version", chainstate_path);
    }

    Ok(true)
}
