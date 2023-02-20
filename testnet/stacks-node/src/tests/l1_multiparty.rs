use std;
use std::thread;

use crate::burnchains::commitment::MultiMinerParticipant;
use crate::config::CommitStrategy;
use crate::config::{EventKeyType, EventObserverConfig};
use crate::tests::l1_observer_test::{call_read_only, wait_for_target_l1_block};
use crate::tests::neon_integrations::{
    filter_map_events, get_account, get_nft_withdrawal_entry, test_observer,
};
use crate::tests::{make_contract_call, make_contract_publish};
use crate::Config;

use crate::neon;
use crate::tests::l1_observer_test::{
    publish_subnet_contracts_to_l1, wait_for_next_stacks_block, StacksL1Controller,
    MOCKNET_PRIVATE_KEY_1, MOCKNET_PRIVATE_KEY_2, MOCKNET_PRIVATE_KEY_3,
};
use crate::tests::neon_integrations::submit_tx;
use crate::tests::to_addr;
use stacks::core::LAYER_1_CHAIN_ID_TESTNET;

use stacks::burnchains::Burnchain;

use clarity::boot_util::{boot_code_addr, boot_code_id};
use clarity::util::hash::{MerklePathOrder, MerkleTree, Sha512Trunc256Sum};
use clarity::vm::database::ClaritySerializable;
use clarity::vm::events::{SmartContractEventData, StacksTransactionEvent};
use clarity::vm::representations::ContractName;
use clarity::vm::types::{PrincipalData, TupleData, TypeSignature};
use clarity::vm::Value;
use stacks::chainstate::stacks::events::{StacksTransactionReceipt, TransactionOrigin};
use stacks::chainstate::stacks::{
    CoinbasePayload, StacksTransaction, TransactionAuth, TransactionPayload,
    TransactionSpendingCondition, TransactionVersion,
};
use stacks::clarity::types::chainstate::StacksPublicKey;
use stacks::clarity_vm::withdrawal::{
    convert_withdrawal_key_to_bytes, create_withdrawal_merkle_tree, generate_key_from_event,
};
use stacks::util::secp256k1::Secp256k1PublicKey;
use stacks::vm::costs::ExecutionCost;
use stacks::vm::types::QualifiedContractIdentifier;
use stacks::vm::types::StandardPrincipalData;
use stacks::vm::ClarityName;

use std::env;
use std::sync::atomic::Ordering;

use std::time::Duration;

/// This is the height to wait for the L1 mocknet node to reach the 2.1 epoch
pub const MOCKNET_EPOCH_2_1: u64 = 4;

/// Uses MOCKNET_PRIVATE_KEY_1 to publish the multi-miner contract
pub fn publish_multiparty_contract_to_l1(
    mut l1_nonce: u64,
    config: &Config,
    miners: &[PrincipalData],
) -> u64 {
    let (required_signers, contract) = match &config.burnchain.commit_strategy {
        CommitStrategy::MultiMiner {
            required_signers,
            contract,
            ..
        } => (*required_signers, contract.clone()),
        _ => panic!("Expected to be configured to use multi-party mining contract"),
    };

    let miners_str: Vec<_> = miners.iter().map(|x| format!("'{}", x)).collect();
    let miners_list_str = format!("(list {})", miners_str.join(" "));

    // Publish the multi-miner control contract on the L1 chain
    let contract_content = include_str!("../../../../core-contracts/contracts/multi-miner.clar")
        .replace(
            "(define-constant signers-required u2)",
            &format!("(define-constant signers-required u{})", required_signers),
        )
        .replace(
            "(define-data-var miners (optional (list 10 principal)) none)",
            &format!(
                "(define-data-var miners (optional (list 10 principal)) (some {}))",
                miners_list_str
            ),
        ).replace(
            "(use-trait nft-trait 'SP2PABAF9FTAJYNFZH93XENAJ8FVY99RRM50D2JG9.nft-trait.nft-trait)",
            "(use-trait nft-trait .sip-traits.nft-trait)"
        ).replace(
            "(use-trait ft-trait 'SP3FBR2AGK5H9QBDH3EEN6DF8EK8JY7RX8QJ5SVTE.sip-010-trait-ft-standard.sip-010-trait)",
            "(use-trait ft-trait .sip-traits.ft-trait)"
        );
    let l1_rpc_origin = config.burnchain.get_rpc_url();

    assert_eq!(
        &StandardPrincipalData::from(to_addr(&MOCKNET_PRIVATE_KEY_1)),
        &contract.issuer,
        "Incorrectly configured mining contract: issuer should be MOCKNET_PRIVATE_KEY_1"
    );

    let miner_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &contract.name.to_string(),
        &contract_content,
    );
    l1_nonce += 1;

    submit_tx(&l1_rpc_origin, &miner_publish);

    println!("Submitted multi-party contract!");

    l1_nonce
}

#[test]
fn l1_multiparty_1_of_n_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace l1_integration_test
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";

    // Start the L2 run loop.
    let mut config = super::new_l1_test_conf(&*MOCKNET_PRIVATE_KEY_2, &*MOCKNET_PRIVATE_KEY_1);
    let miner_account = to_addr(&MOCKNET_PRIVATE_KEY_2);
    let l2_rpc_origin = format!("http://{}", &config.node.rpc_bind);

    let multi_party_contract = QualifiedContractIdentifier::new(
        to_addr(&MOCKNET_PRIVATE_KEY_1).into(),
        "subnet-multiparty-miner".into(),
    );

    config.burnchain.commit_strategy = CommitStrategy::MultiMiner {
        required_signers: 1,
        contract: multi_party_contract.clone(),
        other_participants: vec![],
        leader: true,
    };

    let mut run_loop = neon::RunLoop::new(config.clone());
    let termination_switch = run_loop.get_termination_switch();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Give the run loop time to start.
    thread::sleep(Duration::from_millis(2_000));

    let burnchain = Burnchain::new(&config.get_burn_db_path(), &config.burnchain.chain).unwrap();
    let (sortition_db, burndb) = burnchain.open_db(true).unwrap();

    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), true);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");

    // Sleep to give the L1 chain time to start
    thread::sleep(Duration::from_millis(10_000));

    wait_for_target_l1_block(&sortition_db, MOCKNET_EPOCH_2_1);

    let l1_nonce = publish_subnet_contracts_to_l1(
        0,
        &config,
        multi_party_contract.clone().into(),
        multi_party_contract.clone().into(),
    );
    publish_multiparty_contract_to_l1(l1_nonce, &config, &[miner_account.clone().into()]);

    // Wait for exactly two stacks blocks.
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // The burnchain should have registered what the listener recorded.
    let tip = burndb
        .get_canonical_chain_tip()
        .expect("couldn't get chain tip");
    info!("burnblock chain tip is {:?}", &tip);

    // Ensure that the tip height has moved beyond height 0.
    // We check that we have moved past 3 just to establish we are reliably getting blocks.
    assert!(tip.block_height > 3);

    eprintln!("Miner account: {}", miner_account);

    // test the miner's nonce has incremented: this shows that L2 blocks have
    //  been mined (because the coinbase transactions bump the miner's nonce)
    let account = get_account(&l2_rpc_origin, &miner_account);
    assert_eq!(account.balance, 0);
    assert!(
        account.nonce >= 2,
        "Miner should have produced at least 2 coinbase transactions"
    );

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}

#[test]
// Test that a 2-of-2 multiparty mining setup can make
//  simple progress.
fn l1_multiparty_2_of_2_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace l1_integration_test
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet-double.toml";

    // Start the L2 run loop.
    let mut leader_config =
        super::new_l1_test_conf(&*MOCKNET_PRIVATE_KEY_2, &*MOCKNET_PRIVATE_KEY_1);
    let miner_account = to_addr(&MOCKNET_PRIVATE_KEY_2);
    let l2_rpc_origin = format!("http://{}", &leader_config.node.rpc_bind);

    let multi_party_contract = QualifiedContractIdentifier::new(
        to_addr(&MOCKNET_PRIVATE_KEY_1).into(),
        "subnet-multiparty-miner".into(),
    );

    let mut follower_config =
        super::new_l1_test_conf(&*MOCKNET_PRIVATE_KEY_3, &*MOCKNET_PRIVATE_KEY_1);
    follower_config.node.chain_id = leader_config.node.chain_id;

    let follower_account = to_addr(&MOCKNET_PRIVATE_KEY_3);
    follower_config.connection_options.subnet_validator = follower_config.node.mining_key.clone();
    follower_config.node.rpc_bind = "127.0.0.1:30643".into();
    follower_config.node.data_url = "http://127.0.0.1:30643".into();
    follower_config.node.p2p_bind = "127.0.0.1:30644".into();
    follower_config.burnchain.observer_port = 52303;
    follower_config.events_observers = vec![];
    follower_config.node.miner = false;
    follower_config.node.local_peer_seed = vec![20; 32];

    follower_config.burnchain.commit_strategy = CommitStrategy::MultiMiner {
        required_signers: 2,
        contract: multi_party_contract.clone(),
        other_participants: vec![MultiMinerParticipant {
            rpc_server: l2_rpc_origin.clone(),
            public_key: [0; 33],
        }],
        leader: false,
    };

    follower_config.connection_options.subnet_signing_contract = Some(multi_party_contract.clone());
    follower_config.connection_options.allowed_block_proposers =
        vec![Secp256k1PublicKey::from_private(&MOCKNET_PRIVATE_KEY_2)];

    follower_config.add_bootstrap_node(
        "024d4b6cd1361032ca9bd2aeb9d900aa4d45d9ead80ac9423374c451a7254d0766@127.0.0.1:30444",
    );

    let follower_rpc_origin = format!("http://{}", &follower_config.node.rpc_bind);

    leader_config.burnchain.commit_strategy = CommitStrategy::MultiMiner {
        required_signers: 2,
        contract: multi_party_contract.clone(),
        other_participants: vec![MultiMinerParticipant {
            rpc_server: follower_rpc_origin.clone(),
            public_key: [0; 33],
        }],
        leader: true,
    };

    let mut leader_run_loop = neon::RunLoop::new(leader_config.clone());
    let leader_termination_switch = leader_run_loop.get_termination_switch();
    let leader_run_loop_thread = thread::spawn(move || leader_run_loop.start(None, 0));

    let mut follower_run_loop = neon::RunLoop::new(follower_config.clone());
    let follower_termination_switch = follower_run_loop.get_termination_switch();
    let follower_run_loop_thread = thread::spawn(move || follower_run_loop.start(None, 0));

    // Give the run loop time to start.
    thread::sleep(Duration::from_millis(2_000));

    let burnchain = Burnchain::new(
        &leader_config.get_burn_db_path(),
        &leader_config.burnchain.chain,
    )
    .unwrap();
    let (sortition_db, burndb) = burnchain.open_db(true).unwrap();

    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), false);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");

    // Sleep to give the L1 chain time to start
    thread::sleep(Duration::from_millis(10_000));
    wait_for_target_l1_block(&sortition_db, MOCKNET_EPOCH_2_1);

    let l1_nonce = publish_subnet_contracts_to_l1(
        0,
        &leader_config,
        multi_party_contract.clone().into(),
        multi_party_contract.clone().into(),
    );
    publish_multiparty_contract_to_l1(
        l1_nonce,
        &leader_config,
        &[
            miner_account.clone().into(),
            follower_account.clone().into(),
        ],
    );

    // Wait for exactly two stacks blocks.
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // The burnchain should have registered what the listener recorded.
    let tip = burndb
        .get_canonical_chain_tip()
        .expect("couldn't get chain tip");
    info!("burnblock chain tip is {:?}", &tip);

    // Ensure that the tip height has moved beyond height 0.
    // We check that we have moved past 3 just to establish we are reliably getting blocks.
    assert!(tip.block_height > 3);

    eprintln!("Miner account: {}", miner_account);

    // test the miner's nonce has incremented: this shows that L2 blocks have
    //  been mined (because the coinbase transactions bump the miner's nonce)
    let account = get_account(&l2_rpc_origin, &miner_account);
    assert_eq!(account.balance, 0);
    assert!(
        account.nonce >= 2,
        "Miner should have produced at least 2 coinbase transactions"
    );

    leader_termination_switch.store(false, Ordering::SeqCst);
    follower_termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    leader_run_loop_thread
        .join()
        .expect("Failed to join run loop.");
    follower_run_loop_thread
        .join()
        .expect("Failed to join run loop.");
}

#[test]
fn l1_multiparty_1_of_n_deposit_and_withdraw_asset_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace l1_multiparty_1_of_n_deposit_and_withdraw_asset_integration_test
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";
    let l1_rpc_origin = "http://127.0.0.1:20443";

    // Start the L2 run loop.
    let mut config = super::new_l1_test_conf(&*MOCKNET_PRIVATE_KEY_2, &*MOCKNET_PRIVATE_KEY_1);
    let miner_account = to_addr(&MOCKNET_PRIVATE_KEY_2);
    let user_addr = to_addr(&MOCKNET_PRIVATE_KEY_1);
    config.add_initial_balance(user_addr.to_string(), 10000000);
    config.add_initial_balance(miner_account.to_string(), 10000000);

    let l2_rpc_origin = format!("http://{}", &config.node.rpc_bind);
    let mut l2_nonce = 0;

    let multi_party_contract = QualifiedContractIdentifier::new(
        to_addr(&MOCKNET_PRIVATE_KEY_1).into(),
        "subnet-multiparty-miner".into(),
    );

    config.burnchain.commit_strategy = CommitStrategy::MultiMiner {
        required_signers: 1,
        contract: multi_party_contract.clone(),
        other_participants: vec![],
        leader: true,
    };

    config.events_observers.push(EventObserverConfig {
        endpoint: format!("localhost:{}", test_observer::EVENT_OBSERVER_PORT),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    test_observer::spawn();

    let mut run_loop = neon::RunLoop::new(config.clone());
    let termination_switch = run_loop.get_termination_switch();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Give the run loop time to start.
    thread::sleep(Duration::from_millis(2_000));

    let burnchain = Burnchain::new(&config.get_burn_db_path(), &config.burnchain.chain).unwrap();
    let (sortition_db, burndb) = burnchain.open_db(true).unwrap();

    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), true);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");

    // Sleep to give the L1 chain time to start
    thread::sleep(Duration::from_millis(10_000));

    wait_for_target_l1_block(&sortition_db, MOCKNET_EPOCH_2_1);

    let mut l1_nonce = publish_subnet_contracts_to_l1(
        0,
        &config,
        multi_party_contract.clone().into(),
        multi_party_contract.clone().into(),
    );
    l1_nonce = publish_multiparty_contract_to_l1(l1_nonce, &config, &[miner_account.clone().into()]);

    // Wait for exactly two stacks blocks.
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // The burnchain should have registered what the listener recorded.
    let tip = burndb
        .get_canonical_chain_tip()
        .expect("couldn't get chain tip");
    info!("burnblock chain tip is {:?}", &tip);

    // Ensure that the tip height has moved beyond height 0.
    // We check that we have moved past 3 just to establish we are reliably getting blocks.
    assert!(tip.block_height > 3);

    // Publish a simple FT and NFT
    let ft_content = include_str!("../../../../core-contracts/contracts/helper/simple-ft.clar");
    let ft_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        "simple-ft",
        &ft_content,
    );
    l1_nonce += 1;
    let ft_contract_name = ContractName::from("simple-ft");
    let ft_contract_id = QualifiedContractIdentifier::new(user_addr.into(), ft_contract_name);

    let nft_content = include_str!("../../../../core-contracts/contracts/helper/simple-nft.clar");
    let nft_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        "simple-nft",
        &nft_content,
    );
    l1_nonce += 1;
    let nft_contract_name = ContractName::from("simple-nft");
    let nft_contract_id = QualifiedContractIdentifier::new(user_addr.into(), nft_contract_name);

    submit_tx(&l1_rpc_origin, &ft_publish);
    submit_tx(&l1_rpc_origin, &nft_publish);

    println!("Submitted FT, NFT, and Subnet contracts!");

    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Publish subnet contract for ft-token
    let subnet_ft_content =
        include_str!("../../../../core-contracts/contracts/helper/simple-ft-l2.clar");
    let subnet_ft_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        "simple-ft",
        subnet_ft_content,
    );
    l2_nonce += 1;
    let subnet_ft_contract_id =
        QualifiedContractIdentifier::new(user_addr.into(), ContractName::from("simple-ft"));

    // Publish subnet contract for nft-token
    let subnet_nft_content =
        include_str!("../../../../core-contracts/contracts/helper/simple-nft-l2.clar");
    let subnet_nft_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        "simple-nft",
        subnet_nft_content,
    );
    l2_nonce += 1;
    let subnet_nft_contract_id =
        QualifiedContractIdentifier::new(user_addr.into(), ContractName::from("simple-nft"));

    // Mint a ft-token for user on L1 chain (amount = 1)
    let l1_mint_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        "simple-ft",
        "gift-tokens",
        &[Value::Principal(user_addr.into())],
    );
    l1_nonce += 1;
    // Mint a nft-token for user on L1 chain (ID = 1)
    let l1_mint_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        "simple-nft",
        "test-mint",
        &[Value::Principal(user_addr.into())],
    );
    l1_nonce += 1;

    submit_tx(&l2_rpc_origin, &subnet_ft_publish);
    submit_tx(&l2_rpc_origin, &subnet_nft_publish);
    submit_tx(&l1_rpc_origin, &l1_mint_ft_tx);
    submit_tx(&l1_rpc_origin, &l1_mint_nft_tx);

    // Register the contract (submitted by miner)
    let account = get_account(&l1_rpc_origin, &miner_account);
    let subnet_setup_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_2,
        LAYER_1_CHAIN_ID_TESTNET,
        account.nonce,
        1_000_000,
        &multi_party_contract.issuer.clone().into(),
        multi_party_contract.name.as_str(),
        "register-new-ft-contract",
        &[
            Value::Principal(PrincipalData::Contract(ft_contract_id.clone())),
            Value::Principal(PrincipalData::Contract(subnet_ft_contract_id.clone())),
            Value::UInt(1),
            Value::list_from(vec![]).unwrap(),
        ],
    );
    submit_tx(&l1_rpc_origin, &subnet_setup_ft_tx);

    let subnet_setup_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_2,
        LAYER_1_CHAIN_ID_TESTNET,
        account.nonce + 1,
        1_000_000,
        &multi_party_contract.issuer.clone().into(),
        multi_party_contract.name.as_str(),
        "register-new-nft-contract",
        &[
            Value::Principal(PrincipalData::Contract(nft_contract_id.clone())),
            Value::Principal(PrincipalData::Contract(subnet_nft_contract_id.clone())),
            Value::UInt(1),
            Value::list_from(vec![]).unwrap(),
        ],
    );
    submit_tx(&l1_rpc_origin, &subnet_setup_nft_tx);

    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user does not own any of the fungible tokens on the subnet now
    let res = call_read_only(
        &l2_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-token-balance",
        vec![Value::Principal(user_addr.into()).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    assert_eq!(res["result"], "0x0100000000000000000000000000000000");

    // Check that the user does not own the NFT on the subnet now
    let res = call_read_only(
        &l2_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-token-owner",
        vec![Value::UInt(1).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let addr = Value::deserialize(
        &result,
        &TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
    );
    assert_eq!(addr, Value::none());

    let l1_deposit_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "deposit-ft-asset",
        &[
            Value::Principal(PrincipalData::Contract(ft_contract_id.clone())),
            Value::UInt(1),
            Value::Principal(user_addr.into()),
            Value::none(),
        ],
    );
    l1_nonce += 1;
    let l1_deposit_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "deposit-nft-asset",
        &[
            Value::Principal(PrincipalData::Contract(nft_contract_id.clone())),
            Value::UInt(1),
            Value::Principal(user_addr.into()),
        ],
    );
    l1_nonce += 1;

    // deposit ft-token into subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_deposit_ft_tx);
    // deposit nft-token into subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_deposit_nft_tx);

    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user owns a fungible token on the subnet now
    let res = call_read_only(
        &l2_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-token-balance",
        vec![Value::Principal(user_addr.into()).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(&result, &TypeSignature::UIntType);
    assert_eq!(amount, Value::UInt(1));
    // Check that the user owns the NFT on the subnet now
    let res = call_read_only(
        &l2_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-token-owner",
        vec![Value::UInt(1).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let addr = Value::deserialize(
        &result,
        &TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
    );
    assert_eq!(
        addr,
        Value::some(Value::Principal(user_addr.into())).unwrap()
    );

    // Check that the user does not own the FT on the L1
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-balance",
        vec![Value::Principal(user_addr.into()).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((TypeSignature::UIntType, TypeSignature::UIntType))),
    );
    assert_eq!(amount, Value::okay(Value::UInt(0)).unwrap());
    // Check that the user does not own the NFT on the L1 (the contract should own it)
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-owner",
        vec![Value::UInt(1).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((
            TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
            TypeSignature::UIntType,
        ))),
    );
    assert_ne!(
        amount,
        Value::some(Value::Principal(user_addr.into())).unwrap()
    );

    // Withdraw the ft on the L2
    let l2_withdraw_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        &boot_code_addr(false),
        "subnet",
        "ft-withdraw?",
        &[
            Value::Principal(PrincipalData::Contract(QualifiedContractIdentifier::new(
                user_addr.into(),
                ContractName::from("simple-ft"),
            ))),
            Value::UInt(1),
            Value::Principal(user_addr.into()),
        ],
    );
    l2_nonce += 1;
    // Withdraw the nft on the L2
    let l2_withdraw_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        &boot_code_addr(false),
        "subnet",
        "nft-withdraw?",
        &[
            Value::Principal(PrincipalData::Contract(QualifiedContractIdentifier::new(
                user_addr.into(),
                ContractName::from("simple-nft"),
            ))),
            Value::UInt(1),
            Value::Principal(user_addr.into()),
        ],
    );
    l2_nonce += 1;
    // Withdraw ft-token from subnet contract on L2
    submit_tx(&l2_rpc_origin, &l2_withdraw_ft_tx);
    // Withdraw nft-token from subnet contract on L2
    submit_tx(&l2_rpc_origin, &l2_withdraw_nft_tx);

    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that user no longer owns the fungible token on L2 chain.
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-balance",
        vec![Value::Principal(user_addr.into()).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((TypeSignature::UIntType, TypeSignature::UIntType))),
    );
    assert_eq!(amount, Value::okay(Value::UInt(0)).unwrap());
    // Check that user no longer owns the nft on L2 chain.
    let res = call_read_only(
        &l2_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-token-owner",
        vec![Value::UInt(1).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let addr = Value::deserialize(
        &result,
        &TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
    );
    assert_eq!(addr, Value::none(),);
    // Check that the user does not *yet* own the FT on the L1
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-balance",
        vec![Value::Principal(user_addr.into()).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((TypeSignature::UIntType, TypeSignature::UIntType))),
    );
    assert_eq!(amount, Value::okay(Value::UInt(0)).unwrap());
    // Check that the user does not *yet* own the NFT on the L1 (the contract should own it)
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-owner",
        vec![Value::UInt(1).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((
            TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
            TypeSignature::UIntType,
        ))),
    );
    assert_ne!(
        amount,
        Value::some(Value::Principal(user_addr.into())).unwrap()
    );

    let block_data = test_observer::get_blocks();
    let mut withdraw_events = filter_map_events(&block_data, |height, event| {
        let ev_type = event.get("type").unwrap().as_str().unwrap();
        if ev_type == "contract_event" {
            let contract_event = event.get("contract_event").unwrap();
            let contract_identifier = contract_event
                .get("contract_identifier")
                .unwrap()
                .as_str()
                .unwrap();
            let topic = contract_event.get("topic").unwrap().as_str().unwrap();
            match (contract_identifier, topic) {
                ("ST000000000000000000002AMW42H.subnet", "print") => {
                    let value: Value =
                        serde_json::from_value(contract_event.get("value").unwrap().clone())
                            .unwrap();
                    let data_map = value.expect_tuple();
                    if data_map.get("type").unwrap().clone().expect_ascii() != "nft" {
                        return None;
                    }
                    Some((height, data_map.clone()))
                }
                _ => None,
            }
        } else {
            None
        }
    });
    assert_eq!(withdraw_events.len(), 1);
    let (withdrawal_height, withdrawal) = withdraw_events.pop().unwrap();

    let withdrawal_id = withdrawal
        .get("withdrawal_id")
        .unwrap()
        .clone()
        .expect_u128() as u64;

    let nft_withdrawal_entry = get_nft_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        withdrawal_id,
        QualifiedContractIdentifier::new(user_addr.into(), ContractName::from("simple-nft")),
        1,
    );

    // Create the withdrawal merkle tree by mocking the ft & nft withdraw event (if the root hash of
    // this constructed merkle tree is not identical to the root hash published by the subnet node,
    // then the test will fail).
    let mut spending_condition = TransactionSpendingCondition::new_singlesig_p2pkh(
        StacksPublicKey::from_private(&MOCKNET_PRIVATE_KEY_1),
    )
    .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(l2_nonce - 1);
    spending_condition.set_tx_fee(1000);
    let auth = TransactionAuth::Standard(spending_condition);
    let mut ft_withdraw_event =
        StacksTransactionEvent::SmartContractEvent(SmartContractEventData {
            key: (boot_code_id("subnet".into(), false), "print".into()),
            value: Value::Tuple(
                TupleData::from_data(vec![
                    (
                        "type".into(),
                        Value::string_ascii_from_bytes("ft".to_string().into_bytes()).unwrap(),
                    ),
                    (
                        "asset-contract".into(),
                        Value::Principal(PrincipalData::Contract(
                            QualifiedContractIdentifier::new(
                                user_addr.into(),
                                ContractName::from("simple-ft"),
                            ),
                        )),
                    ),
                    (
                        "sender".into(),
                        Value::Principal(PrincipalData::Standard(user_addr.into())),
                    ),
                    ("amount".into(), Value::UInt(1)),
                ])
                .expect("Failed to create tuple data."),
            ),
        });
    let mut nft_withdraw_event =
        StacksTransactionEvent::SmartContractEvent(SmartContractEventData {
            key: (boot_code_id("subnet".into(), false), "print".into()),
            value: Value::Tuple(
                TupleData::from_data(vec![
                    (
                        "type".into(),
                        Value::string_ascii_from_bytes("nft".to_string().into_bytes()).unwrap(),
                    ),
                    (
                        "asset-contract".into(),
                        Value::Principal(PrincipalData::Contract(
                            QualifiedContractIdentifier::new(
                                user_addr.into(),
                                ContractName::from("simple-nft"),
                            ),
                        )),
                    ),
                    (
                        "sender".into(),
                        Value::Principal(PrincipalData::Standard(user_addr.into())),
                    ),
                    ("id".into(), Value::UInt(1)),
                ])
                .expect("Failed to create tuple data."),
            ),
        });
    let withdrawal_receipt = StacksTransactionReceipt {
        transaction: TransactionOrigin::Stacks(StacksTransaction::new(
            TransactionVersion::Testnet,
            auth.clone(),
            TransactionPayload::Coinbase(CoinbasePayload([0u8; 32])),
        )),
        events: vec![ft_withdraw_event.clone(), nft_withdraw_event.clone()],
        post_condition_aborted: false,
        result: Value::err_none(),
        stx_burned: 0,
        contract_analysis: None,
        execution_cost: ExecutionCost::zero(),
        microblock_header: None,
        tx_index: 0,
    };
    let mut receipts = vec![withdrawal_receipt];
    let withdrawal_tree = create_withdrawal_merkle_tree(&mut receipts, withdrawal_height);
    let root_hash = withdrawal_tree.root().as_bytes().to_vec();

    let ft_withdrawal_key =
        generate_key_from_event(&mut ft_withdraw_event, 0, withdrawal_height).unwrap();
    let ft_withdrawal_key_bytes = convert_withdrawal_key_to_bytes(&ft_withdrawal_key);
    let ft_withdrawal_leaf_hash =
        MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(ft_withdrawal_key_bytes.as_slice())
            .as_bytes()
            .to_vec();
    let ft_path = withdrawal_tree.path(&ft_withdrawal_key_bytes).unwrap();

    let nft_withdrawal_key =
        generate_key_from_event(&mut nft_withdraw_event, 1, withdrawal_height).unwrap();
    let nft_withdrawal_key_bytes = convert_withdrawal_key_to_bytes(&nft_withdrawal_key);
    let nft_withdrawal_leaf_hash =
        MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(nft_withdrawal_key_bytes.as_slice())
            .as_bytes()
            .to_vec();
    let nft_path = withdrawal_tree.path(&nft_withdrawal_key_bytes).unwrap();

    let mut ft_sib_data = Vec::new();
    for sib in ft_path.iter() {
        let sib_hash = Value::buff_from(sib.hash.as_bytes().to_vec()).unwrap();
        // the sibling's side is the opposite of what PathOrder is set to
        let sib_is_left = Value::Bool(sib.order == MerklePathOrder::Right);
        let curr_sib_data = vec![
            (ClarityName::from("hash"), sib_hash),
            (ClarityName::from("is-left-side"), sib_is_left),
        ];
        let sib_tuple = Value::Tuple(TupleData::from_data(curr_sib_data).unwrap());
        ft_sib_data.push(sib_tuple);
    }
    let mut nft_sib_data = Vec::new();
    for sib in nft_path.iter() {
        let sib_hash = Value::buff_from(sib.hash.as_bytes().to_vec()).unwrap();
        // the sibling's side is the opposite of what PathOrder is set to
        let sib_is_left = Value::Bool(sib.order == MerklePathOrder::Right);
        let curr_sib_data = vec![
            (ClarityName::from("hash"), sib_hash),
            (ClarityName::from("is-left-side"), sib_is_left),
        ];
        let sib_tuple = Value::Tuple(TupleData::from_data(curr_sib_data).unwrap());
        nft_sib_data.push(sib_tuple);
    }

    let root_hash_val = Value::buff_from(root_hash.clone()).unwrap();
    let leaf_hash_val = Value::buff_from(nft_withdrawal_leaf_hash.clone()).unwrap();
    let siblings_val = Value::list_from(nft_sib_data.clone()).unwrap();

    assert_eq!(
        &root_hash_val, &nft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &leaf_hash_val, &nft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &siblings_val, &nft_withdrawal_entry.siblings,
        "Sibling hashes should match value returned via RPC"
    );

    // TODO: call withdraw from unauthorized principal once leaf verification is added to the subnet contract

    let l1_withdraw_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "withdraw-ft-asset",
        &[
            Value::Principal(PrincipalData::Contract(ft_contract_id.clone())),
            Value::UInt(1),
            Value::Principal(user_addr.into()),
            Value::UInt(0),
            Value::UInt(withdrawal_height.into()),
            Value::none(),
            Value::some(Value::Principal(PrincipalData::Contract(
                ft_contract_id.clone(),
            )))
            .unwrap(),
            Value::buff_from(root_hash.clone()).unwrap(),
            Value::buff_from(ft_withdrawal_leaf_hash).unwrap(),
            Value::list_from(ft_sib_data).unwrap(),
        ],
    );
    l1_nonce += 1;
    let l1_withdraw_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "withdraw-nft-asset",
        &[
            Value::Principal(PrincipalData::Contract(nft_contract_id.clone())),
            Value::UInt(1),
            Value::Principal(user_addr.into()),
            Value::UInt(1),
            Value::UInt(withdrawal_height.into()),
            Value::some(Value::Principal(PrincipalData::Contract(
                nft_contract_id.clone(),
            )))
            .unwrap(),
            Value::buff_from(root_hash).unwrap(),
            Value::buff_from(nft_withdrawal_leaf_hash).unwrap(),
            Value::list_from(nft_sib_data).unwrap(),
        ],
    );
    l1_nonce += 1;

    // Withdraw ft-token from subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_withdraw_ft_tx);
    // Withdraw nft-token from subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_withdraw_nft_tx);

    // Sleep to give the run loop time to mine a block
    thread::sleep(Duration::from_secs(25));

    // Check that the user owns the fungible token on the L1 chain now
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-balance",
        vec![Value::Principal(user_addr.into()).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((TypeSignature::UIntType, TypeSignature::UIntType))),
    );
    assert_eq!(amount, Value::okay(Value::UInt(1)).unwrap());
    // Check that the user owns the NFT on the L1 chain now
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-owner",
        vec![Value::UInt(1).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let amount = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((
            TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
            TypeSignature::UIntType,
        ))),
    );
    assert_eq!(
        amount,
        Value::okay(Value::some(Value::Principal(user_addr.into())).unwrap()).unwrap()
    );

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}
