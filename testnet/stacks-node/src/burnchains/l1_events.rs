use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use clarity::vm::errors::{Error as ClarityError, RuntimeErrorType as ClarityRuntimeError};
use clarity::vm::types::{
    SequenceSubtype, StringSubtype, StringUTF8Length, TupleTypeSignature, TypeSignature,
    Value as ClarityValue,
};
use stacks::burnchains::db::BurnchainDB;
use stacks::burnchains::events::NewBlock;
use stacks::burnchains::indexer::BurnchainIndexer;
use stacks::burnchains::{Burnchain, Error as BurnchainError, Txid};
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::chainstate::stacks::index::ClarityMarfTrieId;
use stacks::chainstate::stacks::miner::SignedProposal;
use stacks::chainstate::stacks::StacksTransaction;
use stacks::codec::StacksMessageCodec;
use stacks::core::StacksEpoch;
use stacks::net::CallReadOnlyRequestBody;
use stacks::util::hash::hex_bytes;
use stacks::util::sleep_ms;
use stacks::util_lib::boot::boot_code_addr;
use stacks_common::types::chainstate::{BlockHeaderHash, BurnchainHeaderHash, StacksBlockId};

use super::commitment::{Layer1Committer, MultiPartyCommitter};
use super::db_indexer::DBBurnchainIndexer;
use super::{burnchain_from_config, BurnchainChannel, ClaritySignature, Error};

use crate::burnchains::commitment::DirectCommitter;
use crate::config::CommitStrategy;
use crate::operations::BurnchainOpSigner;
use crate::util::hash::Sha512Trunc256Sum;
use crate::{BurnchainController, BurnchainTip, Config};

#[derive(Clone)]
pub struct L1Channel {
    blocks: Arc<Mutex<Vec<NewBlock>>>,
}

pub struct L1Controller {
    burnchain: Burnchain,
    config: Config,
    indexer: DBBurnchainIndexer,

    db: Option<SortitionDB>,
    burnchain_db: Option<BurnchainDB>,

    should_keep_running: Option<Arc<AtomicBool>>,

    coordinator: CoordinatorChannels,
    chain_tip: Option<BurnchainTip>,

    committer: Box<dyn Layer1Committer + Send>,

    l1_contract_check_passed: bool,
}

/// Semver version of a Clarity contract
#[derive(Deserialize, Serialize)]
pub struct ContractVersion {
    major: u128,
    minor: u128,
    patch: u128,
    prerelease: Option<String>,
    metadata: Option<String>,
}

impl fmt::Display for ContractVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.prerelease {
            write!(f, "-{pre}")?;
        }
        if let Some(meta) = &self.metadata {
            write!(f, "+{meta}")?;
        }
        Ok(())
    }
}

impl TryFrom<ClarityValue> for ContractVersion {
    type Error = ClarityError;

    fn try_from(value: ClarityValue) -> Result<Self, Self::Error> {
        // FIXME: Clean up this mess. This shouldn't `panic!()` ever!
        match value {
            ClarityValue::Tuple(t) => Ok(ContractVersion {
                major: t.get("major")?.clone().expect_u128(),
                minor: t.get("minor")?.clone().expect_u128(),
                patch: t.get("patch")?.clone().expect_u128(),
                prerelease: t
                    .get("prerelease")?
                    .clone()
                    .expect_optional()
                    .map(|v| v.expect_ascii()),
                metadata: t
                    .get("metadata")?
                    .clone()
                    .expect_optional()
                    .map(|v| v.expect_ascii()),
            }),
            _ => Err(ClarityError::Runtime(
                ClarityRuntimeError::ParseError("Expected Tuple".into()),
                None,
            )),
        }
    }
}

/// Response from read-only function
#[derive(Deserialize, Serialize)]
pub struct GetVersionResponse {
    okay: bool,
    /// Response will contain `result` on success
    result: Option<String>,
    /// Response will contain `cause` on failure
    cause: Option<String>,
}

impl L1Channel {
    /// Creates a channel with a single block with hash from `make_mock_byte_string`.
    pub fn single_block() -> L1Channel {
        L1Channel {
            blocks: Arc::new(Mutex::new(vec![NewBlock {
                block_height: 0,
                burn_block_time: 0,
                index_block_hash: StacksBlockId(make_mock_byte_string_for_first_l1_block()),
                parent_index_block_hash: StacksBlockId::sentinel(),
                events: vec![],
            }])),
        }
    }
}
lazy_static! {
    pub static ref STATIC_EVENTS_STREAM: Arc<L1Channel> = Arc::new(L1Channel::single_block());
    static ref NEXT_BURN_BLOCK: Arc<Mutex<u64>> = Arc::new(Mutex::new(1));
}

/// This outputs a hard-coded value for the hash of the first block created by the
/// Stacks L1 chain. For some reason, this seems stable.
fn make_mock_byte_string_for_first_l1_block() -> [u8; 32] {
    let mut bytes_1 = [0u8; 32];
    let bytes_vec = hex_bytes("55c9861be5cff984a20ce6d99d4aa65941412889bdc665094136429b84f8c2ee")
        .expect("hex value problem");
    bytes_1.copy_from_slice(&bytes_vec[0..32]);
    bytes_1
}

impl BurnchainChannel for L1Channel {
    fn push_block(&self, new_block: NewBlock) -> Result<(), stacks::burnchains::Error> {
        let mut blocks = self.blocks.lock().unwrap();
        blocks.push(new_block);
        Ok(())
    }
}

impl L1Controller {
    pub fn new(config: Config, coordinator: CoordinatorChannels) -> Result<L1Controller, Error> {
        let indexer = DBBurnchainIndexer::new(
            &config.get_burnchain_path_str(),
            config.burnchain.clone(),
            true,
        )?;
        let burnchain = burnchain_from_config(&config.get_burn_db_path(), &config.burnchain)?;
        let committer: Box<dyn Layer1Committer + Send> = match &config.burnchain.commit_strategy {
            CommitStrategy::Direct => Box::new(DirectCommitter {
                config: config.burnchain.clone(),
            }),
            CommitStrategy::MultiMiner {
                required_signers,
                contract,
                other_participants,
                leader: _,
            } => Box::new(MultiPartyCommitter::new(
                &config.burnchain,
                *required_signers,
                contract,
                other_participants.clone(),
            )),
        };
        let l1_controller = L1Controller {
            burnchain,
            config,
            indexer,
            db: None,
            burnchain_db: None,
            should_keep_running: Some(Arc::new(AtomicBool::new(true))),
            coordinator,
            chain_tip: None,
            committer,
            l1_contract_check_passed: false,
        };
        Ok(l1_controller)
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
                &mut self.indexer,
                coordinator_comms.clone(),
                target_block_height_opt,
                None,
                self.should_keep_running.clone(),
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

    fn should_keep_running(&self) -> bool {
        match self.should_keep_running {
            Some(ref should_keep_running) => should_keep_running.load(Ordering::SeqCst),
            _ => true,
        }
    }

    fn l1_rpc_interface(&self) -> String {
        self.config.burnchain.get_rpc_url()
    }

    pub fn l1_submit_tx(&self, tx: StacksTransaction) -> Result<Txid, Error> {
        let client = reqwest::blocking::Client::new();
        let url = format!("{}/v2/transactions", self.l1_rpc_interface());
        let res = client
            .post(url)
            .header("Content-Type", "application/octet-stream")
            .body(tx.serialize_to_vec())
            .send()?;

        if res.status().is_success() {
            let res: String = res.json().unwrap();
            Txid::from_hex(&res).map_err(|e| Error::RPCError(e.to_string()))
        } else {
            Err(Error::RPCError(res.text()?))
        }
    }

    /// Return the Semver version of the `subnet.clar` contract this node is configured to use
    fn get_l1_contract_version(&self) -> Result<ContractVersion, Error> {
        let burn_conf = &self.config.burnchain;
        let url = format!(
            "{http_origin}/v2/contracts/call-read/{contract_addr}/{contract}/get-version",
            http_origin = self.l1_rpc_interface(),
            contract_addr = burn_conf.contract_identifier.issuer,
            contract = burn_conf.contract_identifier.name,
        );

        let body = CallReadOnlyRequestBody {
            sender: boot_code_addr(self.config.is_mainnet()).to_string(),
            arguments: Vec::default(),
        };

        let response = reqwest::blocking::Client::new()
            .post(url)
            .header("Content-Type", "application/octet-stream")
            .json(&body)
            .send()?
            .error_for_status()?
            .json::<GetVersionResponse>()?;

        if !response.okay {
            let message = response
                .cause
                .unwrap_or_else(|| "Unknown contract error".to_string());
            return Err(Error::RPCError(message));
        }

        let result = response
            .result
            .ok_or(Error::RPCError("Empty result".to_string()))?
            .strip_prefix("0x")
            .unwrap() // FIXME
            .to_string();

        let typesig = TypeSignature::TupleType(
            TupleTypeSignature::try_from(vec![
                ("major".into(), TypeSignature::UIntType),
                ("minor".into(), TypeSignature::UIntType),
                ("patch".into(), TypeSignature::UIntType),
                (
                    "prerelease".into(),
                    TypeSignature::OptionalType(Box::new(TypeSignature::SequenceType(
                        SequenceSubtype::StringType(StringSubtype::UTF8(
                            StringUTF8Length::try_from(64usize).unwrap(),
                        )),
                    ))),
                ),
                (
                    "metadata".into(),
                    TypeSignature::OptionalType(Box::new(TypeSignature::SequenceType(
                        SequenceSubtype::StringType(StringSubtype::UTF8(
                            StringUTF8Length::try_from(64usize).unwrap(),
                        )),
                    ))),
                ),
            ])
            .unwrap(),
        );

        let value = ClarityValue::deserialize(&result, &typesig);
        ContractVersion::try_from(value).map_err(Error::from)
    }

    /// Check that the version of `subnet.clar` the node is configured to use is supported
    fn get_validated_l1_contract_version(&self) -> Result<ContractVersion, Error> {
        const EXACT_MAJOR_VERSION: u128 = 2;
        const MINIMUM_MINOR_VERSION: u128 = 0;
        const MINIMUM_PATCH_VERSION: u128 = 0;
        let version = self.get_l1_contract_version()?;
        let ContractVersion {
            major,
            minor,
            patch,
            ..
        } = version;

        if major != EXACT_MAJOR_VERSION {
            let msg = format!("Major version must be {EXACT_MAJOR_VERSION} (found {major})");
            return Err(Error::BurnchainContractVersion(msg));
        };
        if minor < MINIMUM_MINOR_VERSION {
            let msg =
                format!("Minor version must be at least {MINIMUM_MINOR_VERSION} (found {minor})");
            return Err(Error::BurnchainContractVersion(msg));
        };
        if minor == MINIMUM_MINOR_VERSION && patch < MINIMUM_PATCH_VERSION {
            let msg =
                format!("Patch version must be at least {MINIMUM_PATCH_VERSION} (found {patch})");
            return Err(Error::BurnchainContractVersion(msg));
        };
        Ok(version)
    }

    /// Check that the version of `subnet.clar` the node is configured to use is supported
    fn l1_contract_ok(&mut self) -> Result<(), Error> {
        match self.l1_contract_check_passed {
            true => Ok(()),
            false => match self.get_validated_l1_contract_version() {
                // This error is fatal. We can't continue with wrong contract version
                Err(e @ Error::BurnchainContractVersion(_)) => panic!("{e}"),
                // Error checking version, not fatal
                Err(e @ Error::BurnchainContractCheck(_)) => Err(e),
                // Error, transform into `Error::BurnchainContractCheck`
                Err(e) => Err(Error::BurnchainContractCheck(e.to_string())),
                Ok(version) => {
                    info!("Found supported L1 contract version: {version}");
                    self.l1_contract_check_passed = true;
                    Ok(())
                }
            },
        }
    }
}

impl BurnchainController for L1Controller {
    fn start(
        &mut self,
        target_block_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), Error> {
        self.receive_blocks(
            false,
            target_block_height_opt.map_or_else(|| Some(1), |x| Some(x)),
        )
    }
    fn get_channel(&self) -> Arc<dyn BurnchainChannel> {
        self.indexer.get_channel()
    }

    fn commit_required_signatures(&self) -> u8 {
        self.committer.commit_required_signatures()
    }

    fn propose_block(
        &self,
        participant_index: u8,
        proposal: &SignedProposal,
    ) -> Result<ClaritySignature, Error> {
        self.committer
            .propose_block_to(participant_index, proposal)
            .map_err(|e| {
                warn!("Block proposal failed"; "error" => %e);
                Error::BadCommitment(e)
            })
    }

    fn submit_commit(
        &mut self,
        committed_block_hash: BlockHeaderHash,
        committed_block_height: u64,
        target_tip: BurnchainHeaderHash,
        withdrawal_merkle_root: Sha512Trunc256Sum,
        signatures: Vec<super::ClaritySignature>,
        op_signer: &mut BurnchainOpSigner,
        attempt: u64,
    ) -> Result<Txid, Error> {
        self.l1_contract_ok()?;

        let tx = self.committer.make_commit_tx(
            committed_block_hash,
            committed_block_height,
            target_tip,
            withdrawal_merkle_root,
            signatures,
            attempt,
            op_signer,
        )?;

        self.l1_submit_tx(tx)
    }

    fn sync(&mut self, target_block_height_opt: Option<u64>) -> Result<(BurnchainTip, u64), Error> {
        self.receive_blocks(true, target_block_height_opt)
    }

    fn get_chain_tip(&self) -> BurnchainTip {
        self.chain_tip.as_ref().unwrap().clone()
    }

    fn get_headers_height(&self) -> u64 {
        self.indexer.get_headers_height().unwrap()
    }

    fn sortdb_ref(&self) -> &SortitionDB {
        self.db
            .as_ref()
            .expect("BUG: did not instantiate the burn DB")
    }

    fn sortdb_mut(&mut self) -> &mut SortitionDB {
        let burnchain = self.get_burnchain();

        let (db, burnchain_db) = burnchain.open_db(true).unwrap();
        self.db = Some(db);
        self.burnchain_db = Some(burnchain_db);

        match self.db {
            Some(ref mut sortdb) => sortdb,
            None => unreachable!(),
        }
    }

    fn connect_dbs(&mut self) -> Result<(), Error> {
        let burnchain = self.get_burnchain();

        self.indexer.connect(true)?;
        burnchain.connect_db(&self.indexer, true)?;
        Ok(())
    }

    fn get_stacks_epochs(&self) -> Vec<StacksEpoch> {
        self.indexer.get_stacks_epochs()
    }

    fn get_burnchain(&self) -> Burnchain {
        self.burnchain.clone()
    }

    fn wait_for_sortitions(&mut self, height_to_wait: Option<u64>) -> Result<BurnchainTip, Error> {
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
            if !self.should_keep_running() {
                return Err(Error::CoordinatorClosed);
            }
            // yield some time
            sleep_ms(100);
        }
    }

    #[cfg(test)]
    fn bootstrap_chain(&mut self, _blocks_count: u64) {
        todo!()
    }
}
