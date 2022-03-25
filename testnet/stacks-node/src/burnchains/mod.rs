use crate::Config;

use super::operations::BurnchainOpSigner;

use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use stacks::burnchains;
use stacks::burnchains::db::BurnchainDB;
use stacks::burnchains::indexer::BurnchainIndexer;
use stacks::burnchains::Burnchain;
use stacks::burnchains::BurnchainStateTransition;
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::burn::operations::BlockstackOperationType;
use stacks::chainstate::burn::BlockSnapshot;

use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::core::StacksEpoch;
use stacks::util::sleep_ms;

/// This module implements a burnchain controller that
/// simulates the L1 chain. This controller accepts miner
/// commitments, and uses them to produce the next simulated
/// burnchain block.
pub mod mock_events;

#[derive(Debug)]
pub enum Error {
    CoordinatorClosed,
    IndexerError(burnchains::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::CoordinatorClosed => write!(f, "ChainsCoordinator closed"),
            Error::IndexerError(ref e) => write!(f, "Indexer error: {:?}", e),
        }
    }
}

impl From<burnchains::Error> for Error {
    fn from(e: burnchains::Error) -> Self {
        Error::IndexerError(e)
    }
}

pub trait InputBurnblockChannel: Send + Sync {
    fn input_burnblock(&mut self) -> bool;
}
pub trait SubmitOperationChannel: Send + Sync {
    fn submit_operation(
        &mut self,
        operation: BlockstackOperationType,
        op_signer: &mut BurnchainOpSigner,
        attempt: u64,
    ) -> bool;
}

use stacks::burnchains::Error as BurnchainError;
pub struct BurnchainController {
    indexer: Box<dyn BurnchainIndexer>,
    submit_operation_channel: Box<dyn SubmitOperationChannel>,
    input_burnblock_channel: Arc<dyn InputBurnblockChannel>,
    should_keep_running: Arc<AtomicBool>,
    coordinator: CoordinatorChannels,
    chain_tip: Option<BurnchainTip>,
    burnchain: Option<Burnchain>,
    db: Option<SortitionDB>,
    burnchain_db: Option<BurnchainDB>,
    burn_db_path: String,
}
impl BurnchainController {
    pub fn new(
        config: Config,
        submit_operation_channel: Box<dyn SubmitOperationChannel>,
        coordinator: CoordinatorChannels,
    ) -> BurnchainController {
        let contract_identifier = config.burnchain.contract_identifier.clone();
        let indexer = MockIndexer::new(contract_identifier.clone());
        BurnchainController {
            contract_identifier,
            burnchain: None,
            config,
            indexer,
            db: None,
            burnchain_db: None,
            should_keep_running: Some(Arc::new(AtomicBool::new(true))),
            coordinator,
            chain_tip: None,
            burn_db_path: config.get_burn_db_path(),
        }
    }
    pub fn start(
        &mut self,
        target_block_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), Error> {
        self.receive_blocks(
            false,
            target_block_height_opt.map_or_else(|| Some(1), |x| Some(x)),
        )
    }
    fn receive_blocks(
        &mut self,
        block_for_sortitions: bool,
        target_block_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), Error> {
        let coordinator_comms = self.coordinator.clone();
        let mut burnchain = self.get_burnchain();

        let (block_snapshot, burnchain_height) = loop {
            match burnchain.sync_with_indexer(
                self.indexer.as_mut(),
                coordinator_comms.clone(),
                target_block_height_opt,
                None,
                Some(self.should_keep_running.clone()),
            ) {
                Ok(x) => {
                    // initialize the dbs...
                    self.sortdb_mut();

                    // wait for the chains coordinator to catch up with us
                    if block_for_sortitions {
                        self.wait_for_sortitions(Some(x.block_height))?;
                    }

                    // NOTE: This is the latest _sortition_ on the canonical sortition history, not the latest burnchain block!
                    let sort_tip =
                        SortitionDB::get_canonical_burn_chain_tip(self.sortdb_ref().conn())
                            .expect("Sortition DB error.");

                    let snapshot = self
                        .sortdb_ref()
                        .get_sortition_result(&sort_tip.sortition_id)
                        .expect("Sortition DB error.")
                        .expect("BUG: no data for the canonical chain tip");

                    let burnchain_height = self
                        .indexer
                        .get_highest_header_height()
                        .map_err(Error::IndexerError)?;
                    break (snapshot, burnchain_height);
                }
                Err(e) => {
                    // keep trying
                    error!("Unable to sync with burnchain: {}", e);
                    match e {
                        BurnchainError::CoordinatorClosed => return Err(Error::CoordinatorClosed),
                        BurnchainError::TrySyncAgain => {
                            // try again immediately
                            continue;
                        }
                        BurnchainError::BurnchainPeerBroken => {
                            // remote burnchain peer broke, and produced a shorter blockchain fork.
                            // just keep trying
                            sleep_ms(5000);
                            continue;
                        }
                        _ => {
                            // delay and try again
                            sleep_ms(5000);
                            continue;
                        }
                    }
                }
            }
        };

        let burnchain_tip = BurnchainTip {
            block_snapshot,
            received_at: Instant::now(),
        };

        self.chain_tip = Some(burnchain_tip.clone());
        debug!("Done receiving blocks");

        Ok((burnchain_tip, burnchain_height))
    }
    pub fn input_burnblock_channel(&self) -> Arc<dyn InputBurnblockChannel> {
        self.input_burnblock_channel.clone()
    }
    pub fn submit_operation(
        &mut self,
        operation: BlockstackOperationType,
        op_signer: &mut BurnchainOpSigner,
        attempt: u64,
    ) -> bool {
        self.submit_operation_channel
            .submit_operation(operation, op_signer, attempt)
    }
    pub fn sync(
        &mut self,
        target_block_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), Error> {
        self.receive_blocks(true, target_block_height_opt)
    }

    pub fn get_chain_tip(&self) -> BurnchainTip {
        self.chain_tip.as_ref().unwrap().clone()
    }

    pub fn get_headers_height(&self) -> u64 {
        self.indexer.get_headers_height().unwrap()
    }

    pub fn sortdb_ref(&self) -> &SortitionDB {
        self.db
            .as_ref()
            .expect("BUG: did not instantiate the burn DB")
    }

    pub fn sortdb_mut(&mut self) -> &mut SortitionDB {
        let burnchain = self.get_burnchain();

        let (db, burnchain_db) = burnchain.open_db(true).unwrap();
        self.db = Some(db);
        self.burnchain_db = Some(burnchain_db);

        match self.db {
            Some(ref mut sortdb) => sortdb,
            None => unreachable!(),
        }
    }

    pub fn connect_dbs(&mut self) -> Result<(), Error> {
        let burnchain = self.get_burnchain();
        burnchain.connect_db(
            self.indexer.as_ref(),
            true,
            self.indexer.get_first_block_header_hash()?,
            self.indexer.get_first_block_header_timestamp()?,
        )?;
        Ok(())
    }

    pub fn get_stacks_epochs(&self) -> Vec<StacksEpoch> {
        self.indexer.get_stacks_epochs()
    }

    pub fn get_burnchain(&self) -> Burnchain {
        match &self.burnchain {
            Some(burnchain) => burnchain.clone(),
            None => {
                let working_dir = &self.burn_db_path;
                Burnchain::new(&working_dir, "mockstack", "hyperchain").unwrap_or_else(|e| {
                    error!("Failed to instantiate burnchain: {}", e);
                    panic!()
                })
            }
        }
    }

    pub fn wait_for_sortitions(
        &mut self,
        height_to_wait: Option<u64>,
    ) -> Result<BurnchainTip, Error> {
        loop {
            let canonical_burnchain_tip = self
                .burnchain_db
                .as_ref()
                .expect("BurnchainDB not opened")
                .get_canonical_chain_tip()
                .unwrap();
            let canonical_sortition_tip =
                SortitionDB::get_canonical_burn_chain_tip(self.sortdb_ref().conn()).unwrap();
            if canonical_burnchain_tip.block_height == canonical_sortition_tip.block_height {
                // If the canonical burnchain tip is the same as a sortition tip.
                let _ = self
                    .sortdb_ref()
                    .get_sortition_result(&canonical_sortition_tip.sortition_id)
                    .expect("Sortition DB error.")
                    .expect("BUG: no data for the canonical chain tip");
                return Ok(BurnchainTip {
                    block_snapshot: canonical_sortition_tip,
                    received_at: Instant::now(),
                });
            } else if let Some(height_to_wait) = height_to_wait {
                // If the height to wait until has been reached, then ext.
                if canonical_sortition_tip.block_height >= height_to_wait {
                    let _ = self
                        .sortdb_ref()
                        .get_sortition_result(&canonical_sortition_tip.sortition_id)
                        .expect("Sortition DB error.")
                        .expect("BUG: no data for the canonical chain tip");

                    return Ok(BurnchainTip {
                        block_snapshot: canonical_sortition_tip,
                        received_at: Instant::now(),
                    });
                }
            }
            if !self.should_keep_running.load(Ordering::SeqCst) {
                return Err(Error::CoordinatorClosed);
            }
            // yield some time
            sleep_ms(100);
        }
    }

    #[cfg(test)]
    fn bootstrap_chain(&mut self, blocks_count: u64) {
        todo!()
    }
}

#[derive(Debug, Clone)]
pub struct BurnchainTip {
    pub block_snapshot: BlockSnapshot,
    pub received_at: Instant,
}
