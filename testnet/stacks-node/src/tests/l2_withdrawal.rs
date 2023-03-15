use std;
use std::thread;

use crate::config::{EventKeyType, EventObserverConfig};
use crate::neon;
use crate::tests::l1_multiparty::MOCKNET_EPOCH_2_1;
use crate::tests::neon_integrations::{get_account, submit_tx, test_observer};
use crate::tests::{make_contract_call, make_contract_publish, to_addr};
use clarity::boot_util::boot_code_addr;
use clarity::vm::database::ClaritySerializable;
use clarity::vm::representations::ContractName;
use clarity::vm::types::{PrincipalData, TypeSignature};
use clarity::vm::Value;
use stacks::burnchains::Burnchain;
use stacks::core::LAYER_1_CHAIN_ID_TESTNET;
use stacks::vm::types::QualifiedContractIdentifier;
use std::env;
use std::sync::atomic::Ordering;

use std::time::Duration;

use crate::tests::l1_observer_test::{
    call_read_only, publish_subnet_contracts_to_l1, wait_for_next_stacks_block,
    wait_for_target_l1_block, StacksL1Controller,
};
use crate::tests::l1_observer_test::{MOCKNET_PRIVATE_KEY_1, MOCKNET_PRIVATE_KEY_2};

/// This integration test verifies that:
/// (a) assets minted on L1 chain can be deposited into subnet
/// (b) assets minted on subnet can be withdrawn to the L1
#[test]
fn withdraw_unregistered_asset() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace nft_deposit_and_withdraw_integration_test
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";
    let l1_rpc_origin = "http://127.0.0.1:20443";

    // Start the L2 run loop.
    let mut config = super::new_test_conf();
    config.node.mining_key = Some(MOCKNET_PRIVATE_KEY_2.clone());
    let miner_account = to_addr(&MOCKNET_PRIVATE_KEY_2);
    let user_addr = to_addr(&MOCKNET_PRIVATE_KEY_1);
    config.add_initial_balance(user_addr.to_string(), 10000000);
    config.add_initial_balance(miner_account.to_string(), 10000000);

    config.burnchain.first_burn_header_height = 1;
    config.burnchain.chain = "stacks_layer_1".to_string();
    config.burnchain.rpc_ssl = false;
    config.burnchain.rpc_port = 20443;
    config.burnchain.peer_host = "127.0.0.1".into();
    config.node.wait_time_for_microblocks = 10_000;
    config.node.rpc_bind = "127.0.0.1:30443".into();
    config.node.p2p_bind = "127.0.0.1:30444".into();
    let l2_rpc_origin = format!("http://{}", &config.node.rpc_bind);
    let mut l2_nonce = 0;

    config.burnchain.contract_identifier =
        QualifiedContractIdentifier::new(user_addr.into(), "subnet-controller".into());

    config.node.miner = true;

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

    // The burnchain should have registered what the listener recorded.
    let burnchain = Burnchain::new(&config.get_burn_db_path(), &config.burnchain.chain).unwrap();
    let (sortition_db, burndb) = burnchain.open_db(true).unwrap();

    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), true);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");
    let mut l1_nonce = 0;

    // Sleep to give the L1 chain time to start
    thread::sleep(Duration::from_millis(10_000));
    wait_for_target_l1_block(&sortition_db, MOCKNET_EPOCH_2_1);

    l1_nonce = publish_subnet_contracts_to_l1(
        l1_nonce,
        &config,
        miner_account.clone().into(),
        user_addr.clone().into(),
    );

    // Publish a simple NFT onto L1
    let nft_content = include_str!("../../../../core-contracts/contracts/helper/simple-nft.clar");
    let nft_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        "simple-nft",
        &nft_content,
    );

    submit_tx(l1_rpc_origin, &nft_publish);

    // Sleep to give the run loop time to listen to blocks,
    //  and start mining L2 blocks
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    let tip = burndb
        .get_canonical_chain_tip()
        .expect("couldn't get chain tip");

    // Ensure that the tip height has moved beyond height 0.
    // We check that we have moved past 3 just to establish we are reliably getting blocks.
    assert!(tip.block_height > 3);

    // test the miner's nonce has incremented: this shows that L2 blocks have
    //  been mined (because the coinbase transactions bump the miner's nonce)
    let account = get_account(&l2_rpc_origin, &miner_account);
    assert!(
        account.nonce >= 2,
        "Miner should have produced at least 2 coinbase transactions"
    );

    // Publish subnet contract for nft-token, but do not register it with the subnet!
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

    submit_tx(&l2_rpc_origin, &subnet_nft_publish);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Mint a nft-token for user on subnet (ID = 5)
    let l2_mint_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        &user_addr,
        "simple-nft",
        "gift-nft",
        &[Value::Principal(user_addr.into()), Value::UInt(5)],
    );
    l2_nonce += 1;

    submit_tx(&l2_rpc_origin, &l2_mint_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Try to withdraw the subnet native nft from the L2 (with `nft-withdraw?`)
    // This should fail, because the contract is not registered with the subnet.
    let l2_withdraw_native_nft_tx = make_contract_call(
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
            Value::UInt(5),
            Value::Principal(user_addr.into()),
        ],
    );

    // Submit withdrawal function calls
    submit_tx(&l2_rpc_origin, &l2_withdraw_native_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that user still owns the subnet native NFT on L2 chain.
    let res = call_read_only(
        &l2_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-token-owner",
        vec![Value::UInt(5).serialize()],
    );
    assert!(res.get("cause").is_none());
    assert!(res["okay"].as_bool().unwrap());
    let result = res["result"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap()
        .to_string();
    let owner = Value::deserialize(
        &result,
        &TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
    );
    assert_eq!(
        owner,
        Value::some(Value::Principal(user_addr.into())).unwrap()
    );

    // Check that the user does not own the subnet-native NFT on the L1
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
    let owner = Value::deserialize(
        &result,
        &TypeSignature::ResponseType(Box::new((
            TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
            TypeSignature::UIntType,
        ))),
    );
    assert_eq!(owner, Value::okay(Value::none()).unwrap());

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}
