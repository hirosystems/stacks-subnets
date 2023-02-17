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

use std::convert::From;
use std::convert::TryInto;
use std::error;
use std::fmt;
use std::fs;
use std::io;

use crate::burnchains::Burnchain;
use crate::burnchains::BurnchainBlockHeader;
use crate::burnchains::Error as BurnchainError;
use crate::burnchains::Txid;
use crate::burnchains::{Address, PublicKey};
use crate::burnchains::{BurnchainRecipient, BurnchainSigner, BurnchainTransaction};
use crate::chainstate::burn::db::sortdb::SortitionHandleTx;
use crate::chainstate::burn::operations::leader_block_commit::{
    MissedBlockCommit, BURN_BLOCK_MINED_AT_MODULUS,
};
use crate::types::chainstate::BlockHeaderHash;
use crate::types::chainstate::StacksAddress;
use crate::types::chainstate::StacksBlockId;
use crate::types::chainstate::TrieHash;
use crate::types::chainstate::VRFSeed;

use crate::chainstate::burn::ConsensusHash;
use crate::chainstate::burn::Opcodes;
use crate::util_lib::db::DBConn;
use crate::util_lib::db::DBTx;
use crate::util_lib::db::Error as db_error;
use clarity::util::HexError;
use serde::Deserialize;
use stacks_common::util::hash::Hash160;
use stacks_common::util::hash::Sha512Trunc256Sum;
use stacks_common::util::secp256k1::MessageSignature;
use stacks_common::util::vrf::VRFPublicKey;

use crate::types::chainstate::BurnchainHeaderHash;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use clarity::vm::ClarityName;

pub mod deposit_ft;
pub mod deposit_nft;
pub mod deposit_stx;
pub mod leader_block_commit;
pub mod withdraw_ft;
pub mod withdraw_nft;
pub mod withdraw_stx;

/// This module contains all burn-chain operations
#[derive(Debug)]
pub enum Error {
    /// Failed to parse the operation from the burnchain transaction
    ParseError,
    /// Invalid input data
    InvalidInput,
    /// Database error
    DBError(db_error),

    // all the things that can go wrong with block commits
    BlockCommitPredatesGenesis,
    BlockCommitAlreadyExists,
    BlockCommitNoLeaderKey,
    BlockCommitNoParent,
    BlockCommitBadInput,
    BlockCommitBadOutputs,
    BlockCommitAnchorCheck,
    BlockCommitBadModulus,
    BlockCommitBadEpoch,
    MissedBlockCommit(MissedBlockCommit),

    // all the things that can go wrong with leader key register
    LeaderKeyAlreadyRegistered,

    // all the things that can go wrong with user burn supports
    UserBurnSupportBadConsensusHash,
    UserBurnSupportNoLeaderKey,
    UserBurnSupportNotSupported,

    TransferStxMustBePositive,
    TransferStxSelfSend,

    StackStxMustBePositive,
    StackStxInvalidCycles,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::ParseError => write!(f, "Failed to parse transaction into Blockstack operation"),
            Error::InvalidInput => write!(f, "Invalid input"),
            Error::DBError(ref e) => fmt::Display::fmt(e, f),

            Error::BlockCommitPredatesGenesis => write!(f, "Block commit predates genesis block"),
            Error::BlockCommitAlreadyExists => {
                write!(f, "Block commit commits to an already-seen block")
            }
            Error::BlockCommitNoLeaderKey => write!(f, "Block commit has no matching register key"),
            Error::BlockCommitNoParent => write!(f, "Block commit parent does not exist"),
            Error::BlockCommitBadInput => write!(
                f,
                "Block commit tx input does not match register key tx output"
            ),
            Error::BlockCommitAnchorCheck => {
                write!(f, "Failure checking PoX anchor block for commit")
            }
            Error::BlockCommitBadOutputs => {
                write!(f, "Block commit included a bad commitment output")
            }
            Error::BlockCommitBadModulus => {
                write!(f, "Block commit included a bad burn block height modulus")
            }
            Error::BlockCommitBadEpoch => {
                write!(f, "Block commit has an invalid epoch")
            }
            Error::MissedBlockCommit(_) => write!(
                f,
                "Block commit included in a burn block that was not intended"
            ),
            Error::LeaderKeyAlreadyRegistered => {
                write!(f, "Leader key has already been registered")
            }
            Error::UserBurnSupportBadConsensusHash => {
                write!(f, "User burn support has an invalid consensus hash")
            }
            Error::UserBurnSupportNoLeaderKey => write!(
                f,
                "User burn support does not match a registered leader key"
            ),
            Error::UserBurnSupportNotSupported => {
                write!(f, "User burn operations are not supported")
            }
            Error::TransferStxMustBePositive => write!(f, "Transfer STX must be positive amount"),
            Error::TransferStxSelfSend => write!(f, "Transfer STX must not send to self"),
            Error::StackStxMustBePositive => write!(f, "Stack STX must be positive amount"),
            Error::StackStxInvalidCycles => write!(
                f,
                "Stack STX must set num cycles between 1 and max num cycles"
            ),
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::DBError(ref e) => Some(e),
            _ => None,
        }
    }
}

impl From<db_error> for Error {
    fn from(e: db_error) -> Error {
        Error::DBError(e)
    }
}

trait HexSerialization<T, E: fmt::Display> {
    fn ser_to_hex(&self) -> String;
    fn deser_from_hex(s: &str) -> Result<T, E>;
}

/// Implement HexSerialization for byte_array macro
///  defined types (i.e., types with to_hex and from_hex)
macro_rules! impl_hex_serialization {
    ($thing:ident) => {
        impl HexSerialization<$thing, HexError> for $thing {
            fn ser_to_hex(&self) -> String {
                self.to_hex()
            }
            fn deser_from_hex(s: &str) -> Result<Self, HexError> {
                Self::from_hex(s)
            }
        }
    };
}

impl_hex_serialization!(BurnchainHeaderHash);
impl_hex_serialization!(Txid);
impl_hex_serialization!(Sha512Trunc256Sum);

fn hex_serialize<S: serde::Serializer, T: HexSerialization<T, E>, E: fmt::Display>(
    bhh: &T,
    s: S,
) -> Result<S::Ok, S::Error> {
    let inst = bhh.ser_to_hex();
    s.serialize_str(inst.as_str())
}

fn hex_deserialize<'de, D: serde::Deserializer<'de>, T: HexSerialization<T, E>, E: fmt::Display>(
    d: D,
) -> Result<T, D::Error> {
    let inst_str = String::deserialize(d)?;
    T::deser_from_hex(&inst_str).map_err(serde::de::Error::custom)
}

fn qc_serialize<S: serde::Serializer>(
    qc: &QualifiedContractIdentifier,
    s: S,
) -> Result<S::Ok, S::Error> {
    let inst = qc.to_string();
    s.serialize_str(inst.as_str())
}

fn qc_deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<QualifiedContractIdentifier, D::Error> {
    let inst_str = String::deserialize(d)?;
    QualifiedContractIdentifier::parse(&inst_str).map_err(serde::de::Error::custom)
}

fn pd_serialize<S: serde::Serializer>(pd: &PrincipalData, s: S) -> Result<S::Ok, S::Error> {
    let inst = pd.to_string();
    s.serialize_str(inst.as_str())
}

fn pd_deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<PrincipalData, D::Error> {
    let inst_str = String::deserialize(d)?;
    PrincipalData::parse(&inst_str).map_err(serde::de::Error::custom)
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct TransferStxOp {
    pub sender: StacksAddress,
    pub recipient: StacksAddress,
    pub transfered_ustx: u128,
    pub memo: Vec<u8>,

    // common to all transactions
    pub txid: Txid,        // transaction ID
    pub vtxindex: u32,     // index in the block where this tx occurs
    pub block_height: u64, // block height at which this tx occurs
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash, // hash of the burn chain block header
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct StackStxOp {
    pub sender: StacksAddress,
    /// the PoX reward address
    pub reward_addr: StacksAddress,
    /// how many ustx this transaction locks
    pub stacked_ustx: u128,
    pub num_cycles: u8,

    // common to all transactions
    pub txid: Txid,                            // transaction ID
    pub vtxindex: u32,                         // index in the block where this tx occurs
    pub block_height: u64,                     // block height at which this tx occurs
    pub burn_header_hash: BurnchainHeaderHash, // hash of the burn chain block header
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct PreStxOp {
    /// the output address
    pub output: StacksAddress,

    // common to all transactions
    pub txid: Txid,                            // transaction ID
    pub vtxindex: u32,                         // index in the block where this tx occurs
    pub block_height: u64,                     // block height at which this tx occurs
    pub burn_header_hash: BurnchainHeaderHash, // hash of the burn chain block header
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct LeaderBlockCommitOp {
    /// Hash of the committed block (anchor block hash)
    pub block_header_hash: BlockHeaderHash,
    /// Merkle root of the withdrawal events
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub withdrawal_merkle_root: Sha512Trunc256Sum,
    /// Transaction ID of this commit op
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub txid: Txid,
    /// Hash of the base chain block that produced this commit op.
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash,
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct DepositStxOp {
    /// Transaction ID of this commit op
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub txid: Txid,
    /// Hash of the base chain block that produced this commit op.
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash,

    /// Amount of STX that was deposited
    pub amount: u128,
    /// The principal that performed the deposit
    #[serde(serialize_with = "pd_serialize", deserialize_with = "pd_deserialize")]
    pub sender: PrincipalData,
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct DepositFtOp {
    /// Transaction ID of this commit op
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub txid: Txid,
    /// Hash of the base chain block that produced this commit op.
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash,

    /// Contract ID on L1 chain for this fungible token
    #[serde(serialize_with = "qc_serialize", deserialize_with = "qc_deserialize")]
    pub l1_contract_id: QualifiedContractIdentifier,
    /// Contract ID on subnet for this fungible token
    #[serde(serialize_with = "qc_serialize", deserialize_with = "qc_deserialize")]
    pub subnet_contract_id: QualifiedContractIdentifier,
    /// Name of the function to call in the subnet contract to execute deposit
    pub subnet_function_name: ClarityName,
    /// Name of fungible token
    pub name: String,
    /// Amount of the fungible token that was deposited
    pub amount: u128,
    /// The principal that performed the deposit
    #[serde(serialize_with = "pd_serialize", deserialize_with = "pd_deserialize")]
    pub sender: PrincipalData,
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct DepositNftOp {
    /// Transaction ID of this commit op
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub txid: Txid,
    /// Hash of the base chain block that produced this commit op.
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash,

    /// Contract ID on L1 chain for this NFT
    #[serde(serialize_with = "qc_serialize", deserialize_with = "qc_deserialize")]
    pub l1_contract_id: QualifiedContractIdentifier,
    /// Contract ID on subnet for this NFT
    #[serde(serialize_with = "qc_serialize", deserialize_with = "qc_deserialize")]
    pub subnet_contract_id: QualifiedContractIdentifier,
    /// Name of the function to call in the subnet contract to execute deposit
    pub subnet_function_name: ClarityName,
    /// The ID of the NFT transferred
    pub id: u128,
    /// The principal that performed the deposit
    #[serde(serialize_with = "pd_serialize", deserialize_with = "pd_deserialize")]
    pub sender: PrincipalData,
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct WithdrawStxOp {
    /// Transaction ID of this commit op
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub txid: Txid,
    /// Hash of the base chain block that produced this commit op.
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash,

    /// Amount of STX that was withdrawn
    pub amount: u128,
    /// The principal that is the recipient of this withdrawal
    #[serde(serialize_with = "pd_serialize", deserialize_with = "pd_deserialize")]
    pub recipient: PrincipalData,
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct WithdrawFtOp {
    /// Transaction ID of this commit op
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub txid: Txid,
    /// Hash of the base chain block that produced this commit op.
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash,

    // Contract ID on L1 chain for this fungible token
    #[serde(serialize_with = "qc_serialize", deserialize_with = "qc_deserialize")]
    pub l1_contract_id: QualifiedContractIdentifier,
    /// The name of the fungible token
    pub name: String,
    /// Amount of the fungible token that was withdrawn
    pub amount: u128,
    /// The principal the contract is sending the fungible token to
    #[serde(serialize_with = "pd_serialize", deserialize_with = "pd_deserialize")]
    pub recipient: PrincipalData,
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct WithdrawNftOp {
    /// Transaction ID of this commit op
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub txid: Txid,
    /// Hash of the base chain block that produced this commit op.
    #[serde(serialize_with = "hex_serialize", deserialize_with = "hex_deserialize")]
    pub burn_header_hash: BurnchainHeaderHash,

    /// Contract ID on L1 chain for this NFT
    #[serde(serialize_with = "qc_serialize", deserialize_with = "qc_deserialize")]
    pub l1_contract_id: QualifiedContractIdentifier,
    /// The ID of the NFT being withdrawn
    pub id: u128,
    /// The principal the contract is sending the NFT to
    #[serde(serialize_with = "pd_serialize", deserialize_with = "pd_deserialize")]
    pub recipient: PrincipalData,
}

#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct LeaderKeyRegisterOp {
    pub consensus_hash: ConsensusHash, // consensus hash at time of issuance
    pub public_key: VRFPublicKey,      // EdDSA public key
    pub memo: Vec<u8>,                 // extra bytes in the op-return
    pub address: StacksAddress, // NOTE: no longer used for anything consensus-critical, but identifies the change address output

    // common to all transactions
    pub txid: Txid,                            // transaction ID
    pub vtxindex: u32,                         // index in the block where this tx occurs
    pub block_height: u64,                     // block height at which this tx occurs
    pub burn_header_hash: BurnchainHeaderHash, // hash of burn chain block
}

/// NOTE: this struct is currently not used
#[derive(Debug, PartialEq, Clone, Eq, Serialize, Deserialize)]
pub struct UserBurnSupportOp {
    pub address: StacksAddress,
    pub consensus_hash: ConsensusHash,
    pub public_key: VRFPublicKey,
    pub key_block_ptr: u32,
    pub key_vtxindex: u16,
    pub block_header_hash_160: Hash160,
    pub burn_fee: u64,

    // common to all transactions
    pub txid: Txid,                            // transaction ID
    pub vtxindex: u32,                         // index in the block where this tx occurs
    pub block_height: u64,                     // block height at which this tx occurs
    pub burn_header_hash: BurnchainHeaderHash, // hash of burnchain block with this tx
}

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockstackOperationType {
    LeaderBlockCommit(LeaderBlockCommitOp),
    DepositStx(DepositStxOp),
    DepositFt(DepositFtOp),
    DepositNft(DepositNftOp),
    WithdrawStx(WithdrawStxOp),
    WithdrawFt(WithdrawFtOp),
    WithdrawNft(WithdrawNftOp),
}

impl From<LeaderBlockCommitOp> for BlockstackOperationType {
    fn from(op: LeaderBlockCommitOp) -> Self {
        BlockstackOperationType::LeaderBlockCommit(op)
    }
}

impl From<DepositStxOp> for BlockstackOperationType {
    fn from(op: DepositStxOp) -> Self {
        BlockstackOperationType::DepositStx(op)
    }
}

impl From<DepositFtOp> for BlockstackOperationType {
    fn from(op: DepositFtOp) -> Self {
        BlockstackOperationType::DepositFt(op)
    }
}

impl From<DepositNftOp> for BlockstackOperationType {
    fn from(op: DepositNftOp) -> Self {
        BlockstackOperationType::DepositNft(op)
    }
}

impl From<WithdrawStxOp> for BlockstackOperationType {
    fn from(op: WithdrawStxOp) -> Self {
        BlockstackOperationType::WithdrawStx(op)
    }
}

impl From<WithdrawFtOp> for BlockstackOperationType {
    fn from(op: WithdrawFtOp) -> Self {
        BlockstackOperationType::WithdrawFt(op)
    }
}

impl From<WithdrawNftOp> for BlockstackOperationType {
    fn from(op: WithdrawNftOp) -> Self {
        BlockstackOperationType::WithdrawNft(op)
    }
}

impl BlockstackOperationType {
    pub fn txid(&self) -> Txid {
        self.txid_ref().clone()
    }

    pub fn txid_ref(&self) -> &Txid {
        match *self {
            BlockstackOperationType::LeaderBlockCommit(ref data) => &data.txid,
            BlockstackOperationType::DepositStx(ref data) => &data.txid,
            BlockstackOperationType::DepositFt(ref data) => &data.txid,
            BlockstackOperationType::DepositNft(ref data) => &data.txid,
            BlockstackOperationType::WithdrawStx(ref data) => &data.txid,
            BlockstackOperationType::WithdrawFt(ref data) => &data.txid,
            BlockstackOperationType::WithdrawNft(ref data) => &data.txid,
        }
    }

    pub fn vtxindex(&self) -> u32 {
        0
    }

    pub fn block_height(&self) -> u64 {
        panic!("Not implemented")
    }

    pub fn burn_header_hash(&self) -> BurnchainHeaderHash {
        match *self {
            BlockstackOperationType::LeaderBlockCommit(ref data) => data.burn_header_hash.clone(),
            BlockstackOperationType::DepositStx(ref data) => data.burn_header_hash.clone(),
            BlockstackOperationType::DepositFt(ref data) => data.burn_header_hash.clone(),
            BlockstackOperationType::DepositNft(ref data) => data.burn_header_hash.clone(),
            BlockstackOperationType::WithdrawStx(ref data) => data.burn_header_hash.clone(),
            BlockstackOperationType::WithdrawFt(ref data) => data.burn_header_hash.clone(),
            BlockstackOperationType::WithdrawNft(ref data) => data.burn_header_hash.clone(),
        }
    }

    #[cfg(test)]
    pub fn set_block_height(&mut self, height: u64) {
        match self {
            BlockstackOperationType::LeaderBlockCommit(ref mut data) => {
                data.set_burn_height(height)
            }
            BlockstackOperationType::DepositStx(ref mut data) => data.set_burn_height(height),
            BlockstackOperationType::DepositFt(ref mut data) => data.set_burn_height(height),
            BlockstackOperationType::DepositNft(ref mut data) => data.set_burn_height(height),
            BlockstackOperationType::WithdrawStx(ref mut data) => data.set_burn_height(height),
            BlockstackOperationType::WithdrawFt(ref mut data) => data.set_burn_height(height),
            BlockstackOperationType::WithdrawNft(ref mut data) => data.set_burn_height(height),
        };
    }

    #[cfg(test)]
    pub fn set_burn_header_hash(&mut self, hash: BurnchainHeaderHash) {
        match self {
            BlockstackOperationType::LeaderBlockCommit(ref mut data) => {
                data.burn_header_hash = hash
            }
            BlockstackOperationType::DepositStx(ref mut data) => data.burn_header_hash = hash,
            BlockstackOperationType::DepositFt(ref mut data) => data.burn_header_hash = hash,
            BlockstackOperationType::DepositNft(ref mut data) => data.burn_header_hash = hash,
            BlockstackOperationType::WithdrawStx(ref mut data) => data.burn_header_hash = hash,
            BlockstackOperationType::WithdrawFt(ref mut data) => data.burn_header_hash = hash,
            BlockstackOperationType::WithdrawNft(ref mut data) => data.burn_header_hash = hash,
        };
    }

    /// Explicit JSON serialization function for burnchain ops.
    pub fn blockstack_op_to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap()
    }
}

impl fmt::Display for BlockstackOperationType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            BlockstackOperationType::LeaderBlockCommit(ref op) => write!(f, "{:?}", op),
            BlockstackOperationType::DepositStx(ref op) => write!(f, "{:?}", op),
            BlockstackOperationType::DepositFt(ref op) => write!(f, "{:?}", op),
            BlockstackOperationType::DepositNft(ref op) => write!(f, "{:?}", op),
            BlockstackOperationType::WithdrawStx(ref op) => write!(f, "{:?}", op),
            BlockstackOperationType::WithdrawFt(ref op) => write!(f, "{:?}", op),
            BlockstackOperationType::WithdrawNft(ref op) => write!(f, "{:?}", op),
        }
    }
}

// parser helpers
pub fn parse_u128_from_be(bytes: &[u8]) -> Option<u128> {
    bytes.try_into().ok().map(u128::from_be_bytes)
}

pub fn parse_u32_from_be(bytes: &[u8]) -> Option<u32> {
    bytes.try_into().ok().map(u32::from_be_bytes)
}

pub fn parse_u16_from_be(bytes: &[u8]) -> Option<u16> {
    bytes.try_into().ok().map(u16::from_be_bytes)
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn deposit_ft() {
        let deposit_ft = DepositFtOp {
            txid: Txid([0x11; 32]),
            burn_header_hash: BurnchainHeaderHash([0xaa; 32]),
            l1_contract_id: QualifiedContractIdentifier::parse("SP000000000000000000002Q6VF78.bns")
                .unwrap(),
            subnet_contract_id: QualifiedContractIdentifier::parse(
                "SP000000000000000000002Q6VF78.bns",
            )
            .unwrap(),
            subnet_function_name: "deposit-mint".into(),
            name: "ft-name".into(),
            amount: 7381273163198273,
            sender: PrincipalData::parse("SP000000000000000000002Q6VF78.bns").unwrap(),
        }
        .into();

        let expected = r#"
        {
          "deposit_ft": {
            "amount": 7381273163198273,
            "burn_header_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "l1_contract_id": "SP000000000000000000002Q6VF78.bns",
            "name": "ft-name",
            "sender": "SP000000000000000000002Q6VF78.bns",
            "subnet_contract_id": "SP000000000000000000002Q6VF78.bns",
            "subnet_function_name": "deposit-mint",
            "txid": "1111111111111111111111111111111111111111111111111111111111111111"
          }
        }"#;

        assert_eq!(
            BlockstackOperationType::blockstack_op_to_json(&deposit_ft),
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }

    #[test]
    fn deposit_nft() {
        let deposit_nft = DepositNftOp {
            txid: Txid([0xf1; 32]),
            burn_header_hash: BurnchainHeaderHash([0xcc; 32]),
            l1_contract_id: QualifiedContractIdentifier::parse("SP000000000000000000002Q6VF78.bns")
                .unwrap(),
            subnet_contract_id: QualifiedContractIdentifier::parse(
                "SP000000000000000000002Q6VF78.bns",
            )
            .unwrap(),
            subnet_function_name: "deposit-nft-mint".into(),
            sender: PrincipalData::parse("SP000000000000000000002Q6VF78.bns").unwrap(),
            id: 123123,
        }
        .into();

        let expected = r#"
        {
          "deposit_nft": {
            "burn_header_hash": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "id": 123123,
            "l1_contract_id": "SP000000000000000000002Q6VF78.bns",
            "sender": "SP000000000000000000002Q6VF78.bns",
            "subnet_contract_id": "SP000000000000000000002Q6VF78.bns",
            "subnet_function_name": "deposit-nft-mint",
            "txid": "f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1"
          }
        }"#;

        assert_eq!(
            BlockstackOperationType::blockstack_op_to_json(&deposit_nft),
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }

    #[test]
    fn deposit_stx() {
        let deposit_stx = DepositStxOp {
            txid: Txid([0x33; 32]),
            burn_header_hash: BurnchainHeaderHash([0xaa; 32]),
            amount: 7381273163198273,
            sender: PrincipalData::parse("SP000000000000000000002Q6VF78").unwrap(),
        }
        .into();
        let expected = r#"
        {
          "deposit_stx": {
            "amount": 7381273163198273,
            "burn_header_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sender": "SP000000000000000000002Q6VF78",
            "txid": "3333333333333333333333333333333333333333333333333333333333333333"
          }
        }"#;
        assert_eq!(
            BlockstackOperationType::blockstack_op_to_json(&deposit_stx),
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }

    #[test]
    fn withdraw_ft() {
        let withdraw_ft = WithdrawFtOp {
            txid: Txid([0x11; 32]),
            burn_header_hash: BurnchainHeaderHash([0xaa; 32]),
            l1_contract_id: QualifiedContractIdentifier::parse("SP000000000000000000002Q6VF78.bns")
                .unwrap(),
            name: "ft-name".into(),
            amount: 7381273163198273,
            recipient: PrincipalData::parse("SP000000000000000000002Q6VF78.bns").unwrap(),
        }
        .into();
        let expected = r#"
        {
          "withdraw_ft": {
            "amount": 7381273163198273,
            "burn_header_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "l1_contract_id": "SP000000000000000000002Q6VF78.bns",
            "name": "ft-name",
            "recipient": "SP000000000000000000002Q6VF78.bns",
            "txid": "1111111111111111111111111111111111111111111111111111111111111111"
          }
        }"#;
        assert_eq!(
            BlockstackOperationType::blockstack_op_to_json(&withdraw_ft),
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }

    #[test]
    fn withdraw_nft() {
        let withdraw_nft = WithdrawNftOp {
            txid: Txid([0xf1; 32]),
            burn_header_hash: BurnchainHeaderHash([0xcc; 32]),
            l1_contract_id: QualifiedContractIdentifier::parse("SP000000000000000000002Q6VF78.bns")
                .unwrap(),
            recipient: PrincipalData::parse("SP000000000000000000002Q6VF78.bns").unwrap(),
            id: 123123,
        }
        .into();
        let expected = r#"
        {
          "withdraw_nft": {
            "burn_header_hash": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "id": 123123,
            "l1_contract_id": "SP000000000000000000002Q6VF78.bns",
            "recipient": "SP000000000000000000002Q6VF78.bns",
            "txid": "f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1"
          }
        }"#;
        assert_eq!(
            BlockstackOperationType::blockstack_op_to_json(&withdraw_nft),
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }

    #[test]
    fn withdraw_stx() {
        let withdraw_stx = WithdrawStxOp {
            txid: Txid([0x3b; 32]),
            burn_header_hash: BurnchainHeaderHash([0xba; 32]),
            amount: 7381273163198273,
            recipient: PrincipalData::parse("SP000000000000000000002Q6VF78").unwrap(),
        }
        .into();
        let expected = r#"
        {
          "withdraw_stx": {
            "amount": 7381273163198273,
            "burn_header_hash": "babababababababababababababababababababababababababababababababa",
            "recipient": "SP000000000000000000002Q6VF78",
            "txid": "3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b"
          }
        }"#;
        assert_eq!(
            BlockstackOperationType::blockstack_op_to_json(&withdraw_stx),
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }

    #[test]
    fn commit_op() {
        let commit_op = LeaderBlockCommitOp {
            txid: Txid([0xa1; 32]),
            burn_header_hash: BurnchainHeaderHash([0xaa; 32]),
            block_header_hash: BlockHeaderHash([0x12; 32]),
            withdrawal_merkle_root: Sha512Trunc256Sum([0x31; 32]),
        }
        .into();

        let expected = r#"
        {
          "leader_block_commit": {
            "block_header_hash": "1212121212121212121212121212121212121212121212121212121212121212",
            "burn_header_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "txid": "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
            "withdrawal_merkle_root": "3131313131313131313131313131313131313131313131313131313131313131"
          }
        }
        "#;
        assert_eq!(
            BlockstackOperationType::blockstack_op_to_json(&commit_op),
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }
}
