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

/// This module contains drivers and types for all burn chains we support.
use std::collections::HashMap;
use std::convert::TryFrom;
use std::default::Default;
use std::error;
use std::fmt;
use std::fmt::Formatter;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;

use clarity::vm::types::QualifiedContractIdentifier;
use rusqlite::Error as sqlite_error;

use address::AddressHashMode;
use chainstate::burn::operations::leader_block_commit::OUTPUTS_PER_COMMIT;
use chainstate::burn::operations::BlockstackOperationType;
use chainstate::burn::operations::Error as op_error;
use chainstate::burn::operations::LeaderKeyRegisterOp;
use chainstate::burn::ConsensusHash;
use chainstate::stacks::StacksPublicKey;
use core::*;
use net::neighbors::MAX_NEIGHBOR_BLOCK_DELAY;
use util::hash::Hash160;
use util::secp256k1::MessageSignature;
use util_lib::db::Error as db_error;

use core::BLOCK_INVENTORY_SYNC_CYCLE_SIZE;
use stacks_common::types::chainstate::StacksAddress;
use stacks_common::types::chainstate::TrieHash;
use stacks_common::types::chainstate::{BlockHeaderHash, BurnchainHeaderHash, StacksBlockId};

pub use types::{Address, PrivateKey, PublicKey};

pub mod burnchain;
pub mod db;
/// Stacks events parser used to construct the L1 hyperchain operations.
///
/// The events module processes an event stream from a Layer-1 Stacks
/// node (provided by an indexer) and produces `BurnchainTransaction`
/// and `BurnchainBlock` types. These types are fed into the sortition
/// db (again, by the indexer) in order to prepare the rest of the
/// hyperchain node to download and validate the corresponding
/// hyperchain blocks.
pub mod events;

#[derive(Serialize, Deserialize)]
pub struct Txid(pub [u8; 32]);
impl_array_newtype!(Txid, u8, 32);
impl_array_hexstring_fmt!(Txid);
impl_byte_array_newtype!(Txid, u8, 32);
pub const TXID_ENCODED_SIZE: u32 = 32;

pub const MAGIC_BYTES_LENGTH: usize = 2;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MagicBytes([u8; MAGIC_BYTES_LENGTH]);
impl_array_newtype!(MagicBytes, u8, MAGIC_BYTES_LENGTH);
impl MagicBytes {
    pub fn default() -> MagicBytes {
        BLOCKSTACK_MAGIC_MAINNET
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BitcoinNetworkType {
    Mainnet,
    Testnet,
    Regtest,
}

pub const BLOCKSTACK_MAGIC_MAINNET: MagicBytes = MagicBytes([105, 100]); // 'id'

#[derive(Debug, PartialEq, Clone)]
pub struct BurnchainParameters {
    chain_name: String,
    network_name: String,
    network_id: u32,
    stable_confirmations: u32,
    consensus_hash_lifetime: u32,
    pub first_block_height: u64,
    pub first_block_hash: BurnchainHeaderHash,
    pub first_block_timestamp: u32,
    pub initial_reward_start_block: u64,
}

impl BurnchainParameters {
    pub fn from_params(chain: &str, network: &str) -> Option<BurnchainParameters> {
        match (chain, network) {
            ("mockstack", "mainnet") => Some(BurnchainParameters::hyperchain_mocknet()),
            ("bitcoin", "mainnet") => Some(BurnchainParameters::bitcoin_mainnet()),
            ("bitcoin", "testnet") => Some(BurnchainParameters::bitcoin_testnet()),
            ("bitcoin", "regtest") => Some(BurnchainParameters::bitcoin_regtest()),
            _ => None,
        }
    }

    pub fn hyperchain_mocknet() -> BurnchainParameters {
        BurnchainParameters {
            chain_name: "mockstack".to_string(),
            network_name: "mainnet".into(),
            network_id: 0,
            stable_confirmations: 0,
            consensus_hash_lifetime: 24,
            first_block_height: 0,
            first_block_hash: BurnchainHeaderHash::from_hex(BITCOIN_MAINNET_FIRST_BLOCK_HASH)
                .unwrap(),
            first_block_timestamp: 0,
            initial_reward_start_block: 0,
        }
    }

    pub fn bitcoin_mainnet() -> BurnchainParameters {
        BurnchainParameters {
            chain_name: "bitcoin".to_string(),
            network_name: "mainnet".into(),
            network_id: 0,
            stable_confirmations: 7,
            consensus_hash_lifetime: 24,
            first_block_height: BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT,
            first_block_hash: BurnchainHeaderHash::from_hex(BITCOIN_MAINNET_FIRST_BLOCK_HASH)
                .unwrap(),
            first_block_timestamp: BITCOIN_MAINNET_FIRST_BLOCK_TIMESTAMP,
            initial_reward_start_block: BITCOIN_MAINNET_INITIAL_REWARD_START_BLOCK,
        }
    }

    pub fn bitcoin_testnet() -> BurnchainParameters {
        BurnchainParameters {
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".into(),
            network_id: 1,
            stable_confirmations: 7,
            consensus_hash_lifetime: 24,
            first_block_height: BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT,
            first_block_hash: BurnchainHeaderHash::from_hex(BITCOIN_TESTNET_FIRST_BLOCK_HASH)
                .unwrap(),
            first_block_timestamp: BITCOIN_TESTNET_FIRST_BLOCK_TIMESTAMP,
            initial_reward_start_block: BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT - 10_000,
        }
    }

    pub fn bitcoin_regtest() -> BurnchainParameters {
        BurnchainParameters {
            chain_name: "bitcoin".to_string(),
            network_name: "regtest".into(),
            network_id: 2,
            stable_confirmations: 1,
            consensus_hash_lifetime: 24,
            first_block_height: BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT,
            first_block_hash: BurnchainHeaderHash::from_hex(BITCOIN_REGTEST_FIRST_BLOCK_HASH)
                .unwrap(),
            first_block_timestamp: BITCOIN_REGTEST_FIRST_BLOCK_TIMESTAMP,
            initial_reward_start_block: BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT,
        }
    }

    pub fn is_testnet(network_id: u32) -> bool {
        if network_id == 0 {
            false
        } else {
            true
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct BurnchainSigner {
    pub hash_mode: AddressHashMode,
    pub num_sigs: usize,
    pub public_keys: Vec<StacksPublicKey>,
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct BurnchainRecipient {
    pub address: StacksAddress,
    pub amount: u64,
}

#[derive(Debug, PartialEq, Clone)]
/// This is the inner type of the Layer-1 Stacks event,
/// containing any operation specific data.
pub enum StacksHyperOpType {
    BlockCommit { subnet_block_hash: BlockHeaderHash },
}

#[derive(Debug, PartialEq, Clone)]
/// These operations are derived from a Layer-1 Stacks chain,
/// parsed from the `stacks-node` events API.
pub struct StacksHyperOp {
    pub txid: Txid,
    pub in_block: StacksBlockId,
    pub opcode: u8,
    pub event_index: u32,
    pub event: StacksHyperOpType,
}

#[derive(Debug, PartialEq, Clone)]
/// Enum for wrapping Layer-1 operation providers for hyperchains
pub enum BurnchainTransaction {
    StacksBase(StacksHyperOp),
}

impl BurnchainTransaction {
    pub fn txid(&self) -> Txid {
        match *self {
            BurnchainTransaction::StacksBase(ref tx) => tx.txid.clone(),
        }
    }

    pub fn vtxindex(&self) -> u32 {
        match *self {
            BurnchainTransaction::StacksBase(ref tx) => tx.event_index,
        }
    }

    pub fn opcode(&self) -> u8 {
        match *self {
            BurnchainTransaction::StacksBase(ref tx) => tx.opcode,
        }
    }

    pub fn get_burn_amount(&self) -> u64 {
        0
    }
}

use burnchains::Error as burnchain_error;

/// Abstract representation of a burn block header.
pub trait BurnHeaderIPC: Send + Sync {
    fn height(&self) -> u64;
    fn header_hash(&self) -> BurnchainHeaderHash;
    fn parent_header_hash(&self) -> BurnchainHeaderHash;
    fn time_stamp(&self) -> u64;
}

impl std::fmt::Debug for dyn BurnHeaderIPC {
    /// Shortened debug string, for logging.
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "BurnHeaderIPC(height={:?}, header_hash={:?}, parent_header_hash={:?}, time_stamp={:?})",
            self.height(),
            self.header_hash(),
            self.parent_header_hash(),
            self.time_stamp()
        )
    }
}

impl PartialEq for dyn BurnHeaderIPC {
    fn eq(&self, other: &Self) -> bool {
        true
    }
}

/// Abstract representation of a burn block.
pub trait BurnBlockIPC: Send + Sync {
    fn height(&self) -> u64;
    fn header(&self) -> Box<dyn BurnHeaderIPC>;
    fn to_burn_block(
        &self,
        subnets_contract: &QualifiedContractIdentifier,
    ) -> Result<BurnchainBlock, burnchain_error>;

    fn clone_box(&self) -> Box<dyn BurnBlockIPC>;
}

/// Manages the downloading of blocks given headers. Unlike `BurnchainIndexer`,
/// a downloader can be sent between threads (implements `Send + Sync`).
pub trait BurnchainBlockDownloader: Send + Sync {
    fn download(
        &self,
        header: &dyn BurnHeaderIPC,
    ) -> Result<Box<dyn BurnBlockIPC>, burnchain_error>;
}

/// Allows the user to push a new block into the system.
pub trait BurnBlockInputChannel: Send + Sync {
    /// Push a block into the channel.
    fn push_block(&self, new_block: Box<dyn BurnBlockIPC>) -> Result<(), burnchain_error>;
}

/// Provides an interface where new L1 blocks can be received (by providng a
/// `BurnBlockInputChannel`.
pub trait BurnchainIndexer {
    /// Give the indexer a chance to connect to any underlying databases.
    fn connect(&mut self, readwrite: bool) -> Result<(), burnchain_error>;

    /// The returned channel is used to push a block into the system understood by this indexer.
    fn get_input_channel(&self) -> Box<dyn BurnBlockInputChannel>;

    /// Returns the earliest block height.
    fn get_first_block_height(&self) -> u64;
    fn get_first_block_header_hash(&self) -> Result<BurnchainHeaderHash, burnchain_error>;
    fn get_first_block_header_timestamp(&self) -> Result<u64, burnchain_error>;
    fn get_stacks_epochs(&self) -> Vec<StacksEpoch>;

    fn get_headers_path(&self) -> String;
    fn get_headers_height(&self) -> Result<u64, burnchain_error>;
    fn get_highest_header_height(&self) -> Result<u64, burnchain_error>;

    /// Returns the canonical chain tip.
    fn get_canonical_chain_tip(&self) -> Option<Box<dyn BurnHeaderIPC>>;

    /// Returns true if there has been a re-organization since the last call.
    fn find_chain_reorg(&mut self) -> Result<u64, burnchain_error>;

    /// Wait for all of these headers to sync with the burnchain.
    fn sync_headers(
        &mut self,
        start_height: u64,
        end_height: Option<u64>,
    ) -> Result<u64, burnchain_error>;
    fn drop_headers(&mut self, new_height: u64) -> Result<(), burnchain_error>;

    /// Read the headers on the canonical chain from heights `start_block` (inclusive) to `end_block` (exclusive).
    fn read_headers(
        &self,
        start_block: u64,
        end_block: u64,
    ) -> Result<Vec<Box<dyn BurnHeaderIPC>>, burnchain_error>;

    /// Returns the subnets contract for this hyper chain.
    fn subnets_contract(&self) -> QualifiedContractIdentifier;

    fn downloader(&self) -> Box<dyn BurnchainBlockDownloader>;
}

#[derive(Debug, PartialEq, Clone)]
/// Represents a layer-1 Stacks block with the Hyperchain
/// relevant information parsed into th `ops` vector.
pub struct StacksHyperBlock {
    pub current_block: StacksBlockId,
    pub parent_block: StacksBlockId,
    pub block_height: u64,
    pub ops: Vec<StacksHyperOp>,
}

#[derive(Debug, PartialEq, Clone)]
/// Enum for wrapping Layer-1 blocks for hyperchains
pub enum BurnchainBlock {
    StacksHyperBlock(StacksHyperBlock),
}

#[derive(Debug, PartialEq, Clone)]
pub struct BurnchainBlockHeader {
    pub block_height: u64,
    pub block_hash: BurnchainHeaderHash,
    pub parent_block_hash: BurnchainHeaderHash,
    pub num_txs: u64,
    pub timestamp: u64,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Burnchain {
    pub peer_version: u32,
    pub network_id: u32,
    pub chain_name: String,
    pub network_name: String,
    pub working_dir: String,
    pub consensus_hash_lifetime: u32,
    pub stable_confirmations: u32,
    pub first_block_height: u64,
    pub first_block_hash: BurnchainHeaderHash,
    pub first_block_timestamp: u32,
    pub pox_constants: PoxConstants,
    pub initial_reward_start_block: u64,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PoxConstants {
    /// the length (in burn blocks) of the reward cycle
    pub reward_cycle_length: u32,
}

impl PoxConstants {
    pub fn new(reward_cycle_length: u32) -> PoxConstants {
        PoxConstants {
            reward_cycle_length,
        }
    }
    #[cfg(test)]
    pub fn test_default() -> PoxConstants {
        PoxConstants::new(10)
    }

    pub fn mainnet_default() -> PoxConstants {
        PoxConstants::new(BLOCK_INVENTORY_SYNC_CYCLE_SIZE)
    }

    pub fn testnet_default() -> PoxConstants {
        PoxConstants::new(BLOCK_INVENTORY_SYNC_CYCLE_SIZE / 2)
    }

    pub fn regtest_default() -> PoxConstants {
        PoxConstants::new(5)
    }

    /// Return the number of cycles, up to and including the current cycle.
    pub fn num_sync_cycles_to_height(&self, target_height: u64) -> u64 {
        PoxConstants::num_sync_cycles_to_height_internal(
            target_height,
            self.reward_cycle_length as u64,
        )
    }
    /// Implements `num_sync_cycles_to_height`.
    fn num_sync_cycles_to_height_internal(target_height: u64, cycle_length: u64) -> u64 {
        (target_height / cycle_length) + 1
    }
}

/// Structure for encoding our view of the network
#[derive(Debug, PartialEq, Clone)]
pub struct BurnchainView {
    pub burn_block_height: u64, // last-seen block height (at chain tip)
    pub burn_block_hash: BurnchainHeaderHash, // last-seen burn block hash
    pub burn_stable_block_height: u64, // latest stable block height (e.g. chain tip minus 7)
    pub burn_stable_block_hash: BurnchainHeaderHash, // latest stable burn block hash
    pub last_burn_block_hashes: HashMap<u64, BurnchainHeaderHash>, // map all block heights from burn_block_height back to the oldest one we'll take for considering the peer a neighbor
}

/// The burnchain block's encoded state transition:
/// -- the new burn distribution
/// -- the sequence of valid blockstack operations that went into it
/// -- the set of previously-accepted leader VRF keys consumed
#[derive(Debug, Clone)]
pub struct BurnchainStateTransition {
    pub accepted_ops: Vec<BlockstackOperationType>,
}

#[derive(Debug)]
pub enum Error {
    /// Unsupported burn chain
    UnsupportedBurnchain,
    /// Bitcoin-related error
    Bitcoin(String),
    /// burn database error
    DBError(db_error),
    /// Download error
    DownloadError(String),
    /// Parse error
    ParseError,
    /// Thread channel error
    ThreadChannelError,
    /// Missing headers
    MissingHeaders,
    /// Missing parent block
    MissingParentBlock,
    /// Remote burnchain peer has misbehaved
    BurnchainPeerBroken,
    /// filesystem error
    FSError(io::Error),
    /// Operation processing error
    OpError(op_error),
    /// Try again error
    TrySyncAgain,
    UnknownBlock(BurnchainHeaderHash),
    CoordinatorClosed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::UnsupportedBurnchain => write!(f, "Unsupported burnchain"),
            Error::Bitcoin(ref btce) => fmt::Display::fmt(btce, f),
            Error::DBError(ref dbe) => fmt::Display::fmt(dbe, f),
            Error::DownloadError(ref btce) => fmt::Display::fmt(btce, f),
            Error::ParseError => write!(f, "Parse error"),
            Error::MissingHeaders => write!(f, "Missing block headers"),
            Error::MissingParentBlock => write!(f, "Missing parent block"),
            Error::ThreadChannelError => write!(f, "Error in thread channel"),
            Error::BurnchainPeerBroken => write!(f, "Remote burnchain peer has misbehaved"),
            Error::FSError(ref e) => fmt::Display::fmt(e, f),
            Error::OpError(ref e) => fmt::Display::fmt(e, f),
            Error::TrySyncAgain => write!(f, "Try synchronizing again"),
            Error::UnknownBlock(block) => write!(f, "Unknown burnchain block {}", block),
            Error::CoordinatorClosed => write!(f, "ChainsCoordinator channel hung up"),
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::UnsupportedBurnchain => None,
            Error::Bitcoin(ref _e) => None,
            Error::DBError(ref e) => Some(e),
            Error::DownloadError(ref _e) => None,
            Error::ParseError => None,
            Error::MissingHeaders => None,
            Error::MissingParentBlock => None,
            Error::ThreadChannelError => None,
            Error::BurnchainPeerBroken => None,
            Error::FSError(ref e) => Some(e),
            Error::OpError(ref e) => Some(e),
            Error::TrySyncAgain => None,
            Error::UnknownBlock(_) => None,
            Error::CoordinatorClosed => None,
        }
    }
}

impl From<db_error> for Error {
    fn from(e: db_error) -> Error {
        Error::DBError(e)
    }
}

impl From<sqlite_error> for Error {
    fn from(e: sqlite_error) -> Error {
        Error::DBError(db_error::SqliteError(e))
    }
}

impl BurnchainView {
    #[cfg(test)]
    pub fn make_test_data(&mut self) {
        let oldest_height = if self.burn_stable_block_height < MAX_NEIGHBOR_BLOCK_DELAY {
            0
        } else {
            self.burn_stable_block_height - MAX_NEIGHBOR_BLOCK_DELAY
        };

        let mut ret = HashMap::new();
        for i in oldest_height..self.burn_block_height + 1 {
            if i == self.burn_stable_block_height {
                ret.insert(i, self.burn_stable_block_hash.clone());
            } else if i == self.burn_block_height {
                ret.insert(i, self.burn_block_hash.clone());
            } else {
                let data = {
                    use sha2::Digest;
                    use sha2::Sha256;
                    let mut hasher = Sha256::new();
                    hasher.input(&i.to_le_bytes());
                    hasher.result()
                };
                let mut data_32 = [0x00; 32];
                data_32.copy_from_slice(&data[0..32]);
                ret.insert(i, BurnchainHeaderHash(data_32));
            }
        }
        self.last_burn_block_hashes = ret;
    }
}

/// Corresponds to a row in the table.
pub struct BasicBurnHeader {
    pub height: u64,
    pub header_hash: BurnchainHeaderHash,
    pub parent_header_hash: BurnchainHeaderHash,
    pub time_stamp: u64,
}

impl BurnHeaderIPC for BasicBurnHeader {
    fn height(&self) -> u64 {
        self.height
    }
    fn header_hash(&self) -> BurnchainHeaderHash {
        self.header_hash
    }
    fn parent_header_hash(&self) -> BurnchainHeaderHash {
        self.parent_header_hash
    }
    fn time_stamp(&self) -> u64 {
        self.time_stamp
    }
}

#[cfg(test)]
pub mod test;
