use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::convert::TryInto;

use crate::chainstate::burn::ConsensusHash;
use crate::chainstate::stacks::boot::BOOT_CODE_COST_VOTING_TESTNET as BOOT_CODE_COST_VOTING;
use crate::chainstate::stacks::db::{MinerPaymentSchedule, StacksHeaderInfo};
use crate::chainstate::stacks::index::MarfTrieId;
use crate::chainstate::stacks::index::{ClarityMarfTrieId, TrieMerkleProof};
use crate::chainstate::stacks::C32_ADDRESS_VERSION_TESTNET_SINGLESIG;
use crate::chainstate::stacks::*;
use crate::clarity_vm::database::marf::MarfedKV;
use crate::core::{
    BITCOIN_REGTEST_FIRST_BLOCK_HASH, BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT,
    BITCOIN_REGTEST_FIRST_BLOCK_TIMESTAMP, BLOCK_INVENTORY_SYNC_CYCLE_SIZE,
    FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH, SUBNETS_STACKS_EPOCH,
};
use crate::util_lib::db::{DBConn, FromRow};
use clarity::vm::analysis::arithmetic_checker::ArithmeticOnlyChecker;
use clarity::vm::analysis::mem_type_check;
use clarity::vm::ast::ASTRules;
use clarity::vm::contexts::OwnedEnvironment;
use clarity::vm::contracts::Contract;
use clarity::vm::costs::CostOverflowingMath;
use clarity::vm::database::*;
use clarity::vm::errors::{
    CheckErrors, Error, IncomparableError, InterpreterError, InterpreterResult as Result,
    RuntimeErrorType,
};
use clarity::vm::eval;
use clarity::vm::representations::SymbolicExpression;
use clarity::vm::test_util::{execute, symbols_from_values, TEST_BURN_STATE_DB, TEST_HEADER_DB};
use clarity::vm::types::Value::Response;
use clarity::vm::types::{
    OptionalData, PrincipalData, QualifiedContractIdentifier, ResponseData, StandardPrincipalData,
    TupleData, TupleTypeSignature, TypeSignature, Value, NONE,
};
use stacks_common::address::AddressHashMode;
use stacks_common::util::hash::to_hex;
use stacks_common::util::hash::{Sha256Sum, Sha512Trunc256Sum};

use crate::util_lib::boot::boot_code_addr;
use crate::util_lib::boot::boot_code_id;
use crate::{
    burnchains::PoxConstants,
    clarity_vm::{clarity::ClarityBlockConnection, database::marf::WritableMarfStore},
    core::StacksEpoch,
};
use crate::{
    core::StacksEpochId,
    types::chainstate::{
        BlockHeaderHash, BurnchainHeaderHash, StacksAddress, StacksBlockId, VRFSeed,
    },
};

use crate::clarity_vm::clarity::Error as ClarityError;
use crate::core::PEER_VERSION_EPOCH_1_0;

const USTX_PER_HOLDER: u128 = 1_000_000;

lazy_static! {
    static ref FIRST_INDEX_BLOCK_HASH: StacksBlockId = StacksBlockHeader::make_index_block_hash(
        &FIRST_BURNCHAIN_CONSENSUS_HASH,
        &FIRST_STACKS_BLOCK_HASH
    );
    static ref POX_CONTRACT_TESTNET: QualifiedContractIdentifier = boot_code_id("pox", false);
    static ref COST_VOTING_CONTRACT_TESTNET: QualifiedContractIdentifier =
        boot_code_id("cost-voting", false);
    static ref USER_KEYS: Vec<StacksPrivateKey> =
        (0..50).map(|_| StacksPrivateKey::new()).collect();
    static ref POX_ADDRS: Vec<Value> = (0..50u64)
        .map(|ix| execute(&format!(
            "{{ version: 0x00, hashbytes: 0x000000000000000000000000{} }}",
            &to_hex(&ix.to_le_bytes())
        )))
        .collect();
    static ref MINER_KEY: StacksPrivateKey = StacksPrivateKey::new();
    static ref MINER_ADDR: StacksAddress = StacksAddress::from_public_keys(
        C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![StacksPublicKey::from_private(&MINER_KEY.clone())],
    )
    .unwrap();
    static ref LIQUID_SUPPLY: u128 = USTX_PER_HOLDER * (POX_ADDRS.len() as u128);
    static ref MIN_THRESHOLD: u128 = *LIQUID_SUPPLY / super::test::TESTNET_STACKING_THRESHOLD_25;
}

pub struct ClarityTestSim {
    marf: MarfedKV,
    height: u64,
    fork: u64,
}

struct TestSimHeadersDB {
    height: u64,
}

impl ClarityTestSim {
    pub fn new() -> ClarityTestSim {
        let mut marf = MarfedKV::temporary();
        {
            let mut store = marf.begin(
                &StacksBlockId::sentinel(),
                &StacksBlockId(test_sim_height_to_hash(0, 0)),
            );

            store
                .as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB)
                .initialize();

            let mut owned_env = OwnedEnvironment::new(
                store.as_clarity_db(&TEST_HEADER_DB, &TEST_BURN_STATE_DB),
                SUBNETS_STACKS_EPOCH,
            );

            for user_key in USER_KEYS.iter() {
                owned_env.stx_faucet(
                    &StandardPrincipalData::from(user_key).into(),
                    USTX_PER_HOLDER,
                );
            }
            store.test_commit();
        }

        ClarityTestSim {
            marf,
            height: 0,
            fork: 0,
        }
    }

    pub fn execute_next_block<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut OwnedEnvironment) -> R,
    {
        let mut store = self.marf.begin(
            &StacksBlockId(test_sim_height_to_hash(self.height, self.fork)),
            &StacksBlockId(test_sim_height_to_hash(self.height + 1, self.fork)),
        );

        let r = {
            let headers_db = TestSimHeadersDB {
                height: self.height + 1,
            };
            let mut owned_env = OwnedEnvironment::new(
                store.as_clarity_db(&headers_db, &TEST_BURN_STATE_DB),
                SUBNETS_STACKS_EPOCH,
            );
            f(&mut owned_env)
        };

        store.test_commit();
        self.height += 1;

        r
    }

    pub fn execute_block_as_fork<F, R>(&mut self, parent_height: u64, f: F) -> R
    where
        F: FnOnce(&mut OwnedEnvironment) -> R,
    {
        let mut store = self.marf.begin(
            &StacksBlockId(test_sim_height_to_hash(parent_height, self.fork)),
            &StacksBlockId(test_sim_height_to_hash(parent_height + 1, self.fork + 1)),
        );

        let r = {
            let headers_db = TestSimHeadersDB {
                height: parent_height + 1,
            };
            let mut owned_env = OwnedEnvironment::new(
                store.as_clarity_db(&headers_db, &TEST_BURN_STATE_DB),
                SUBNETS_STACKS_EPOCH,
            );
            f(&mut owned_env)
        };

        store.test_commit();
        self.height = parent_height + 1;
        self.fork += 1;

        r
    }
}

fn test_sim_height_to_hash(burn_height: u64, fork: u64) -> [u8; 32] {
    let mut out = [0; 32];
    out[0..8].copy_from_slice(&burn_height.to_le_bytes());
    out[8..16].copy_from_slice(&fork.to_le_bytes());
    out
}

fn test_sim_hash_to_height(in_bytes: &[u8; 32]) -> Option<u64> {
    if &in_bytes[8..] != &[0; 24] {
        None
    } else {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&in_bytes[0..8]);
        Some(u64::from_le_bytes(bytes))
    }
}

fn check_arithmetic_only(contract: &str) {
    let analysis = mem_type_check(contract, ClarityVersion::Clarity2, StacksEpochId::latest())
        .unwrap()
        .1;
    ArithmeticOnlyChecker::run(&analysis).expect("Should pass arithmetic checks");
}

#[test]
fn cost_contract_is_arithmetic_only() {
    use crate::chainstate::stacks::boot::BOOT_CODE_COSTS;
    check_arithmetic_only(BOOT_CODE_COSTS);
}

#[test]
fn cost_2_contract_is_arithmetic_only() {
    use crate::chainstate::stacks::boot::BOOT_CODE_COSTS_2;
    check_arithmetic_only(BOOT_CODE_COSTS_2);
}

impl HeadersDB for TestSimHeadersDB {
    fn get_burn_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
    ) -> Option<BurnchainHeaderHash> {
        if *id_bhh == *FIRST_INDEX_BLOCK_HASH {
            Some(BurnchainHeaderHash::from_hex(BITCOIN_REGTEST_FIRST_BLOCK_HASH).unwrap())
        } else {
            self.get_burn_block_height_for_block(id_bhh)?;
            Some(BurnchainHeaderHash(id_bhh.0.clone()))
        }
    }

    fn get_vrf_seed_for_block(&self, _bhh: &StacksBlockId) -> Option<VRFSeed> {
        None
    }

    fn get_stacks_block_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
    ) -> Option<BlockHeaderHash> {
        if *id_bhh == *FIRST_INDEX_BLOCK_HASH {
            Some(FIRST_STACKS_BLOCK_HASH)
        } else {
            self.get_burn_block_height_for_block(id_bhh)?;
            Some(BlockHeaderHash(id_bhh.0.clone()))
        }
    }

    fn get_burn_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64> {
        if *id_bhh == *FIRST_INDEX_BLOCK_HASH {
            Some(BITCOIN_REGTEST_FIRST_BLOCK_TIMESTAMP as u64)
        } else {
            let burn_block_height = self.get_burn_block_height_for_block(id_bhh)? as u64;
            Some(
                BITCOIN_REGTEST_FIRST_BLOCK_TIMESTAMP as u64 + burn_block_height
                    - BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT as u64,
            )
        }
    }
    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32> {
        if *id_bhh == *FIRST_INDEX_BLOCK_HASH {
            Some(BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT as u32)
        } else {
            let input_height = test_sim_hash_to_height(&id_bhh.0)?;
            if input_height > self.height {
                eprintln!("{} > {}", input_height, self.height);
                None
            } else {
                Some(
                    (BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT as u32 + input_height as u32)
                        .try_into()
                        .unwrap(),
                )
            }
        }
    }
    fn get_miner_address(&self, _id_bhh: &StacksBlockId) -> Option<StacksAddress> {
        Some(MINER_ADDR.clone())
    }

    fn get_consensus_hash_for_block(
        &self,
        _: &stacks_common::types::chainstate::StacksBlockId,
    ) -> std::option::Option<stacks_common::types::chainstate::ConsensusHash> {
        todo!()
    }
    fn get_burnchain_tokens_spent_for_block(
        &self,
        _: &stacks_common::types::chainstate::StacksBlockId,
    ) -> std::option::Option<u128> {
        todo!()
    }
    fn get_burnchain_tokens_spent_for_winning_block(
        &self,
        _: &stacks_common::types::chainstate::StacksBlockId,
    ) -> std::option::Option<u128> {
        todo!()
    }
    fn get_tokens_earned_for_block(
        &self,
        _: &stacks_common::types::chainstate::StacksBlockId,
    ) -> std::option::Option<u128> {
        todo!()
    }
}

#[test]
fn test_vote_withdrawal() {
    let mut sim = ClarityTestSim::new();

    sim.execute_next_block(|env| {
        env.initialize_versioned_contract(
            COST_VOTING_CONTRACT_TESTNET.clone(),
            ClarityVersion::Clarity1,
            &BOOT_CODE_COST_VOTING,
            None,
            ASTRules::PrecheckSize,
        )
        .unwrap();

        // Submit a proposal
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "submit-proposal",
                &symbols_from_values(vec![
                    Value::Principal(
                        PrincipalData::parse_qualified_contract_principal(
                            "ST000000000000000000002AMW42H.function-name"
                        )
                        .unwrap()
                    ),
                    Value::string_ascii_from_bytes("function-name".into()).unwrap(),
                    Value::Principal(
                        PrincipalData::parse_qualified_contract_principal(
                            "ST000000000000000000002AMW42H.cost-function-name"
                        )
                        .unwrap()
                    ),
                    Value::string_ascii_from_bytes("cost-function-name".into()).unwrap(),
                ])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: true,
                data: Value::UInt(0).into()
            })
        );

        // Vote on the proposal
        env.execute_transaction(
            (&USER_KEYS[0]).into(),
            None,
            COST_VOTING_CONTRACT_TESTNET.clone(),
            "vote-proposal",
            &symbols_from_values(vec![Value::UInt(0), Value::UInt(10)]),
        )
        .unwrap()
        .0;

        // Assert that the number of votes is correct
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "get-proposal-votes",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Optional(OptionalData {
                data: Some(Box::from(Value::UInt(10)))
            })
        );

        // Vote again on the proposal
        env.execute_transaction(
            (&USER_KEYS[0]).into(),
            None,
            COST_VOTING_CONTRACT_TESTNET.clone(),
            "vote-proposal",
            &symbols_from_values(vec![Value::UInt(0), Value::UInt(5)]),
        )
        .unwrap()
        .0;

        // Assert that the number of votes is correct
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "get-proposal-votes",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Optional(OptionalData {
                data: Some(Box::from(Value::UInt(15)))
            })
        );

        // Assert votes are assigned to principal
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "get-principal-votes",
                &symbols_from_values(vec![
                    Value::Principal(StandardPrincipalData::from(&USER_KEYS[0]).into()),
                    Value::UInt(0),
                ])
            )
            .unwrap()
            .0,
            Value::Optional(OptionalData {
                data: Some(Box::from(Value::UInt(15)))
            })
        );

        // Assert withdrawal fails if amount is more than voted
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "withdraw-votes",
                &symbols_from_values(vec![Value::UInt(0), Value::UInt(20)]),
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: false,
                data: Value::Int(5).into()
            })
        );

        // Withdraw votes
        env.execute_transaction(
            (&USER_KEYS[0]).into(),
            None,
            COST_VOTING_CONTRACT_TESTNET.clone(),
            "withdraw-votes",
            &symbols_from_values(vec![Value::UInt(0), Value::UInt(5)]),
        )
        .unwrap();

        // Assert withdrawal worked
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "get-proposal-votes",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Optional(OptionalData {
                data: Some(Box::from(Value::UInt(10)))
            })
        );
    });

    // Fast forward to proposal expiration
    for _ in 0..2016 {
        sim.execute_next_block(|_| {});
    }

    sim.execute_next_block(|env| {
        // Withdraw STX after proposal expires
        env.execute_transaction(
            (&USER_KEYS[0]).into(),
            None,
            COST_VOTING_CONTRACT_TESTNET.clone(),
            "withdraw-votes",
            &symbols_from_values(vec![Value::UInt(0), Value::UInt(10)]),
        )
        .unwrap();
    });

    sim.execute_next_block(|env| {
        // Assert that stx balance is correct
        assert_eq!(
            env.eval_read_only(
                &COST_VOTING_CONTRACT_TESTNET,
                &format!("(stx-get-balance '{})", &Value::from(&USER_KEYS[0]))
            )
            .unwrap()
            .0,
            Value::UInt(1000000)
        );
    });
}

#[test]
fn test_vote_fail() {
    let mut sim = ClarityTestSim::new();

    // Test voting in a proposal
    sim.execute_next_block(|env| {
        env.initialize_contract(
            COST_VOTING_CONTRACT_TESTNET.clone(),
            &BOOT_CODE_COST_VOTING,
            None,
            ASTRules::PrecheckSize,
        )
        .unwrap();

        // Submit a proposal
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "submit-proposal",
                &symbols_from_values(vec![
                    Value::Principal(
                        PrincipalData::parse_qualified_contract_principal(
                            "ST000000000000000000002AMW42H.function-name2"
                        )
                        .unwrap()
                    ),
                    Value::string_ascii_from_bytes("function-name2".into()).unwrap(),
                    Value::Principal(
                        PrincipalData::parse_qualified_contract_principal(
                            "ST000000000000000000002AMW42H.cost-function-name2"
                        )
                        .unwrap()
                    ),
                    Value::string_ascii_from_bytes("cost-function-name2".into()).unwrap(),
                ])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: true,
                data: Value::UInt(0).into()
            })
        );

        // Assert confirmation fails
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-votes",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: false,
                data: Value::Int(11).into()
            })
        );

        // Assert voting with more STX than are in an account fails
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "vote-proposal",
                &symbols_from_values(vec![Value::UInt(0), Value::UInt(USTX_PER_HOLDER + 1)]),
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: false,
                data: Value::Int(5).into()
            })
        );

        // Commit all liquid stacks to vote
        for user in USER_KEYS.iter() {
            env.execute_transaction(
                user.into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "vote-proposal",
                &symbols_from_values(vec![Value::UInt(0), Value::UInt(USTX_PER_HOLDER)]),
            )
            .unwrap()
            .0;
        }

        // Assert confirmation returns true
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-votes",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: true,
                data: Value::Bool(true).into()
            })
        );
    });

    sim.execute_next_block(|env| {
        env.execute_transaction(
            (&MINER_KEY.clone()).into(),
            None,
            COST_VOTING_CONTRACT_TESTNET.clone(),
            "veto",
            &symbols_from_values(vec![Value::UInt(0)]),
        )
        .unwrap();

        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "get-proposal-vetos",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Optional(OptionalData {
                data: Some(Box::from(Value::UInt(1)))
            })
        );
    });

    let fork_start = sim.height;

    for i in 0..25 {
        sim.execute_next_block(|env| {
            env.execute_transaction(
                (&MINER_KEY.clone()).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "veto",
                &symbols_from_values(vec![Value::UInt(0)]),
            )
            .unwrap();

            // assert error if already vetoed in this block
            assert_eq!(
                env.execute_transaction(
                    (&MINER_KEY.clone()).into(),
                    None,
                    COST_VOTING_CONTRACT_TESTNET.clone(),
                    "veto",
                    &symbols_from_values(vec![Value::UInt(0)])
                )
                .unwrap()
                .0,
                Value::Response(ResponseData {
                    committed: false,
                    data: Value::Int(9).into()
                })
            );
        })
    }

    for _ in 0..100 {
        sim.execute_next_block(|_| {});
    }

    sim.execute_next_block(|env| {
        // Assert confirmation fails because of majority veto
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-miners",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: false,
                data: Value::Int(14).into()
            })
        );
    });

    // let's fork, and overcome the veto
    sim.execute_block_as_fork(fork_start, |_| {});
    for _ in 0..125 {
        sim.execute_next_block(|_| {});
    }

    sim.execute_next_block(|env| {
        // Assert confirmation passes because there are no vetos
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-miners",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: true,
                data: Value::Bool(true).into(),
            })
        );
    });
}

#[test]
fn test_vote_confirm() {
    let mut sim = ClarityTestSim::new();

    sim.execute_next_block(|env| {
        env.initialize_contract(
            COST_VOTING_CONTRACT_TESTNET.clone(),
            &BOOT_CODE_COST_VOTING,
            None,
            ASTRules::PrecheckSize,
        )
        .unwrap();

        // Submit a proposal
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "submit-proposal",
                &symbols_from_values(vec![
                    Value::Principal(
                        PrincipalData::parse_qualified_contract_principal(
                            "ST000000000000000000002AMW42H.function-name2"
                        )
                        .unwrap()
                    ),
                    Value::string_ascii_from_bytes("function-name2".into()).unwrap(),
                    Value::Principal(
                        PrincipalData::parse_qualified_contract_principal(
                            "ST000000000000000000002AMW42H.cost-function-name2"
                        )
                        .unwrap()
                    ),
                    Value::string_ascii_from_bytes("cost-function-name2".into()).unwrap(),
                ])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: true,
                data: Value::UInt(0).into()
            })
        );

        // Assert confirmation fails
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-votes",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: false,
                data: Value::Int(11).into()
            })
        );

        // Commit all liquid stacks to vote
        for user in USER_KEYS.iter() {
            env.execute_transaction(
                user.into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "vote-proposal",
                &symbols_from_values(vec![Value::UInt(0), Value::UInt(USTX_PER_HOLDER)]),
            )
            .unwrap()
            .0;
        }

        // Assert confirmation returns true
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-votes",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: true,
                data: Value::Bool(true).into()
            })
        );
    });

    // Fast forward to proposal expiration
    for _ in 0..2016 {
        sim.execute_next_block(|_| {});
    }

    for _ in 0..1007 {
        sim.execute_next_block(|_| {});
    }

    sim.execute_next_block(|env| {
        // Assert confirmation passes
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-miners",
                &symbols_from_values(vec![Value::UInt(0)])
            )
            .unwrap()
            .0,
            Value::Response(ResponseData {
                committed: true,
                data: Value::Bool(true).into()
            })
        );
    });
}

#[test]
fn test_vote_too_many_confirms() {
    let mut sim = ClarityTestSim::new();

    let MAX_CONFIRMATIONS_PER_BLOCK = 10;
    sim.execute_next_block(|env| {
        env.initialize_contract(
            COST_VOTING_CONTRACT_TESTNET.clone(),
            &BOOT_CODE_COST_VOTING,
            None,
            ASTRules::PrecheckSize,
        )
        .unwrap();

        // Submit a proposal
        for i in 0..(MAX_CONFIRMATIONS_PER_BLOCK + 1) {
            assert_eq!(
                env.execute_transaction(
                    (&USER_KEYS[0]).into(),
                    None,
                    COST_VOTING_CONTRACT_TESTNET.clone(),
                    "submit-proposal",
                    &symbols_from_values(vec![
                        Value::Principal(
                            PrincipalData::parse_qualified_contract_principal(
                                "ST000000000000000000002AMW42H.function-name2"
                            )
                            .unwrap()
                        ),
                        Value::string_ascii_from_bytes("function-name2".into()).unwrap(),
                        Value::Principal(
                            PrincipalData::parse_qualified_contract_principal(
                                "ST000000000000000000002AMW42H.cost-function-name2"
                            )
                            .unwrap()
                        ),
                        Value::string_ascii_from_bytes("cost-function-name2".into()).unwrap(),
                    ])
                )
                .unwrap()
                .0,
                Value::Response(ResponseData {
                    committed: true,
                    data: Value::UInt(i as u128).into()
                })
            );
        }

        for i in 0..(MAX_CONFIRMATIONS_PER_BLOCK + 1) {
            // Commit all liquid stacks to vote
            for user in USER_KEYS.iter() {
                assert_eq!(
                    env.execute_transaction(
                        user.into(),
                        None,
                        COST_VOTING_CONTRACT_TESTNET.clone(),
                        "vote-proposal",
                        &symbols_from_values(vec![
                            Value::UInt(i as u128),
                            Value::UInt(USTX_PER_HOLDER)
                        ]),
                    )
                    .unwrap()
                    .0,
                    Value::okay_true()
                );
            }

            // Assert confirmation returns true
            assert_eq!(
                env.execute_transaction(
                    (&USER_KEYS[0]).into(),
                    None,
                    COST_VOTING_CONTRACT_TESTNET.clone(),
                    "confirm-votes",
                    &symbols_from_values(vec![Value::UInt(i as u128)])
                )
                .unwrap()
                .0,
                Value::okay_true(),
            );

            // withdraw
            for user in USER_KEYS.iter() {
                env.execute_transaction(
                    user.into(),
                    None,
                    COST_VOTING_CONTRACT_TESTNET.clone(),
                    "withdraw-votes",
                    &symbols_from_values(vec![
                        Value::UInt(i as u128),
                        Value::UInt(USTX_PER_HOLDER),
                    ]),
                )
                .unwrap()
                .0;
            }
        }
    });

    // Fast forward to proposal expiration
    for _ in 0..2016 {
        sim.execute_next_block(|_| {});
    }

    for _ in 0..1007 {
        sim.execute_next_block(|_| {});
    }

    sim.execute_next_block(|env| {
        for i in 0..MAX_CONFIRMATIONS_PER_BLOCK {
            // Assert confirmation passes
            assert_eq!(
                env.execute_transaction(
                    (&USER_KEYS[0]).into(),
                    None,
                    COST_VOTING_CONTRACT_TESTNET.clone(),
                    "confirm-miners",
                    &symbols_from_values(vec![Value::UInt(i as u128)])
                )
                .unwrap()
                .0,
                Value::okay_true(),
            );
        }

        // Assert next confirmation fails
        assert_eq!(
            env.execute_transaction(
                (&USER_KEYS[0]).into(),
                None,
                COST_VOTING_CONTRACT_TESTNET.clone(),
                "confirm-miners",
                &symbols_from_values(vec![Value::UInt(MAX_CONFIRMATIONS_PER_BLOCK)])
            )
            .unwrap()
            .0,
            Value::error(Value::Int(17)).unwrap()
        );
    });
}
