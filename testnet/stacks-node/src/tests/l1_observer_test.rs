use std;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use crate::config::{EventKeyType, EventObserverConfig};
use crate::tests::l1_multiparty::MOCKNET_EPOCH_2_1;
use crate::tests::neon_integrations::{
    filter_map_events, get_account, get_ft_withdrawal_entry, get_nft_withdrawal_entry,
    get_withdrawal_entry, submit_tx, test_observer,
};
use crate::tests::{make_contract_call, make_contract_publish, to_addr};
use crate::{neon, Config};
use clarity::boot_util::{boot_code_addr, boot_code_id};
use clarity::types::chainstate::StacksAddress;
use clarity::util::hash::{MerklePathOrder, MerkleTree, Sha512Trunc256Sum};
use clarity::vm::database::ClaritySerializable;
use clarity::vm::events::SmartContractEventData;
use clarity::vm::events::StacksTransactionEvent;
use clarity::vm::representations::ContractName;
use clarity::vm::types::{PrincipalData, TypeSignature};
use clarity::vm::Value;
use stacks::burnchains::Burnchain;
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::stacks::events::{StacksTransactionReceipt, TransactionOrigin};
use stacks::chainstate::stacks::{
    CoinbasePayload, StacksPrivateKey, StacksTransaction, TransactionAuth, TransactionPayload,
    TransactionSpendingCondition, TransactionVersion,
};
use stacks::clarity::types::chainstate::StacksPublicKey;
use stacks::clarity_vm::withdrawal::{
    convert_withdrawal_key_to_bytes, create_withdrawal_merkle_tree, generate_key_from_event,
};
use stacks::codec::StacksMessageCodec;
use stacks::core::LAYER_1_CHAIN_ID_TESTNET;
use stacks::net::CallReadOnlyRequestBody;
use stacks::util::hash::hex_bytes;
use stacks::vm::costs::ExecutionCost;
use stacks::vm::types::{QualifiedContractIdentifier, TupleData};
use stacks::vm::ClarityName;
use std::convert::{TryFrom, TryInto};
use std::env;
use std::io::{BufRead, BufReader};
use std::sync::atomic::Ordering;

use std::time::{Duration, Instant};

#[derive(std::fmt::Debug)]
pub enum SubprocessError {
    SpawnFailed(String),
}

type SubprocessResult<T> = Result<T, SubprocessError>;

/// In charge of running L1 `stacks-node`.
pub struct StacksL1Controller {
    sub_process: Option<Child>,
    config_path: String,
    printer_handle: Option<JoinHandle<()>>,
    log_process: bool,
}

lazy_static! {
    pub static ref MOCKNET_PRIVATE_KEY_1: StacksPrivateKey = StacksPrivateKey::from_hex(
        "aaf57b4730f713cf942bc63f0801c4a62abe5a6ac8e3da10389f9ca3420b0dc701"
    )
    .unwrap();
    pub static ref MOCKNET_PRIVATE_KEY_2: StacksPrivateKey = StacksPrivateKey::from_hex(
        "0916e2eb04b5702e0e946081829cee67d3bb76e1792af506646843db9252ff4101"
    )
    .unwrap();
    pub static ref MOCKNET_PRIVATE_KEY_3: StacksPrivateKey = StacksPrivateKey::from_hex(
        "374b6734eaff979818c5f1367331c685459b03b1a2053310906d1408dc928a0001"
    )
    .unwrap();
}

pub fn call_read_only(
    http_origin: &str,
    addr: &StacksAddress,
    contract_name: &str,
    function_name: &str,
    args: Vec<String>,
) -> serde_json::Value {
    let client = reqwest::blocking::Client::new();

    let path = format!(
        "{}/v2/contracts/call-read/{}/{}/{}",
        &http_origin, addr, contract_name, function_name
    );
    let principal: PrincipalData = addr.clone().into();
    let body = CallReadOnlyRequestBody {
        sender: principal.to_string(),
        arguments: args,
    };

    let read_info = client
        .post(&path)
        .json(&body)
        .send()
        .unwrap()
        .json::<serde_json::Value>()
        .unwrap();

    read_info
}

impl StacksL1Controller {
    pub fn new(config_path: String, log_process: bool) -> StacksL1Controller {
        StacksL1Controller {
            sub_process: None,
            config_path,
            printer_handle: None,
            log_process,
        }
    }

    pub fn start_process(&mut self) -> SubprocessResult<()> {
        let binary = match env::var("STACKS_BASE_DIR") {
            Err(_) => {
                // assume stacks-node is in path
                "stacks-node".into()
            }
            Ok(path) => path,
        };
        let mut command = Command::new(&binary);
        command
            .stderr(Stdio::piped())
            .arg("start")
            .arg("--config=".to_owned() + &self.config_path);

        info!("stacks-node mainchain spawn: {:?}", command);

        let mut process = match command.spawn() {
            Ok(child) => child,
            Err(e) => return Err(SubprocessError::SpawnFailed(format!("{:?}", e))),
        };

        let printer_handle = if self.log_process {
            let child_out = process.stderr.take().unwrap();
            Some(thread::spawn(|| {
                let buffered_out = BufReader::new(child_out);
                for line in buffered_out.lines() {
                    let line = match line {
                        Ok(x) => x,
                        Err(_e) => return,
                    };
                    println!("L1: {}", line);
                }
            }))
        } else {
            None
        };

        info!("stacks-node mainchain spawned, waiting for startup");

        self.sub_process = Some(process);
        self.printer_handle = printer_handle;

        Ok(())
    }

    pub fn kill_process(&mut self) {
        if let Some(mut sub_process) = self.sub_process.take() {
            sub_process.kill().unwrap();
        }
        if let Some(handle) = self.printer_handle.take() {
            println!("Joining print handler: {:?}", handle.join());
        }
    }
}

impl Drop for StacksL1Controller {
    fn drop(&mut self) {
        self.kill_process();
    }
}

/// Longest time to wait for a stacks block before aborting.
const PANIC_TIMEOUT_SECS: u64 = 600;

/// Height of the current stacks tip.
fn get_stacks_tip_height(sortition_db: &SortitionDB) -> i64 {
    let tip_snapshot = SortitionDB::get_canonical_burn_chain_tip(&sortition_db.conn())
        .expect("Could not read from SortitionDB.");

    tip_snapshot.canonical_stacks_tip_height.try_into().unwrap()
}

/// Wait for the *height* of the stacks chain tip to increment.
pub fn wait_for_next_stacks_block(sortition_db: &SortitionDB) -> bool {
    let current = get_stacks_tip_height(sortition_db);
    let mut next = current;
    info!(
        "wait_for_next_stacks_block: STARTS waiting at time {:?}, Stacks block height {:?}",
        Instant::now(),
        current
    );
    let start = Instant::now();
    while next <= current {
        if start.elapsed() > Duration::from_secs(PANIC_TIMEOUT_SECS) {
            panic!("Timed out waiting for block to process, aborting test.");
        }
        thread::sleep(Duration::from_millis(100));
        next = get_stacks_tip_height(sortition_db);
    }
    info!(
        "wait_for_next_stacks_block: STOPS waiting at time {:?}, Stacks block height {}",
        Instant::now(),
        next
    );
    true
}

pub fn wait_for_target_l1_block(sortition_db: &SortitionDB, target: u64) -> bool {
    let mut next = 0;
    info!("wait_for_target_l1_block started"; "target" => target);
    let start = Instant::now();
    while next <= target {
        if start.elapsed() > Duration::from_secs(PANIC_TIMEOUT_SECS) {
            panic!("Timed out waiting for block to process, aborting test.");
        }

        thread::sleep(Duration::from_millis(100));

        let tip_snapshot = SortitionDB::get_canonical_burn_chain_tip(&sortition_db.conn())
            .expect("Could not read from SortitionDB.");

        next = tip_snapshot.block_height;
    }
    info!("wait_for_target_l1_block finished"; "target" => target);
    true
}

/// Deserializes the `StacksTransaction` objects from `blocks` and returns all those that
/// match `test_fn`.
fn select_transactions_where(
    blocks: &Vec<serde_json::Value>,
    test_fn: fn(&StacksTransaction) -> bool,
) -> Vec<StacksTransaction> {
    let mut result = vec![];
    for block in blocks {
        let transactions = block.get("transactions").unwrap().as_array().unwrap();
        for tx in transactions.iter() {
            let raw_tx = tx.get("raw_tx").unwrap().as_str().unwrap();
            let tx_bytes = hex_bytes(&raw_tx[2..]).unwrap();
            let parsed = StacksTransaction::consensus_deserialize(&mut &tx_bytes[..]).unwrap();
            let test_value = test_fn(&parsed);
            if test_value {
                result.push(parsed);
            }
        }
    }

    return result;
}

/// Uses MOCKNET_PRIVATE_KEY_1 to publish the subnet contract and supporting
///  trait contracts
pub fn publish_subnet_contracts_to_l1(
    mut l1_nonce: u64,
    config: &Config,
    miner: PrincipalData,
    admin: PrincipalData,
) -> u64 {
    // Publish the subnet traits contract
    let trait_standard_contract_name = "subnet-traits";
    let l1_rpc_origin = config.burnchain.get_rpc_url();
    // Publish the trait contract
    let trait_content =
        include_str!("../../../../core-contracts/contracts/helper/subnet-traits.clar");
    let trait_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &trait_standard_contract_name,
        &trait_content,
    );
    l1_nonce += 1;

    // Publish the SIP traits contract
    let sip_traits_contract_name = "sip-traits";
    let sip_traits_content =
        include_str!("../../../../core-contracts/contracts/helper/sip-traits.clar");
    let sip_traits_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &sip_traits_contract_name,
        &sip_traits_content,
    );
    l1_nonce += 1;

    // Publish the default subnet contract on the L1 chain
    let contract_content = include_str!("../../../../core-contracts/contracts/subnet.clar")
        .replace(
            "(define-data-var miner principal tx-sender)",
            &format!(
                "(define-data-var miner principal '{})",
                &miner
                ),
        ).replace(
            "(define-data-var admin principal tx-sender)",
            &format!(
                "(define-data-var admin principal '{})",
                &admin
            ),
        ).replace(
                "(use-trait nft-trait 'SP2PABAF9FTAJYNFZH93XENAJ8FVY99RRM50D2JG9.nft-trait.nft-trait)",
            "(use-trait nft-trait .sip-traits.nft-trait)"
        ).replace(
            "(use-trait ft-trait 'SP3FBR2AGK5H9QBDH3EEN6DF8EK8JY7RX8QJ5SVTE.sip-010-trait-ft-standard.sip-010-trait)",
            "(use-trait ft-trait .sip-traits.ft-trait)"
        );

    let subnet_contract_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        config.burnchain.contract_identifier.name.as_str(),
        &contract_content,
    );
    l1_nonce += 1;

    submit_tx(&l1_rpc_origin, &trait_publish);
    submit_tx(&l1_rpc_origin, &sip_traits_publish);
    // Because the nonce ensures that the trait contract is published
    // before the subnet contract, we can broadcast them all at once, even
    // though the subnet contract depends on that contract.
    submit_tx(&l1_rpc_origin, &subnet_contract_publish);

    println!("Submitted trait and subnet contracts!");

    l1_nonce
}

/// This test brings up the Stacks-L1 chain in "mocknet" mode, and ensures that our listener can hear and record burn blocks
/// from the Stacks-L1 chain.
#[test]
fn l1_basic_listener_test() {
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";
    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), true);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");

    // Start the L2 run loop.
    let mut config = super::new_test_conf();
    config.burnchain.first_burn_header_height = 1;
    config.burnchain.chain = "stacks_layer_1".to_string();
    config.burnchain.rpc_ssl = false;
    config.burnchain.rpc_port = 20443;
    config.burnchain.peer_host = "127.0.0.1".into();

    let mut run_loop = neon::RunLoop::new(config.clone());
    let termination_switch = run_loop.get_termination_switch();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Sleep to give the run loop time to listen to blocks.
    thread::sleep(Duration::from_millis(45000));

    // The burnchain should have registered what the listener recorded.
    let burnchain = Burnchain::new(&config.get_burn_db_path(), &config.burnchain.chain).unwrap();
    let (_sortition_db, burndb) = burnchain.open_db(true).unwrap();

    let tip = burndb
        .get_canonical_chain_tip()
        .expect("couldn't get chain tip");
    info!("burnblock chain tip is {:?}", &tip);

    // Ensure that the tip height has moved beyond height 0.
    // We check that we have moved past 3 just to establish we are reliably getting blocks.
    assert!(tip.block_height > 3);

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}

#[test]
fn l1_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace l1_integration_test
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";
    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), false);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");

    // Start the L2 run loop.
    let config = super::new_l1_test_conf(&*MOCKNET_PRIVATE_KEY_2, &*MOCKNET_PRIVATE_KEY_1);
    let miner_account = to_addr(&MOCKNET_PRIVATE_KEY_2);
    let l2_rpc_origin = format!("http://{}", &config.node.rpc_bind);

    let mut run_loop = neon::RunLoop::new(config.clone());
    let termination_switch = run_loop.get_termination_switch();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Give the run loop time to start.
    thread::sleep(Duration::from_millis(2_000));

    let burnchain = Burnchain::new(&config.get_burn_db_path(), &config.burnchain.chain).unwrap();
    let (sortition_db, burndb) = burnchain.open_db(true).unwrap();

    // Sleep to give the L1 chain time to start
    thread::sleep(Duration::from_millis(10_000));

    wait_for_target_l1_block(&sortition_db, MOCKNET_EPOCH_2_1);
    publish_subnet_contracts_to_l1(
        0,
        &config,
        miner_account.clone().into(),
        miner_account.clone().into(),
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

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}

#[test]
fn l1_deposit_and_withdraw_asset_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace l1_deposit_asset_integration_test
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

    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), true);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");
    let mut l1_nonce = 0;

    // The burnchain should have registered what the listener recorded.
    let burnchain = Burnchain::new(&config.get_burn_db_path(), &config.burnchain.chain).unwrap();
    let (sortition_db, burndb) = burnchain.open_db(true).unwrap();

    // Sleep to give the L1 chain time to start
    thread::sleep(Duration::from_millis(10_000));
    wait_for_target_l1_block(&sortition_db, MOCKNET_EPOCH_2_1);

    l1_nonce = publish_subnet_contracts_to_l1(
        l1_nonce,
        &config,
        miner_account.clone().into(),
        user_addr.clone().into(),
    );

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

    submit_tx(l1_rpc_origin, &nft_publish);
    submit_tx(l1_rpc_origin, &ft_publish);

    println!("Submitted FT, NFT, and Subnet contracts!");

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
        &[Value::UInt(1), Value::Principal(user_addr.into())],
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
    submit_tx(l1_rpc_origin, &l1_mint_ft_tx);
    submit_tx(l1_rpc_origin, &l1_mint_nft_tx);

    // Register the contract
    let subnet_setup_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "register-new-ft-contract",
        &[
            Value::Principal(PrincipalData::Contract(ft_contract_id.clone())),
            Value::Principal(PrincipalData::Contract(subnet_ft_contract_id.clone())),
        ],
    );
    l1_nonce += 1;

    let subnet_setup_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "register-new-nft-contract",
        &[
            Value::Principal(PrincipalData::Contract(nft_contract_id.clone())),
            Value::Principal(PrincipalData::Contract(subnet_nft_contract_id.clone())),
        ],
    );
    l1_nonce += 1;

    submit_tx(l1_rpc_origin, &subnet_setup_ft_tx);
    submit_tx(l1_rpc_origin, &subnet_setup_nft_tx);

    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user does not own any of the fungible tokens on the subnet now
    let res = call_read_only(
        &l2_rpc_origin,
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
    let withdraw_events = filter_map_events(&block_data, |height, event| {
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
                    match data_map
                        .get("type")
                        .unwrap()
                        .clone()
                        .expect_ascii()
                        .as_str()
                    {
                        "ft" | "nft" => Some((height, data_map.clone())),
                        _ => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        }
    });
    assert_eq!(withdraw_events.len(), 2);

    let mut ft_withdrawal_id = 0;
    let mut nft_withdrawal_id = 0;
    let mut withdrawal_height = 0;
    for (height, event) in withdraw_events {
        withdrawal_height = height;
        let withdrawal_id = event.get("withdrawal_id").unwrap().clone().expect_u128() as u32;
        match event.get("type").unwrap().clone().expect_ascii().as_str() {
            "ft" => ft_withdrawal_id = withdrawal_id,
            "nft" => nft_withdrawal_id = withdrawal_id,
            _ => panic!("Unexpected withdrawal event type"),
        }
    }

    let nft_withdrawal_entry = get_nft_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        nft_withdrawal_id,
        QualifiedContractIdentifier::new(user_addr.into(), ContractName::from("simple-nft")),
        1,
    );

    let ft_withdrawal_entry = get_ft_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        ft_withdrawal_id,
        QualifiedContractIdentifier::new(user_addr.into(), ContractName::from("simple-ft")),
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
            key: (boot_code_id("subnet", false), "print".into()),
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
            key: (boot_code_id("subnet", false), "print".into()),
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
        generate_key_from_event(&mut ft_withdraw_event, ft_withdrawal_id, withdrawal_height)
            .unwrap();
    let ft_withdrawal_key_bytes = convert_withdrawal_key_to_bytes(&ft_withdrawal_key);
    let ft_withdrawal_leaf_hash =
        MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(ft_withdrawal_key_bytes.as_slice())
            .as_bytes()
            .to_vec();
    let ft_path = withdrawal_tree.path(&ft_withdrawal_key_bytes).unwrap();

    let nft_withdrawal_key = generate_key_from_event(
        &mut nft_withdraw_event,
        nft_withdrawal_id,
        withdrawal_height,
    )
    .unwrap();
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
    let nft_leaf_hash_val = Value::buff_from(nft_withdrawal_leaf_hash.clone()).unwrap();
    let ft_leaf_hash_val = Value::buff_from(ft_withdrawal_leaf_hash.clone()).unwrap();
    let nft_siblings_val = Value::list_from(nft_sib_data.clone()).unwrap();
    let ft_siblings_val = Value::list_from(ft_sib_data.clone()).unwrap();

    assert_eq!(
        &root_hash_val, &nft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &nft_leaf_hash_val, &nft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &nft_siblings_val, &nft_withdrawal_entry.siblings,
        "Sibling hashes should match value returned via RPC"
    );

    assert_eq!(
        &root_hash_val, &ft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &ft_leaf_hash_val, &ft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &ft_siblings_val, &ft_withdrawal_entry.siblings,
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

/// This test calls the `deposit-stx` function in the subnet contract.
/// We expect to see the stx balance for the user in question increase.
#[test]
fn l1_deposit_and_withdraw_stx_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace l1_deposit_stx_integration_test
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
    let alt_user_addr = to_addr(&MOCKNET_PRIVATE_KEY_3);
    let l2_starting_account_balance = 10000000;
    let l1_starting_account_balance = 100000000000000;
    let default_fee = 1_000_000;
    config.add_initial_balance(user_addr.to_string(), l2_starting_account_balance);
    config.add_initial_balance(miner_account.to_string(), l2_starting_account_balance);
    config.add_initial_balance(alt_user_addr.to_string(), l2_starting_account_balance);

    let l2_rpc_origin = format!("http://{}", &config.node.rpc_bind);

    let mut l2_nonce = 0;

    config.events_observers.push(EventObserverConfig {
        endpoint: format!("localhost:{}", test_observer::EVENT_OBSERVER_PORT),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    test_observer::spawn();

    let mut run_loop = neon::RunLoop::new(config.clone());
    let termination_switch = run_loop.get_termination_switch();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Sleep to give the run loop time to start
    thread::sleep(Duration::from_millis(2_000));

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

    // The burnchain should have registered what the listener recorded.
    let tip = burndb
        .get_canonical_chain_tip()
        .expect("couldn't get chain tip");

    // Ensure that the tip height has moved beyond height 0.
    // We check that we have moved past 3 just to establish we are reliably getting blocks.
    assert!(tip.block_height > 3);

    // Wait a couple blocks to ensure the L2 chain has started
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Publish subnet contract for withdrawing stx
    let subnet_simple_stx = "
    (define-public (subnet-withdraw-stx (amount uint) (sender principal))
      (contract-call? 'ST000000000000000000002AMW42H.subnet stx-withdraw? amount sender)
    )
    ";
    let subnet_stx_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        default_fee,
        "simple-stx",
        subnet_simple_stx,
    );
    l2_nonce += 1;

    submit_tx(&l2_rpc_origin, &subnet_stx_publish);

    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user does not own any additional STX on the subnet now
    let account = get_account(&l2_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l2_starting_account_balance - default_fee * l2_nonce) as u128
    );

    // Check the user's balance on the L1
    let account = get_account(&l1_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l1_starting_account_balance - default_fee * l1_nonce) as u128
    );

    let l1_deposit_stx_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "deposit-stx",
        &[Value::UInt(1), Value::Principal(user_addr.into())],
    );
    l1_nonce += 1;

    // Deposit stx into subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_deposit_stx_tx);

    // Wait to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user owns additional STX on the subnet now
    let account = get_account(&l2_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l2_starting_account_balance - default_fee * l2_nonce + 1) as u128
    );
    // Check that the user's balance decreased on the L1
    let account = get_account(&l1_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l1_starting_account_balance - default_fee * l1_nonce - 1) as u128
    );

    // Call the withdraw stx function on the L2 from unauthorized user
    let l2_withdraw_stx_tx_unauth = make_contract_call(
        &MOCKNET_PRIVATE_KEY_3,
        config.node.chain_id,
        0,
        1_000_000,
        &user_addr,
        "simple-stx",
        "subnet-withdraw-stx",
        &[Value::UInt(1), Value::Principal(user_addr.into())],
    );
    // withdraw stx from L2
    submit_tx(&l2_rpc_origin, &l2_withdraw_stx_tx_unauth);

    // Wait to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user still owns STX on the subnet now (withdraw attempt should fail)
    let account = get_account(&l2_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l2_starting_account_balance - default_fee * l2_nonce + 1) as u128
    );

    // Call the withdraw stx function on the L2 from the correct user
    let l2_withdraw_stx_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        &user_addr,
        "simple-stx",
        "subnet-withdraw-stx",
        &[Value::UInt(1), Value::Principal(user_addr.into())],
    );
    l2_nonce += 1;

    // withdraw stx from L2
    submit_tx(&l2_rpc_origin, &l2_withdraw_stx_tx);

    // Wait to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // TODO: here, read the withdrawal events to get the withdrawal ID, and figure out the
    //       block height to query.
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
                    if data_map.get("type").unwrap().clone().expect_ascii() != "stx" {
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

    // should only be one withdrawal event
    assert_eq!(withdraw_events.len(), 1);
    let (withdrawal_height, withdrawal) = withdraw_events.pop().unwrap();

    let withdrawal_id = withdrawal
        .get("withdrawal_id")
        .unwrap()
        .clone()
        .expect_u128() as u32;
    let withdrawal_amount: u64 = withdrawal.get("amount").unwrap().clone().expect_u128() as u64;
    let withdrawal_sender = withdrawal
        .get("sender")
        .unwrap()
        .clone()
        .expect_principal()
        .to_string();

    assert_eq!(withdrawal_id, 0);
    assert_eq!(withdrawal_amount, 1);
    assert_eq!(withdrawal_sender, user_addr.to_string());

    let withdrawal_entry = get_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        withdrawal_id,
        withdrawal_amount,
    );

    // Check that the user does not own any additional STX anymore on the subnet now
    let account = get_account(&l2_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l2_starting_account_balance - default_fee * l2_nonce) as u128
    );
    // Check that the user's balance has not yet increased on the L1
    let account = get_account(&l1_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l1_starting_account_balance - default_fee * l1_nonce - 1) as u128
    );

    // Create the withdrawal merkle tree by mocking the stx withdraw event (if the root hash of
    // this constructed merkle tree is not identical to the root hash published by the subnet node,
    // then the test will fail).
    let mut spending_condition = TransactionSpendingCondition::new_singlesig_p2pkh(
        StacksPublicKey::from_private(&MOCKNET_PRIVATE_KEY_1),
    )
    .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(l2_nonce - 1);
    spending_condition.set_tx_fee(1000);
    let auth = TransactionAuth::Standard(spending_condition);
    let mut stx_withdraw_event =
        StacksTransactionEvent::SmartContractEvent(SmartContractEventData {
            key: (boot_code_id("subnet", false), "print".into()),
            value: Value::Tuple(
                TupleData::from_data(vec![
                    (
                        "type".into(),
                        Value::string_ascii_from_bytes("stx".to_string().into_bytes()).unwrap(),
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

    let withdrawal_receipt = StacksTransactionReceipt {
        transaction: TransactionOrigin::Stacks(StacksTransaction::new(
            TransactionVersion::Testnet,
            auth.clone(),
            TransactionPayload::Coinbase(CoinbasePayload([0u8; 32])),
        )),
        events: vec![stx_withdraw_event.clone()],
        post_condition_aborted: false,
        result: Value::err_none(),
        stx_burned: 0,
        contract_analysis: None,
        execution_cost: ExecutionCost::zero(),
        microblock_header: None,
        tx_index: 0,
    };
    let mut receipts = vec![withdrawal_receipt];

    // okay to pass a zero block height in tests: the block height parameter is only used for logging
    let withdrawal_tree = create_withdrawal_merkle_tree(&mut receipts, withdrawal_height);
    let root_hash = withdrawal_tree.root().as_bytes().to_vec();

    // okay to pass a zero block height in tests: the block height parameter is only used for logging
    let stx_withdrawal_key =
        generate_key_from_event(&mut stx_withdraw_event, 0, withdrawal_height).unwrap();
    let stx_withdrawal_key_bytes = convert_withdrawal_key_to_bytes(&stx_withdrawal_key);
    let stx_withdrawal_leaf_hash =
        MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(stx_withdrawal_key_bytes.as_slice())
            .as_bytes()
            .to_vec();
    let stx_path = withdrawal_tree.path(&stx_withdrawal_key_bytes).unwrap();

    let mut stx_sib_data = Vec::new();
    for sib in stx_path.iter() {
        let sib_hash = Value::buff_from(sib.hash.as_bytes().to_vec()).unwrap();
        // the sibling's side is the opposite of what PathOrder is set to
        let sib_is_left = Value::Bool(sib.order == MerklePathOrder::Right);
        let curr_sib_data = vec![
            (ClarityName::from("hash"), sib_hash),
            (ClarityName::from("is-left-side"), sib_is_left),
        ];
        let sib_tuple = Value::Tuple(TupleData::from_data(curr_sib_data).unwrap());
        stx_sib_data.push(sib_tuple);
    }

    let root_hash_val = Value::buff_from(root_hash.clone()).unwrap();
    let leaf_hash_val = Value::buff_from(stx_withdrawal_leaf_hash).unwrap();
    let siblings_val = Value::list_from(stx_sib_data).unwrap();

    assert_eq!(
        &root_hash_val, &withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &leaf_hash_val, &withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &siblings_val, &withdrawal_entry.siblings,
        "Sibling hashes should match value returned via RPC"
    );

    // test the result of our RPC call matches our constructed values

    let l1_withdraw_stx_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "withdraw-stx",
        &[
            Value::UInt(1),
            Value::Principal(user_addr.into()),
            Value::UInt(0),
            Value::UInt(withdrawal_height.into()),
            root_hash_val,
            leaf_hash_val,
            siblings_val,
        ],
    );
    l1_nonce += 1;

    // Withdraw 1 stx from subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_withdraw_stx_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user still does not own any additional STX on the subnet now
    let account = get_account(&l2_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l2_starting_account_balance - default_fee * l2_nonce) as u128
    );
    // Check that the user's STX was transferred back to the L1
    let account = get_account(&l1_rpc_origin, &user_addr);
    assert_eq!(
        account.balance,
        (l1_starting_account_balance - default_fee * l1_nonce) as u128
    );

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}

/// Test that we can bring up an L2 node and make some simple calls to the L2 chain.
/// Set up the L2 chain, make N calls, check that they are found in the listener.
#[test]
fn l2_simple_contract_calls() {
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";

    // Start the L2 run loop.
    let mut config = super::new_l1_test_conf(&*MOCKNET_PRIVATE_KEY_2, &*MOCKNET_PRIVATE_KEY_1);
    let miner_account = to_addr(&*MOCKNET_PRIVATE_KEY_2);

    let l2_rpc_origin = format!("http://{}", &config.node.rpc_bind);

    let user_addr = to_addr(&MOCKNET_PRIVATE_KEY_1);
    config.add_initial_balance(user_addr.to_string(), 10000000);

    config.events_observers.push(EventObserverConfig {
        endpoint: format!("localhost:{}", test_observer::EVENT_OBSERVER_PORT),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    test_observer::spawn();

    let mut run_loop = neon::RunLoop::new(config.clone());
    let termination_switch = run_loop.get_termination_switch();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Sleep to give the run loop time to start
    thread::sleep(Duration::from_millis(2_000));

    let burnchain = Burnchain::new(&config.get_burn_db_path(), &config.burnchain.chain).unwrap();
    let (sortition_db, _) = burnchain.open_db(true).unwrap();

    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), true);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");
    // Sleep to give the L1 chain time to start
    thread::sleep(Duration::from_millis(10_000));
    wait_for_target_l1_block(&sortition_db, MOCKNET_EPOCH_2_1);

    publish_subnet_contracts_to_l1(
        0,
        &config,
        miner_account.clone().into(),
        user_addr.clone().into(),
    );

    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    let small_contract = "(define-public (return-one) (ok 1))";
    let mut l2_nonce = 0;
    {
        let subnet_small_contract_publish = make_contract_publish(
            &MOCKNET_PRIVATE_KEY_1,
            config.node.chain_id,
            l2_nonce,
            1000,
            "small-contract",
            small_contract,
        );
        l2_nonce += 1;
        submit_tx(&l2_rpc_origin, &subnet_small_contract_publish);
    }
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Make two contract calls to "return-one".
    for _ in 0..2 {
        let small_contract_call1 = make_contract_call(
            &MOCKNET_PRIVATE_KEY_1,
            config.node.chain_id,
            l2_nonce,
            1000,
            &user_addr,
            "small-contract",
            "return-one",
            &[],
        );
        l2_nonce += 1;
        submit_tx(&l2_rpc_origin, &small_contract_call1);
        wait_for_next_stacks_block(&sortition_db);
    }
    // Wait extra blocks to avoid flakes.
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check for two calls to "return-one".
    let small_contract_calls = select_transactions_where(
        &test_observer::get_blocks(),
        |transaction| match &transaction.payload {
            TransactionPayload::ContractCall(contract) => {
                contract.contract_name == ContractName::try_from("small-contract").unwrap()
                    && contract.function_name == ClarityName::try_from("return-one").unwrap()
            }
            _ => false,
        },
    );
    assert_eq!(small_contract_calls.len(), 2);
    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}

/// This integration test verifies that:
/// (a) assets minted on L1 chain can be deposited into subnet
/// (b) assets minted on subnet can be withdrawn to the L1
#[test]
#[allow(unused_assignments)]
fn nft_deposit_and_withdraw_integration_test() {
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
    l1_nonce += 1;
    let nft_contract_name = ContractName::from("simple-nft");
    let nft_contract_id = QualifiedContractIdentifier::new(user_addr.into(), nft_contract_name);

    submit_tx(l1_rpc_origin, &nft_publish);

    println!("Submitted NFT and Subnet contracts onto L1!");

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

    // Setup subnet contract
    let subnet_setup_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "register-new-nft-contract",
        &[
            Value::Principal(PrincipalData::Contract(nft_contract_id.clone())),
            Value::Principal(PrincipalData::Contract(subnet_nft_contract_id.clone())),
        ],
    );
    l1_nonce += 1;

    submit_tx(&l2_rpc_origin, &subnet_nft_publish);
    submit_tx(l1_rpc_origin, &subnet_setup_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

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
    submit_tx(l1_rpc_origin, &l1_mint_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user does not own the L1 native NFT on the subnet now
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

    // deposit nft-token into subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_deposit_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user owns the L1 native NFT on the subnet now
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

    // Check that the user does not own the L1 native NFT on the L1 anymore (the contract should own it)
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
    let subnet_contract_principal = Value::okay(
        Value::some(Value::Principal(PrincipalData::Contract(
            QualifiedContractIdentifier::new(
                user_addr.into(),
                ContractName::from("subnet-controller"),
            ),
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(owner, subnet_contract_principal);

    // Check that the no one owns the subnet native NFT on the L1
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-owner",
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
        &TypeSignature::ResponseType(Box::new((
            TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
            TypeSignature::UIntType,
        ))),
    );
    assert_eq!(owner, Value::okay(Value::none()).unwrap());

    // Withdraw the L1 native NFT from the L2 (with `nft-withdraw?`)
    let l2_withdraw_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        &boot_code_addr(false),
        "subnet",
        "nft-withdraw?",
        &[
            Value::Principal(PrincipalData::Contract(subnet_nft_contract_id.clone())),
            Value::UInt(1),
            Value::Principal(user_addr.into()),
        ],
    );
    l2_nonce += 1;
    // Withdraw the subnet native nft from the L2 (with `nft-withdraw?`)
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
    l2_nonce += 1;
    // Submit withdrawal function calls
    submit_tx(&l2_rpc_origin, &l2_withdraw_nft_tx);
    submit_tx(&l2_rpc_origin, &l2_withdraw_native_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that user no longer owns the l1 native NFT on L2 chain.
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
    // Check that user no longer owns the subnet native NFT on L2 chain.
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
    let addr = Value::deserialize(
        &result,
        &TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
    );
    assert_eq!(addr, Value::none(),);

    // Check that the user does not *yet* own the L1 native NFT on the L1 (the contract should still own it)
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
    let subnet_contract_principal = Value::okay(
        Value::some(Value::Principal(PrincipalData::Contract(
            QualifiedContractIdentifier::new(
                user_addr.into(),
                ContractName::from("subnet-controller"),
            ),
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(owner, subnet_contract_principal);

    // Check that the user does not *yet* own the subnet native NFT on the L1 (no one should own it)
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-owner",
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
        &TypeSignature::ResponseType(Box::new((
            TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
            TypeSignature::UIntType,
        ))),
    );
    assert_eq!(owner, Value::okay(Value::none()).unwrap());

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
    assert_eq!(withdraw_events.len(), 2);
    let (withdrawal_height, _withdrawal) = withdraw_events.pop().unwrap();

    let l1_native_nft_withdrawal_entry = get_nft_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        0,
        subnet_nft_contract_id.clone(),
        1,
    );
    let subnet_native_nft_withdrawal_entry = get_nft_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        1,
        subnet_nft_contract_id.clone(),
        5,
    );

    // Create the withdrawal merkle tree by mocking both nft withdraw events (if the root hash of
    // this constructed merkle tree is not identical to the root hash published by the subnet node,
    // then the test will fail).
    let mut spending_condition = TransactionSpendingCondition::new_singlesig_p2pkh(
        StacksPublicKey::from_private(&MOCKNET_PRIVATE_KEY_1),
    )
    .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(l2_nonce - 1);
    spending_condition.set_tx_fee(1000);
    let auth = TransactionAuth::Standard(spending_condition);
    let mut l1_native_nft_withdraw_event =
        StacksTransactionEvent::SmartContractEvent(SmartContractEventData {
            key: (boot_code_id("subnet", false), "print".into()),
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
    let mut subnet_native_nft_withdraw_event =
        StacksTransactionEvent::SmartContractEvent(SmartContractEventData {
            key: (boot_code_id("subnet", false), "print".into()),
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
                    ("id".into(), Value::UInt(5)),
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
        events: vec![
            l1_native_nft_withdraw_event.clone(),
            subnet_native_nft_withdraw_event.clone(),
        ],
        post_condition_aborted: false,
        result: Value::err_none(),
        stx_burned: 0,
        contract_analysis: None,
        execution_cost: ExecutionCost::zero(),
        microblock_header: None,
        tx_index: 0,
    };
    let withdrawal_tree =
        create_withdrawal_merkle_tree(&mut vec![withdrawal_receipt], withdrawal_height);
    let root_hash = withdrawal_tree.root().as_bytes().to_vec();

    let l1_native_nft_withdrawal_key =
        generate_key_from_event(&mut l1_native_nft_withdraw_event, 0, withdrawal_height).unwrap();
    let l1_native_nft_withdrawal_key_bytes =
        convert_withdrawal_key_to_bytes(&l1_native_nft_withdrawal_key);
    let l1_native_nft_withdrawal_leaf_hash = MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(
        l1_native_nft_withdrawal_key_bytes.as_slice(),
    )
    .as_bytes()
    .to_vec();
    let l1_native_nft_path = withdrawal_tree
        .path(&l1_native_nft_withdrawal_key_bytes)
        .unwrap();

    let mut l1_native_nft_sib_data = Vec::new();
    for (_i, sib) in l1_native_nft_path.iter().enumerate() {
        let sib_hash = Value::buff_from(sib.hash.as_bytes().to_vec()).unwrap();
        // the sibling's side is the opposite of what PathOrder is set to
        let sib_is_left = Value::Bool(sib.order == MerklePathOrder::Right);
        let curr_sib_data = vec![
            (ClarityName::from("hash"), sib_hash),
            (ClarityName::from("is-left-side"), sib_is_left),
        ];
        let sib_tuple = Value::Tuple(TupleData::from_data(curr_sib_data).unwrap());
        l1_native_nft_sib_data.push(sib_tuple);
    }

    let l1_native_root_hash_val = Value::buff_from(root_hash.clone()).unwrap();
    let l1_native_leaf_hash_val =
        Value::buff_from(l1_native_nft_withdrawal_leaf_hash.clone()).unwrap();
    let l1_native_siblings_val = Value::list_from(l1_native_nft_sib_data.clone()).unwrap();

    assert_eq!(
        &l1_native_root_hash_val, &l1_native_nft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &l1_native_leaf_hash_val, &l1_native_nft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &l1_native_siblings_val, &l1_native_nft_withdrawal_entry.siblings,
        "Sibling hashes should match value returned via RPC"
    );

    let subnet_native_nft_withdrawal_key =
        generate_key_from_event(&mut subnet_native_nft_withdraw_event, 1, withdrawal_height)
            .unwrap();
    let subnet_native_nft_withdrawal_key_bytes =
        convert_withdrawal_key_to_bytes(&subnet_native_nft_withdrawal_key);
    let subnet_native_nft_withdrawal_leaf_hash = MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(
        subnet_native_nft_withdrawal_key_bytes.as_slice(),
    )
    .as_bytes()
    .to_vec();
    let subnet_native_nft_path = withdrawal_tree
        .path(&subnet_native_nft_withdrawal_key_bytes)
        .unwrap();

    let mut subnet_native_nft_sib_data = Vec::new();
    for (_i, sib) in subnet_native_nft_path.iter().enumerate() {
        let sib_hash = Value::buff_from(sib.hash.as_bytes().to_vec()).unwrap();
        // the sibling's side is the opposite of what PathOrder is set to
        let sib_is_left = Value::Bool(sib.order == MerklePathOrder::Right);
        let curr_sib_data = vec![
            (ClarityName::from("hash"), sib_hash),
            (ClarityName::from("is-left-side"), sib_is_left),
        ];
        let sib_tuple = Value::Tuple(TupleData::from_data(curr_sib_data).unwrap());
        subnet_native_nft_sib_data.push(sib_tuple);
    }

    let subnet_native_root_hash_val = Value::buff_from(root_hash.clone()).unwrap();
    let subnet_native_leaf_hash_val =
        Value::buff_from(subnet_native_nft_withdrawal_leaf_hash.clone()).unwrap();
    let subnet_native_siblings_val = Value::list_from(subnet_native_nft_sib_data.clone()).unwrap();

    assert_eq!(
        &subnet_native_root_hash_val, &subnet_native_nft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &subnet_native_leaf_hash_val, &subnet_native_nft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &subnet_native_siblings_val, &subnet_native_nft_withdrawal_entry.siblings,
        "Sibling hashes should match value returned via RPC"
    );

    // TODO: call withdraw from unauthorized principal once leaf verification is added to the subnet contract

    let l1_withdraw_l1_native_nft_tx = make_contract_call(
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
            Value::UInt(0),
            Value::UInt(withdrawal_height.into()),
            Value::some(Value::Principal(PrincipalData::Contract(
                nft_contract_id.clone(),
            )))
            .unwrap(),
            Value::buff_from(root_hash.clone()).unwrap(),
            Value::buff_from(l1_native_nft_withdrawal_leaf_hash).unwrap(),
            Value::list_from(l1_native_nft_sib_data).unwrap(),
        ],
    );
    l1_nonce += 1;
    let l1_withdraw_subnet_native_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "withdraw-nft-asset",
        &[
            Value::Principal(PrincipalData::Contract(nft_contract_id.clone())),
            Value::UInt(5),
            Value::Principal(user_addr.into()),
            Value::UInt(1),
            Value::UInt(withdrawal_height.into()),
            Value::some(Value::Principal(PrincipalData::Contract(
                nft_contract_id.clone(),
            )))
            .unwrap(),
            Value::buff_from(root_hash).unwrap(),
            Value::buff_from(subnet_native_nft_withdrawal_leaf_hash).unwrap(),
            Value::list_from(subnet_native_nft_sib_data).unwrap(),
        ],
    );
    l1_nonce += 1;
    // Withdraw nft-token from subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_withdraw_l1_native_nft_tx);
    submit_tx(&l1_rpc_origin, &l1_withdraw_subnet_native_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user owns the L1 native NFT on the L1 chain now
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
    assert_eq!(
        owner,
        Value::okay(Value::some(Value::Principal(user_addr.into())).unwrap()).unwrap()
    );
    // Check that the user owns the subnet native NFT on the L1 chain now
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-nft",
        "get-owner",
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
        &TypeSignature::ResponseType(Box::new((
            TypeSignature::OptionalType(Box::new(TypeSignature::PrincipalType)),
            TypeSignature::UIntType,
        ))),
    );
    assert_eq!(
        owner,
        Value::okay(Value::some(Value::Principal(user_addr.into())).unwrap()).unwrap()
    );

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}

/// This integration test verifies that:
/// (a) When an NFT deposit to L2 fails user can unlock it from L1 contract
#[test]
#[allow(unused_assignments)]
fn nft_deposit_failure_and_refund_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace nft_deposit_failure_and_refund_integration_test
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
    l1_nonce += 1;
    let nft_contract_name = ContractName::from("simple-nft");
    let nft_contract_id = QualifiedContractIdentifier::new(user_addr.into(), nft_contract_name);

    submit_tx(l1_rpc_origin, &nft_publish);

    println!("Submitted NFT and Subnet contracts onto L1!");

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

    // Publish subnet contract for nft-token
    let subnet_nft_content =
        include_str!("../../../../core-contracts/contracts/helper/simple-nft-l2-no-deposit.clar");
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

    // Setup subnet contract
    let subnet_setup_nft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "register-new-nft-contract",
        &[
            Value::Principal(PrincipalData::Contract(nft_contract_id.clone())),
            Value::Principal(PrincipalData::Contract(subnet_nft_contract_id.clone())),
        ],
    );
    l1_nonce += 1;

    submit_tx(&l2_rpc_origin, &subnet_nft_publish);
    submit_tx(l1_rpc_origin, &subnet_setup_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

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
    submit_tx(l1_rpc_origin, &l1_mint_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user does not own the L1 native NFT on the subnet now
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

    // Attempt deposit nft-token into subnet contract on L1. Should fail
    submit_tx(&l1_rpc_origin, &l1_deposit_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that transfer failed and user does not have NFT on L2
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

    // Check that the user does not own the L1 native NFT on the L1 anymore (the contract should own it)
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
    let subnet_contract_principal = Value::okay(
        Value::some(Value::Principal(PrincipalData::Contract(
            QualifiedContractIdentifier::new(
                user_addr.into(),
                ContractName::from("subnet-controller"),
            ),
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(owner, subnet_contract_principal);

    // Check that contract owns the NFT on L1
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
    let subnet_contract_principal = Value::okay(
        Value::some(Value::Principal(PrincipalData::Contract(
            QualifiedContractIdentifier::new(
                user_addr.into(),
                ContractName::from("subnet-controller"),
            ),
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(owner, subnet_contract_principal);

    // Failed deposit should have generated a withdrawal event, find it
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

    //=========================================================================
    // TODO: Remove this section?
    //=========================================================================

    let (withdrawal_height, _withdrawal) = withdraw_events.pop().unwrap();

    let l1_native_nft_withdrawal_entry = get_nft_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        0,
        subnet_nft_contract_id.clone(),
        1,
    );

    // Create the withdrawal merkle tree by mocking both nft withdraw events (if the root hash of
    // this constructed merkle tree is not identical to the root hash published by the subnet node,
    // then the test will fail).
    let mut spending_condition = TransactionSpendingCondition::new_singlesig_p2pkh(
        StacksPublicKey::from_private(&MOCKNET_PRIVATE_KEY_1),
    )
    .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(l2_nonce - 1);
    spending_condition.set_tx_fee(1000);
    let auth = TransactionAuth::Standard(spending_condition);
    let mut l1_native_nft_withdraw_event =
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
        events: vec![
            l1_native_nft_withdraw_event.clone(),
        ],
        post_condition_aborted: false,
        result: Value::err_none(),
        stx_burned: 0,
        contract_analysis: None,
        execution_cost: ExecutionCost::zero(),
        microblock_header: None,
        tx_index: 0,
    };
    let withdrawal_tree =
        create_withdrawal_merkle_tree(&mut vec![withdrawal_receipt], withdrawal_height);
    let root_hash = withdrawal_tree.root().as_bytes().to_vec();

    let l1_native_nft_withdrawal_key =
        generate_key_from_event(&mut l1_native_nft_withdraw_event, 0, withdrawal_height).unwrap();
    let l1_native_nft_withdrawal_key_bytes =
        convert_withdrawal_key_to_bytes(&l1_native_nft_withdrawal_key);
    let l1_native_nft_withdrawal_leaf_hash = MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(
        l1_native_nft_withdrawal_key_bytes.as_slice(),
    )
    .as_bytes()
    .to_vec();
    let l1_native_nft_path = withdrawal_tree
        .path(&l1_native_nft_withdrawal_key_bytes)
        .unwrap();

    let mut l1_native_nft_sib_data = Vec::new();
    for (_i, sib) in l1_native_nft_path.iter().enumerate() {
        let sib_hash = Value::buff_from(sib.hash.as_bytes().to_vec()).unwrap();
        // the sibling's side is the opposite of what PathOrder is set to
        let sib_is_left = Value::Bool(sib.order == MerklePathOrder::Right);
        let curr_sib_data = vec![
            (ClarityName::from("hash"), sib_hash),
            (ClarityName::from("is-left-side"), sib_is_left),
        ];
        let sib_tuple = Value::Tuple(TupleData::from_data(curr_sib_data).unwrap());
        l1_native_nft_sib_data.push(sib_tuple);
    }

    let l1_native_root_hash_val = Value::buff_from(root_hash.clone()).unwrap();
    let l1_native_leaf_hash_val =
        Value::buff_from(l1_native_nft_withdrawal_leaf_hash.clone()).unwrap();
    let l1_native_siblings_val = Value::list_from(l1_native_nft_sib_data.clone()).unwrap();

    assert_eq!(
        &l1_native_root_hash_val, &l1_native_nft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &l1_native_leaf_hash_val, &l1_native_nft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &l1_native_siblings_val, &l1_native_nft_withdrawal_entry.siblings,
        "Sibling hashes should match value returned via RPC"
    );

    //=========================================================================
    // WIP below this line
    //=========================================================================

    // TODO: call withdraw from unauthorized principal once leaf verification is added to the subnet contract

    let l1_withdraw_l1_native_nft_tx = make_contract_call(
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
            Value::UInt(0),
            Value::UInt(withdrawal_height.into()),
            Value::some(Value::Principal(PrincipalData::Contract(
                nft_contract_id.clone(),
            )))
            .unwrap(),
            Value::buff_from(root_hash.clone()).unwrap(),
            Value::buff_from(l1_native_nft_withdrawal_leaf_hash).unwrap(),
            Value::list_from(l1_native_nft_sib_data).unwrap(),
        ],
    );
    l1_nonce += 1;
    // Withdraw nft-token from subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_withdraw_l1_native_nft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user owns the L1 native NFT on the L1 chain now
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
    assert_eq!(
        owner,
        Value::okay(Value::some(Value::Principal(user_addr.into())).unwrap()).unwrap()
    );

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}

/// This integration test verifies that:
/// (a) assets minted on L1 chain can be deposited into subnet
/// (b) assets minted on subnet can be withdrawn to the L1
#[test]
#[allow(unused_assignments)]
fn ft_deposit_and_withdraw_integration_test() {
    // running locally:
    // STACKS_BASE_DIR=~/devel/stacks-blockchain/target/release/stacks-node STACKS_NODE_TEST=1 cargo test --workspace ft_deposit_and_withdraw_integration_test
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";
    let l1_rpc_origin = "http://127.0.0.1:20443";
    let trait_standards_contract_name = "trait-standards";

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

    // Publish a simple ft onto L1
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

    submit_tx(l1_rpc_origin, &ft_publish);

    println!("Submitted ft and Subnet contracts onto L1!");

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

    // Publish subnet contract for ft-token
    let subnet_simple_ft =
        include_str!("../../../../core-contracts/contracts/helper/simple-ft-l2.clar");
    let subnet_ft_publish = make_contract_publish(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        "simple-ft",
        subnet_simple_ft,
    );
    l2_nonce += 1;
    let subnet_ft_contract_id =
        QualifiedContractIdentifier::new(user_addr.into(), ContractName::from("simple-ft"));

    submit_tx(&l2_rpc_origin, &subnet_ft_publish);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Register the contract with the subnet
    let subnet_setup_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        config.burnchain.contract_identifier.name.as_str(),
        "register-new-ft-contract",
        &[
            Value::Principal(PrincipalData::Contract(ft_contract_id.clone())),
            Value::Principal(PrincipalData::Contract(subnet_ft_contract_id.clone())),
        ],
    );
    l1_nonce += 1;

    submit_tx(l1_rpc_origin, &subnet_setup_ft_tx);

    // Mint 2 ft-tokens for user on L1 chain
    let l1_mint_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        LAYER_1_CHAIN_ID_TESTNET,
        l1_nonce,
        1_000_000,
        &user_addr,
        "simple-ft",
        "gift-tokens",
        &[Value::UInt(2), Value::Principal(user_addr.into())],
    );
    l1_nonce += 1;

    // Mint 5 ft-tokens for user on subnet
    let l2_mint_ft_tx = make_contract_call(
        &MOCKNET_PRIVATE_KEY_1,
        config.node.chain_id,
        l2_nonce,
        1_000_000,
        &user_addr,
        "simple-ft",
        "gift-tokens",
        &[Value::UInt(5), Value::Principal(user_addr.into())],
    );
    l2_nonce += 1;

    submit_tx(&l2_rpc_origin, &l2_mint_ft_tx);
    submit_tx(l1_rpc_origin, &l1_mint_ft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user does not own the L1 native ft on the subnet now
    let res = call_read_only(
        &l2_rpc_origin,
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
    assert_eq!(amount, Value::okay(Value::UInt(5)).unwrap());

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

    // deposit 1 ft-token into subnet contract on L1
    let tx_res = submit_tx(&l1_rpc_origin, &l1_deposit_ft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user owns the L1 native ft on the subnet now
    let res = call_read_only(
        &l2_rpc_origin,
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
    assert_eq!(amount, Value::okay(Value::UInt(6)).unwrap());

    // Check that the user now only owns 1 ft on the L1
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

    // Check that the subnet contract owns 1 ft on the L1
    let subnet_contract_principal = Value::Principal(PrincipalData::Contract(
        config.burnchain.contract_identifier.clone(),
    ));
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-balance",
        vec![subnet_contract_principal.serialize()],
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

    // Withdraw the 4 (out of 6) of the ft-tokens from the L2 (with `ft-withdraw?`)
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
            Value::UInt(4),
            Value::Principal(user_addr.into()),
        ],
    );
    l2_nonce += 1;

    // Submit withdrawal function call
    submit_tx(&l2_rpc_origin, &l2_withdraw_ft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that user owns the remainder of the tokens on the subnet
    let res = call_read_only(
        &l2_rpc_origin,
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
    assert_eq!(amount, Value::okay(Value::UInt(2)).unwrap());

    // Check that the user does not *yet* own the additional ft tokens on the L1
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
                    if data_map.get("type").unwrap().clone().expect_ascii() != "ft" {
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
    let (withdrawal_height, _withdrawal) = withdraw_events.pop().unwrap();

    let ft_withdrawal_entry = get_ft_withdrawal_entry(
        &l2_rpc_origin,
        withdrawal_height,
        &user_addr,
        0,
        QualifiedContractIdentifier::new(user_addr.into(), ContractName::from("simple-ft")),
        4,
    );

    // Create the withdrawal merkle tree by mocking the ft withdraw event (if the root hash of
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
            key: (boot_code_id("subnet", false), "print".into()),
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
                    ("amount".into(), Value::UInt(4)),
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
        events: vec![ft_withdraw_event.clone()],
        post_condition_aborted: false,
        result: Value::err_none(),
        stx_burned: 0,
        contract_analysis: None,
        execution_cost: ExecutionCost::zero(),
        microblock_header: None,
        tx_index: 0,
    };
    let withdrawal_tree =
        create_withdrawal_merkle_tree(&mut vec![withdrawal_receipt], withdrawal_height);
    let root_hash = withdrawal_tree.root().as_bytes().to_vec();

    let ft_withdrawal_key =
        generate_key_from_event(&mut ft_withdraw_event, 0, withdrawal_height).unwrap();
    let ft_withdrawal_key_bytes = convert_withdrawal_key_to_bytes(&ft_withdrawal_key);
    let ft_withdrawal_leaf_hash =
        MerkleTree::<Sha512Trunc256Sum>::get_leaf_hash(ft_withdrawal_key_bytes.as_slice())
            .as_bytes()
            .to_vec();
    let ft_path = withdrawal_tree.path(&ft_withdrawal_key_bytes).unwrap();

    let mut ft_sib_data = Vec::new();
    for (_i, sib) in ft_path.iter().enumerate() {
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

    let root_hash_val = Value::buff_from(root_hash.clone()).unwrap();
    let leaf_hash_val = Value::buff_from(ft_withdrawal_leaf_hash.clone()).unwrap();
    let siblings_val = Value::list_from(ft_sib_data.clone()).unwrap();

    assert_eq!(
        &root_hash_val, &ft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &leaf_hash_val, &ft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &siblings_val, &ft_withdrawal_entry.siblings,
        "Sibling hashes should match value returned via RPC"
    );
    assert_eq!(
        &root_hash_val, &ft_withdrawal_entry.root_hash,
        "Root hash should match value returned via RPC"
    );
    assert_eq!(
        &leaf_hash_val, &ft_withdrawal_entry.leaf_hash,
        "Leaf hash should match value returned via RPC"
    );
    assert_eq!(
        &siblings_val, &ft_withdrawal_entry.siblings,
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
            Value::UInt(4),
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

    // Withdraw ft-token from subnet contract on L1
    submit_tx(&l1_rpc_origin, &l1_withdraw_ft_tx);

    // Sleep to give the run loop time to mine a block
    wait_for_next_stacks_block(&sortition_db);
    wait_for_next_stacks_block(&sortition_db);

    // Check that the user owns the tokens on the L1 chain now
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
    assert_eq!(amount, Value::okay(Value::UInt(5)).unwrap());

    // Check that the subnet contract no longer owns any tokens. It should have
    // transferred the 1 that it had, then minted the remaining 3.
    let res = call_read_only(
        &l1_rpc_origin,
        &user_addr,
        "simple-ft",
        "get-balance",
        vec![subnet_contract_principal.serialize()],
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

    termination_switch.store(false, Ordering::SeqCst);
    stacks_l1_controller.kill_process();
    run_loop_thread.join().expect("Failed to join run loop.");
}
