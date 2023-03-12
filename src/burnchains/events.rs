use std::convert::{TryFrom, TryInto};
use std::fmt::Display;
use std::fmt::Formatter;

use crate::burnchains::Txid;
use clarity::vm::types::Value as ClarityValue;
use clarity::vm::types::{QualifiedContractIdentifier, TraitIdentifier};
use serde::de::Error as DeserError;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serializer;
use stacks_common::util::HexError;

use crate::types::chainstate::BlockHeaderHash;
use crate::types::chainstate::StacksBlockId;
use crate::vm::representations::ClarityName;
use crate::vm::types::CharType;
use crate::vm::types::SequenceData;
use stacks_common::util::hash::{to_hex, Sha512Trunc256Sum};

use super::StacksSubnetBlock;
use super::StacksSubnetOp;
use super::StacksSubnetOpType;
use clarity::vm::types::PrincipalData;
use stacks_common::codec::StacksMessageCodec;
use std::fmt::Write;

/// Parsing struct for the transaction event types of the
/// `stacks-node` events API
#[derive(PartialEq, Clone, Debug, Serialize)]
pub enum TxEventType {
    ContractEvent,
    Other,
}

/// Parsing struct for the contract_event field in transaction events
/// of the `stacks-node` events API
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContractEvent {
    #[serde(serialize_with = "ser_contract_identifier")]
    #[serde(deserialize_with = "deser_contract_identifier")]
    pub contract_identifier: QualifiedContractIdentifier,
    pub topic: String,
    #[serde(rename = "raw_value", serialize_with = "ser_clarity_value")]
    #[serde(deserialize_with = "deser_clarity_value")]
    pub value: ClarityValue,
}

/// Parsing struct for the transaction events of the `stacks-node`
/// events API
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NewBlockTxEvent {
    #[serde(serialize_with = "ser_as_hexstr")]
    #[serde(deserialize_with = "deser_txid")]
    pub txid: Txid,
    pub event_index: usize,
    pub committed: bool,
    #[serde(
        rename = "type",
        serialize_with = "ser_tx_event_type",
        deserialize_with = "deser_tx_event_type"
    )]
    pub event_type: TxEventType,
    #[serde(default)]
    pub contract_event: Option<ContractEvent>,
}

/// Parsing struct for the new block events of the `stacks-node`
/// events API
#[derive(Clone, Serialize, Deserialize)]
pub struct NewBlock {
    pub block_height: u64,
    pub burn_block_time: u64,
    #[serde(serialize_with = "ser_as_hexstr")]
    #[serde(deserialize_with = "deser_stacks_block_id")]
    pub index_block_hash: StacksBlockId,
    #[serde(serialize_with = "ser_as_hexstr")]
    #[serde(deserialize_with = "deser_stacks_block_id")]
    pub parent_index_block_hash: StacksBlockId,
    pub events: Vec<NewBlockTxEvent>,
}

impl std::fmt::Debug for NewBlock {
    /// Shortened debug string, for logging.
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "NewBlock(hash={:?}, parent_hash={:?}, block_height={}, num_events={})",
            &self.index_block_hash,
            &self.parent_index_block_hash,
            self.block_height,
            self.events.len()
        )
    }
}

/// Method for deserializing a ClarityValue from the `raw_value` field of contract
/// transaction events.
fn deser_clarity_value<'de, D>(deser: D) -> Result<ClarityValue, D::Error>
where
    D: Deserializer<'de>,
{
    let str_val = String::deserialize(deser)?;
    ClarityValue::try_deserialize_hex_untyped(&str_val).map_err(DeserError::custom)
}

/// Serialize a clarity value to work with `deser_clarity_value`.
fn ser_clarity_value<S>(value: &ClarityValue, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let byte_serialization = value.serialize_to_vec();
    let string_value = to_hex(byte_serialization.as_slice());
    s.serialize_str(&string_value)
}

/// Method for deserializing a contract identifier from `contract_identifier` fields in
/// transaction events.
fn deser_contract_identifier<'de, D>(deser: D) -> Result<QualifiedContractIdentifier, D::Error>
where
    D: Deserializer<'de>,
{
    let str_val = String::deserialize(deser)?;
    QualifiedContractIdentifier::parse(&str_val).map_err(DeserError::custom)
}

/// Serialize a contract to work with `deser_contract_identifier`.
fn ser_contract_identifier<S>(
    contract_id: &QualifiedContractIdentifier,
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(&contract_id.to_string())
}

/// Method for deserializing a `Txid` from transaction events.
fn deser_txid<'de, D>(deser: D) -> Result<Txid, D::Error>
where
    D: Deserializer<'de>,
{
    let str_val = String::deserialize(deser)?;
    match str_val.get(2..) {
        Some(hex) => Txid::from_hex(hex).map_err(DeserError::custom),
        None => Err(DeserError::custom(HexError::BadLength(2))),
    }
}

/// Method for deserializing a `StacksBlockId` from transaction events.
fn deser_stacks_block_id<'de, D>(deser: D) -> Result<StacksBlockId, D::Error>
where
    D: Deserializer<'de>,
{
    let str_val = String::deserialize(deser)?;
    match str_val.get(2..) {
        Some(hex) => StacksBlockId::from_hex(hex).map_err(DeserError::custom),
        None => Err(DeserError::custom(HexError::BadLength(2))),
    }
}

// Only works if Display implementation uses a hex string, which Txid and StacksBlockId do
fn ser_as_hexstr<T: Display, S: Serializer>(input: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format!("0x{}", &input))
}

/// Method for deserializing a `TxEventType` from transaction events.
/// This module is currently only interested in `contract_event` types,
/// so all other events are parsed as `Other`.
fn deser_tx_event_type<'de, D>(deser: D) -> Result<TxEventType, D::Error>
where
    D: Deserializer<'de>,
{
    let str_val = String::deserialize(deser)?;
    match str_val.as_str() {
        "contract_event" => Ok(TxEventType::ContractEvent),
        _ => Ok(TxEventType::Other),
    }
}

/// Counter-part for deser_tx_event_type.
fn ser_tx_event_type<S: Serializer>(input: &TxEventType, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let as_str = match input {
        TxEventType::ContractEvent => "contract_event",
        TxEventType::Other => "other",
    };
    serializer.serialize_str(as_str)
}

impl StacksSubnetOp {
    /// This method tries to parse a `StacksSubnetOp` from a Clarity value: this should be a tuple
    /// emitted from the subnet contract in a statement like:
    /// `(print { event: "block-commit", block-commit: 0x123... })`
    ///
    /// If the provided value does not match that tuple, this method will return an error.
    pub fn try_from_clar_value(
        v: ClarityValue,
        txid: Txid,
        event_index: u32,
        in_block: &StacksBlockId,
    ) -> Result<Self, String> {
        let tuple = if let ClarityValue::Tuple(tuple) = v {
            Ok(tuple)
        } else {
            Err("Expected Clarity type to be tuple")
        }?;

        let event = tuple
            .get("event")
            .map_err(|_| "No 'event' field in Clarity tuple")?;
        let event = if let ClarityValue::Sequence(SequenceData::String(clar_str)) = event {
            Ok(clar_str.to_string())
        } else {
            Err("Expected 'event' type to be string")
        }?;

        match event.as_str() {
            "\"block-commit\"" => {
                let block_commit = tuple
                    .get("block-commit")
                    .map_err(|_| "No 'block-commit' field in Clarity tuple")?;
                let block_commit =
                    if let ClarityValue::Sequence(SequenceData::Buffer(buff_data)) = block_commit {
                        if u32::from(buff_data.len()) != 32 {
                            Err(format!(
                                "Expected 'block-commit' type to be length 32, found {}",
                                buff_data.len()
                            ))
                        } else {
                            let mut buff = [0; 32];
                            buff.copy_from_slice(&buff_data.data);
                            Ok(buff)
                        }
                    } else {
                        Err("Expected 'block-commit' type to be buffer".into())
                    }?;
                let withdrawal_merkle_root = tuple
                    .get("withdrawal-root")
                    .map_err(|_| "No 'withdrawal-root' field in Clarity tuple")?;
                let withdrawal_merkle_root =
                    if let ClarityValue::Sequence(SequenceData::Buffer(buff_data)) =
                        withdrawal_merkle_root
                    {
                        if u32::from(buff_data.len()) != 32 {
                            Err(format!(
                                "Expected 'withdrawal-root' type to be length 32, found {}",
                                buff_data.len()
                            ))
                        } else {
                            let mut buff = [0; 32];
                            buff.copy_from_slice(&buff_data.data);
                            Ok(buff)
                        }
                    } else {
                        Err("Expected 'withdrawal-root' type to be buffer".into())
                    }?;
                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 0,
                    event: StacksSubnetOpType::BlockCommit {
                        subnet_block_hash: BlockHeaderHash(block_commit),
                        withdrawal_merkle_root: Sha512Trunc256Sum(withdrawal_merkle_root),
                    },
                })
            }
            "\"register-contract\"" => {
                // Parse 3 fields: asset-type, l1-contract, l2-contract
                let asset_type_string = tuple
                    .get("asset-type")
                    .map_err(|_| "No 'asset-type' field in Clarity tuple")?
                    .clone()
                    .expect_ascii();
                let asset_type = asset_type_string.parse().map_err(|_| {
                    format!(
                        "Expected 'asset-type' to be a valid asset type, found '{}'",
                        asset_type_string
                    )
                })?;
                let l1_contract = tuple
                    .get("l1-contract")
                    .map_err(|_| "No 'l1-contract' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let l1_contract_id = if let PrincipalData::Contract(id) = l1_contract {
                    Ok(id)
                } else {
                    Err("Expected 'l1-contract-id' to be a contract principal")
                }?;
                let l2_contract = tuple
                    .get("l2-contract")
                    .map_err(|_| "No 'l2-contract' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let l2_contract_id = if let PrincipalData::Contract(id) = l2_contract {
                    Ok(id)
                } else {
                    Err("Expected 'l1-contract-id' to be a contract principal")
                }?;

                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 6,
                    event: StacksSubnetOpType::RegisterAsset {
                        asset_type,
                        l1_contract_id,
                        l2_contract_id,
                    },
                })
            }
            "\"deposit-stx\"" => {
                // Parse 2 fields: amount and sender
                let amount = tuple
                    .get("amount")
                    .map_err(|_| "No 'amount' field in Clarity tuple")?
                    .clone()
                    .expect_u128();
                let sender = tuple
                    .get("sender")
                    .map_err(|_| "No 'sender' field in Clarity tuple")?
                    .clone()
                    .expect_principal();

                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 1,
                    event: StacksSubnetOpType::DepositStx { amount, sender },
                })
            }
            "\"deposit-ft\"" => {
                // Parse 5 fields: l1-contract-id, ft-name, ft-amount, sender, and subnet-contract-id
                let l1_contract_id = tuple
                    .get("l1-contract-id")
                    .map_err(|_| "No 'l1-contract-id' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let l1_contract_id = if let PrincipalData::Contract(id) = l1_contract_id {
                    Ok(id)
                } else {
                    Err("Expected 'l1-contract-id' to be a contract principal")
                }?;
                let name = tuple
                    .get("ft-name")
                    .map_err(|_| "No 'ft-name' field in Clarity tuple")?
                    .clone()
                    .expect_ascii();
                let amount = tuple
                    .get("ft-amount")
                    .map_err(|_| "No 'ft-amount' field in Clarity tuple")?
                    .clone()
                    .expect_u128();
                let sender = tuple
                    .get("sender")
                    .map_err(|_| "No 'sender' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let subnet_contract_id = tuple
                    .get("subnet-contract-id")
                    .map_err(|_| "No 'subnet-contract-id' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let subnet_contract_id = if let PrincipalData::Contract(id) = subnet_contract_id {
                    Ok(id)
                } else {
                    Err("Expected 'subnet-contract-id' to be a contract principal")
                }?;

                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 2,
                    event: StacksSubnetOpType::DepositFt {
                        l1_contract_id,
                        subnet_contract_id,
                        name,
                        amount,
                        sender,
                    },
                })
            }
            "\"deposit-nft\"" => {
                // Parse 4 fields: l1-contract-id, nft-id, sender, and subnet-contract-id
                // check that this is a valid way of getting the ID of the L1 contract.
                let l1_contract_id = tuple
                    .get("l1-contract-id")
                    .map_err(|_| "No 'l1-contract-id' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let l1_contract_id = if let PrincipalData::Contract(id) = l1_contract_id {
                    Ok(id)
                } else {
                    Err("Expected 'l1-contract-id' to be a contract principal")
                }?;
                let id = tuple
                    .get("nft-id")
                    .map_err(|_| "No 'nft-id' field in Clarity tuple")?
                    .clone()
                    .expect_u128();
                let sender = tuple
                    .get("sender")
                    .map_err(|_| "No 'sender' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let subnet_contract_id = tuple
                    .get("subnet-contract-id")
                    .map_err(|_| "No 'subnet-contract-id' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let subnet_contract_id = if let PrincipalData::Contract(id) = subnet_contract_id {
                    Ok(id)
                } else {
                    Err("Expected 'subnet-contract-id' to be a contract principal")
                }?;

                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 3,
                    event: StacksSubnetOpType::DepositNft {
                        l1_contract_id,
                        subnet_contract_id,
                        id,
                        sender,
                    },
                })
            }
            "\"withdraw-stx\"" => {
                // Parse 2 fields: amount and recipient
                let amount = tuple
                    .get("amount")
                    .map_err(|_| "No 'amount' field in Clarity tuple")?
                    .clone()
                    .expect_u128();
                let recipient = tuple
                    .get("recipient")
                    .map_err(|_| "No 'recipient' field in Clarity tuple")?
                    .clone()
                    .expect_principal();

                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 1,
                    event: StacksSubnetOpType::WithdrawStx { amount, recipient },
                })
            }
            "\"withdraw-ft\"" => {
                // Parse 4 fields: ft-amount, ft-name, l1-contract-id, and recipient
                let amount = tuple
                    .get("ft-amount")
                    .map_err(|_| "No 'ft-amount' field in Clarity tuple")?
                    .clone()
                    .expect_u128();
                let l1_contract_id = tuple
                    .get("l1-contract-id")
                    .map_err(|_| "No 'l1-contract-id' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let l1_contract_id = if let PrincipalData::Contract(id) = l1_contract_id {
                    Ok(id)
                } else {
                    Err("Expected 'l1-contract-id' to be a contract principal")
                }?;
                let name = tuple
                    .get("ft-name")
                    .map_err(|_| "No 'ft-name' field in Clarity tuple")?
                    .clone()
                    .expect_ascii();
                let recipient = tuple
                    .get("recipient")
                    .map_err(|_| "No 'recipient' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 4,
                    event: StacksSubnetOpType::WithdrawFt {
                        l1_contract_id,
                        name,
                        amount,
                        recipient,
                    },
                })
            }
            "\"withdraw-nft\"" => {
                // Parse 3 fields: nft-id, l1-contract-id, and recipient
                let id = tuple
                    .get("nft-id")
                    .map_err(|_| "No 'nft-id' field in Clarity tuple")?
                    .clone()
                    .expect_u128();
                // check that this is a valid way of getting the ID of the L1 contract.
                let l1_contract_id = tuple
                    .get("l1-contract-id")
                    .map_err(|_| "No 'l1-contract-id' field in Clarity tuple")?
                    .clone()
                    .expect_principal();
                let l1_contract_id = if let PrincipalData::Contract(id) = l1_contract_id {
                    Ok(id)
                } else {
                    Err("Expected 'l1-contract-id' to be a contract principal")
                }?;
                let recipient = tuple
                    .get("recipient")
                    .map_err(|_| "No 'recipient' field in Clarity tuple")?
                    .clone()
                    .expect_principal();

                Ok(Self {
                    txid,
                    event_index,
                    in_block: in_block.clone(),
                    opcode: 5,
                    event: StacksSubnetOpType::WithdrawNft {
                        l1_contract_id,
                        id,
                        recipient,
                    },
                })
            }
            event_type => Err(format!("Unexpected 'event' string: {}", event_type)),
        }
    }
}

impl StacksSubnetBlock {
    /// Process a `NewBlock` event from a layer-1 Stacks node, filter
    /// for the transaction events in the block that are relevant to
    /// the subnet and parse out the `StacksSubnetOp`s from the
    /// block, producing a `StacksSubnetBlock` struct.
    pub fn from_new_block_event(
        subnet_contract: &QualifiedContractIdentifier,
        b: NewBlock,
    ) -> Self {
        let NewBlock {
            events,
            index_block_hash,
            parent_index_block_hash,
            block_height,
            ..
        } = b;

        let ops = events
            .into_iter()
            .filter_map(|e| {
                if !e.committed {
                    None
                } else if e.event_type != TxEventType::ContractEvent {
                    None
                } else {
                    let NewBlockTxEvent {
                        txid,
                        contract_event,
                        event_index,
                        ..
                    } = e;

                    let event_index: u32 = match event_index.try_into() {
                        Ok(x) => Some(x),
                        Err(_e) => {
                            warn!(
                                "StacksSubnetBlock skipped event because event_index was not a u32"
                            );
                            None
                        }
                    }?;

                    if let Some(contract_event) = contract_event {
                        if &contract_event.contract_identifier != subnet_contract {
                            None
                        } else {
                            match StacksSubnetOp::try_from_clar_value(
                                contract_event.value,
                                txid,
                                event_index,
                                &index_block_hash,
                            ) {
                                Ok(x) => Some(x),
                                Err(e) => {
                                    info!(
                                        "StacksSubnetBlock parser skipped event because of {:?}",
                                        e
                                    );
                                    None
                                }
                            }
                        }
                    } else {
                        None
                    }
                }
            })
            .collect();

        Self {
            current_block: index_block_hash,
            parent_block: parent_index_block_hash,
            block_height,
            ops,
        }
    }
}
