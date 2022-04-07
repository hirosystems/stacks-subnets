use std;
use std::thread;

use crate::burnchains::db_indexer::DBBurnchainIndexer;
use crate::neon;
use crate::stacks::burnchains::indexer::BurnchainIndexer;
use crate::tests::StacksL1Controller;
use clarity::util::hash::to_hex;
use rand::RngCore;
use stacks::burnchains::Burnchain;
use stacks::util::sleep_ms;
use std::env;
use std::time::Duration;

fn random_sortdb_test_dir() -> String {
    let mut rng = rand::thread_rng();
    let mut buf = [0u8; 32];
    rng.fill_bytes(&mut buf);
    format!("/tmp/stacks-node-tests/sortdb/test-{}", to_hex(&buf))
}

/// This test brings up the Stacks-L1 chain in "mocknet" mode, and ensures that our listener can hear and record burn blocks
/// from the Stacks-L1 chain.
#[test]
fn l1_observer_test() {
    if env::var("STACKS_NODE_TEST") != Ok("1".into()) {
        return;
    }

    // Start the L2 run loop.
    let mut config = super::new_test_conf();
    config.burnchain.chain = "stacks_layer_1".to_string();
    config.burnchain.mode = "hyperchain".to_string();

    let db_path_dir = random_sortdb_test_dir();
    config.burnchain.indexer_base_db_path = db_path_dir;
    config.burnchain.first_burn_header_hash =
        "a7578f11a428bb953e7bbced9858525b6eec0d24d5d9d77285a7d7d891f68561".to_string();

    let mut run_loop = neon::RunLoop::new(config.clone());
    let channel = run_loop.get_coordinator_channel().unwrap();
    thread::spawn(move || run_loop.start(None, 0));

    // Start Stacks L1.
    let l1_toml_file = "../../contrib/conf/stacks-l1-mocknet.toml";
    let mut stacks_l1_controller = StacksL1Controller::new(l1_toml_file.to_string(), true);
    let _stacks_res = stacks_l1_controller
        .start_process()
        .expect("stacks l1 controller didn't start");

    // Sleep to give the run loop time to listen to blocks.
    thread::sleep(Duration::from_millis(45000));

    let indexer = DBBurnchainIndexer::new(config.burnchain.clone(), false)
        .expect("Should be able to create DBBurnchainIndexer.");
    let tip_height = indexer
        .get_highest_header_height()
        .expect("Should have a highest block.");

    // Ensure that the tip height has moved beyond height 0.
    // We check that we have moved past 3 just to establish we are reliably getting blocks.
    assert!(tip_height > 3);

    channel.stop_chains_coordinator();
    stacks_l1_controller.kill_process();
}
