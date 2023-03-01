#[cfg(test)]
mod tests {
    use crate::{
        chainstate::{
            burn::db::sortdb::SortitionDB,
            stacks::{
                db::{blocks::MemPoolRejection, StacksChainState},
                miner::{
                    test::{
                        get_stacks_account, make_coinbase, make_contract_call, make_token_transfer,
                        make_user_contract_call, make_user_contract_publish,
                        make_user_stacks_transfer,
                    },
                    BlockBuilderSettings,
                },
                StacksBlock, StacksBlockBuilder, StacksBlockHeader,
            },
        },
        core::{mempool::MemPoolWalkSettings, MemPoolDB},
        net::test::{TestPeer, TestPeerConfig},
    };

    use super::*;
    use clarity::{
        address::{AddressHashMode, C32_ADDRESS_VERSION_TESTNET_SINGLESIG},
        types::{
            chainstate::{ConsensusHash, StacksAddress, StacksPrivateKey, StacksPublicKey},
            Address, StacksEpochId,
        },
        vm::{costs::ExecutionCost, types::StacksAddressExtensions, Value},
    };
    use rand::Rng;
    use stacks_common::util::hash::Hash160;

    #[test]
    #[ignore]
    fn test_max_block() {
        let mut privks = vec![];
        let mut addrs = vec![];
        let mut balances = vec![];
        let deployer = StacksPrivateKey::new();
        let deployer_addr = StacksAddress::from_public_keys(
            C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
            &AddressHashMode::SerializeP2PKH,
            1,
            &vec![StacksPublicKey::from_private(&deployer)],
        )
        .unwrap();

        balances.push((deployer_addr.to_account_principal(), 100000000));

        for _ in 0..5000 {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(
                C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
                &AddressHashMode::SerializeP2PKH,
                1,
                &vec![StacksPublicKey::from_private(&privk)],
            )
            .unwrap();

            privks.push(privk);
            addrs.push(addr);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("bench_microblocks", 2040, 2041);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();
            sn.block_height
        };

        let recipient_addr_str = "ST1RFD5Q2QPK3E0F08HG9XDX7SSC7CNRS0QR0SGEV";
        let recipient = StacksAddress::from_string(recipient_addr_str).unwrap();

        let mut last_block: Option<StacksBlock> = None;
        for tenure_id in 0..2 {
            let tip =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(
                |ref mut miner,
                 ref mut sortdb,
                 ref mut chainstate,
                 vrf_proof,
                 ref parent_opt,
                 ref parent_microblock_header_opt| {
                    let parent_tip = match parent_opt {
                        None => StacksChainState::get_genesis_header_info(chainstate.db()).unwrap(),
                        Some(block) => {
                            let ic = sortdb.index_conn();
                            let snapshot =
                                SortitionDB::get_block_snapshot_for_winning_stacks_block(
                                    &ic,
                                    &tip.sortition_id,
                                    &block.block_hash(),
                                )
                                .unwrap()
                                .unwrap(); // succeeds because we don't fork
                            StacksChainState::get_anchored_block_header_info(
                                chainstate.db(),
                                &snapshot.consensus_hash,
                                &snapshot.winning_stacks_block_hash,
                            )
                            .unwrap()
                            .unwrap()
                        }
                    };

                    let parent_header_hash = parent_tip.anchored_header.block_hash();
                    let parent_consensus_hash = parent_tip.consensus_hash.clone();

                    let mut mempool =
                        MemPoolDB::open_test(false, 0x80000000, &chainstate_path).unwrap();

                    let coinbase_tx = make_coinbase(miner, tenure_id);
                    let sort_ic = sortdb.index_conn();
                    if tenure_id == 0 {
                        // Deploy the NFT contract in the first block
                        let nft_contract = include_str!(
                            "../../../core-contracts/contracts/templates/helper/simple-nft-l2.clar"
                        );
                        let deploy_contract = make_user_contract_publish(
                            &deployer,
                            0,
                            2000,
                            "simple-nft",
                            nft_contract,
                        );
                        mempool
                            .submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &deploy_contract,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            )
                            .unwrap();
                    } else {
                        let mut rng = rand::thread_rng();
                        let mut nonces = vec![0; addrs.len()];
                        // Add a variety of transactions to the mempool
                        for n in 0..20_000 {
                            let sender = rng.gen_range(0, privks.len());
                            let tx = match rng.gen_range(0, 3) {
                                0 => make_user_contract_call(
                                    &privks[sender],
                                    nonces[sender],
                                    2000,
                                    deployer_addr.clone(),
                                    "simple-nft",
                                    "mint-next",
                                    &[Value::Principal(addrs[sender].clone().into())],
                                ),
                                1 => make_user_contract_call(
                                    &privks[sender],
                                    nonces[sender],
                                    2000,
                                    deployer_addr.clone(),
                                    "simple-nft",
                                    "transfer",
                                    &[
                                        Value::UInt(n as u128),
                                        Value::Principal(addrs[sender].clone().into()),
                                        Value::Principal(recipient.clone().into()),
                                    ],
                                ),
                                2 => make_user_stacks_transfer(
                                    &privks[sender],
                                    nonces[sender],
                                    500,
                                    &recipient.clone().into(),
                                    20,
                                ),
                                _ => unreachable!(),
                            };
                            match mempool.submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &tx,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            ) {
                                Ok(_) => {}
                                Err(MemPoolRejection::TooMuchChaining {
                                    max_nonce,
                                    actual_nonce,
                                    principal,
                                    is_origin,
                                }) => {}
                                Err(e) => {
                                    eprintln!("Failed to submit tx: {:?}", &e);
                                    panic!();
                                }
                            }

                            nonces[sender] += 1;
                        }
                    }

                    // Mine the block
                    let anchored_block = StacksBlockBuilder::build_anchored_block(
                        chainstate,
                        &sort_ic,
                        &mut mempool,
                        &parent_tip,
                        tip.total_burn,
                        vrf_proof,
                        Hash160([tenure_id as u8; 20]),
                        &coinbase_tx,
                        BlockBuilderSettings::max_value(),
                        None,
                    )
                    .unwrap();

                    (anchored_block.0, vec![])
                },
            );

            last_block = Some(stacks_block.clone());

            test_debug!("Process tenure {}", 0);

            // should always succeed
            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip_checked(&stacks_block, &vec![])
                .unwrap();
        }
    }

    #[test]
    #[ignore]
    fn test_max_block_stx_transfers_only() {
        let mut privks = vec![];
        let mut addrs = vec![];
        let mut balances = vec![];
        let deployer = StacksPrivateKey::new();
        let deployer_addr = StacksAddress::from_public_keys(
            C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
            &AddressHashMode::SerializeP2PKH,
            1,
            &vec![StacksPublicKey::from_private(&deployer)],
        )
        .unwrap();

        balances.push((deployer_addr.to_account_principal(), 100000000));

        for _ in 0..5000 {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(
                C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
                &AddressHashMode::SerializeP2PKH,
                1,
                &vec![StacksPublicKey::from_private(&privk)],
            )
            .unwrap();

            privks.push(privk);
            addrs.push(addr);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("bench_microblocks", 2040, 2041);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();
            sn.block_height
        };

        let recipient_addr_str = "ST1RFD5Q2QPK3E0F08HG9XDX7SSC7CNRS0QR0SGEV";
        let recipient = StacksAddress::from_string(recipient_addr_str).unwrap();

        let mut last_block: Option<StacksBlock> = None;
        for tenure_id in 0..2 {
            let tip =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(
                |ref mut miner,
                 ref mut sortdb,
                 ref mut chainstate,
                 vrf_proof,
                 ref parent_opt,
                 ref parent_microblock_header_opt| {
                    let parent_tip = match parent_opt {
                        None => StacksChainState::get_genesis_header_info(chainstate.db()).unwrap(),
                        Some(block) => {
                            let ic = sortdb.index_conn();
                            let snapshot =
                                SortitionDB::get_block_snapshot_for_winning_stacks_block(
                                    &ic,
                                    &tip.sortition_id,
                                    &block.block_hash(),
                                )
                                .unwrap()
                                .unwrap(); // succeeds because we don't fork
                            StacksChainState::get_anchored_block_header_info(
                                chainstate.db(),
                                &snapshot.consensus_hash,
                                &snapshot.winning_stacks_block_hash,
                            )
                            .unwrap()
                            .unwrap()
                        }
                    };

                    let parent_header_hash = parent_tip.anchored_header.block_hash();
                    let parent_consensus_hash = parent_tip.consensus_hash.clone();

                    let mut mempool =
                        MemPoolDB::open_test(false, 0x80000000, &chainstate_path).unwrap();

                    let coinbase_tx = make_coinbase(miner, tenure_id);
                    let sort_ic = sortdb.index_conn();
                    if tenure_id == 0 {
                        // Deploy the NFT contract in the first block
                        let nft_contract = include_str!(
                            "../../../core-contracts/contracts/templates/helper/simple-nft-l2.clar"
                        );
                        let deploy_contract = make_user_contract_publish(
                            &deployer,
                            0,
                            2000,
                            "simple-nft",
                            nft_contract,
                        );
                        mempool
                            .submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &deploy_contract,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            )
                            .unwrap();
                    } else {
                        let mut rng = rand::thread_rng();
                        let mut nonces = vec![0; addrs.len()];
                        // Add a variety of transactions to the mempool
                        for n in 0..20_000 {
                            let sender = rng.gen_range(0, privks.len());
                            let tx = make_user_stacks_transfer(
                                &privks[sender],
                                nonces[sender],
                                500,
                                &recipient.clone().into(),
                                20,
                            );
                            match mempool.submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &tx,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            ) {
                                Ok(_) => {}
                                Err(MemPoolRejection::TooMuchChaining {
                                    max_nonce,
                                    actual_nonce,
                                    principal,
                                    is_origin,
                                }) => {}
                                Err(e) => {
                                    eprintln!("Failed to submit tx: {:?}", &e);
                                    panic!();
                                }
                            }

                            nonces[sender] += 1;
                        }
                    }

                    // Mine the block
                    let anchored_block = StacksBlockBuilder::build_anchored_block(
                        chainstate,
                        &sort_ic,
                        &mut mempool,
                        &parent_tip,
                        tip.total_burn,
                        vrf_proof,
                        Hash160([tenure_id as u8; 20]),
                        &coinbase_tx,
                        BlockBuilderSettings::max_value(),
                        None,
                    )
                    .unwrap();

                    (anchored_block.0, vec![])
                },
            );

            last_block = Some(stacks_block.clone());

            test_debug!("Process tenure {}", 0);

            // should always succeed
            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip_checked(&stacks_block, &vec![])
                .unwrap();
        }
    }

    #[test]
    #[ignore]
    fn test_15s_block() {
        let mut privks = vec![];
        let mut addrs = vec![];
        let mut balances = vec![];
        let deployer = StacksPrivateKey::new();
        let deployer_addr = StacksAddress::from_public_keys(
            C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
            &AddressHashMode::SerializeP2PKH,
            1,
            &vec![StacksPublicKey::from_private(&deployer)],
        )
        .unwrap();

        balances.push((deployer_addr.to_account_principal(), 100000000));

        for _ in 0..5000 {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(
                C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
                &AddressHashMode::SerializeP2PKH,
                1,
                &vec![StacksPublicKey::from_private(&privk)],
            )
            .unwrap();

            privks.push(privk);
            addrs.push(addr);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("bench_microblocks", 2040, 2041);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();
            sn.block_height
        };

        let recipient_addr_str = "ST1RFD5Q2QPK3E0F08HG9XDX7SSC7CNRS0QR0SGEV";
        let recipient = StacksAddress::from_string(recipient_addr_str).unwrap();

        let mut last_block: Option<StacksBlock> = None;
        for tenure_id in 0..2 {
            let tip =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(
                |ref mut miner,
                 ref mut sortdb,
                 ref mut chainstate,
                 vrf_proof,
                 ref parent_opt,
                 ref parent_microblock_header_opt| {
                    let parent_tip = match parent_opt {
                        None => StacksChainState::get_genesis_header_info(chainstate.db()).unwrap(),
                        Some(block) => {
                            let ic = sortdb.index_conn();
                            let snapshot =
                                SortitionDB::get_block_snapshot_for_winning_stacks_block(
                                    &ic,
                                    &tip.sortition_id,
                                    &block.block_hash(),
                                )
                                .unwrap()
                                .unwrap(); // succeeds because we don't fork
                            StacksChainState::get_anchored_block_header_info(
                                chainstate.db(),
                                &snapshot.consensus_hash,
                                &snapshot.winning_stacks_block_hash,
                            )
                            .unwrap()
                            .unwrap()
                        }
                    };

                    let parent_header_hash = parent_tip.anchored_header.block_hash();
                    let parent_consensus_hash = parent_tip.consensus_hash.clone();

                    let mut mempool =
                        MemPoolDB::open_test(false, 0x80000000, &chainstate_path).unwrap();

                    let coinbase_tx = make_coinbase(miner, tenure_id);
                    let sort_ic = sortdb.index_conn();
                    if tenure_id == 0 {
                        // Deploy the NFT contract in the first block
                        let nft_contract = include_str!(
                            "../../../core-contracts/contracts/templates/helper/simple-nft-l2.clar"
                        );
                        let deploy_contract = make_user_contract_publish(
                            &deployer,
                            0,
                            2000,
                            "simple-nft",
                            nft_contract,
                        );
                        mempool
                            .submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &deploy_contract,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            )
                            .unwrap();
                    } else {
                        let mut rng = rand::thread_rng();
                        let mut nonces = vec![0; addrs.len()];
                        // Add a variety of transactions to the mempool
                        for n in 0..20_000 {
                            let sender = rng.gen_range(0, privks.len());
                            let tx = match rng.gen_range(0, 3) {
                                0 => make_user_contract_call(
                                    &privks[sender],
                                    nonces[sender],
                                    2000,
                                    deployer_addr.clone(),
                                    "simple-nft",
                                    "mint-next",
                                    &[Value::Principal(addrs[sender].clone().into())],
                                ),
                                1 => make_user_contract_call(
                                    &privks[sender],
                                    nonces[sender],
                                    2000,
                                    deployer_addr.clone(),
                                    "simple-nft",
                                    "transfer",
                                    &[
                                        Value::UInt(n as u128),
                                        Value::Principal(addrs[sender].clone().into()),
                                        Value::Principal(recipient.clone().into()),
                                    ],
                                ),
                                2 => make_user_stacks_transfer(
                                    &privks[sender],
                                    nonces[sender],
                                    500,
                                    &recipient.clone().into(),
                                    20,
                                ),
                                _ => unreachable!(),
                            };
                            match mempool.submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &tx,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            ) {
                                Ok(_) => {}
                                Err(MemPoolRejection::TooMuchChaining {
                                    max_nonce,
                                    actual_nonce,
                                    principal,
                                    is_origin,
                                }) => {}
                                Err(e) => {
                                    eprintln!("Failed to submit tx: {:?}", &e);
                                    panic!();
                                }
                            }

                            nonces[sender] += 1;
                        }
                    }

                    // Mine the block
                    let anchored_block = StacksBlockBuilder::build_anchored_block(
                        chainstate,
                        &sort_ic,
                        &mut mempool,
                        &parent_tip,
                        tip.total_burn,
                        vrf_proof,
                        Hash160([tenure_id as u8; 20]),
                        &coinbase_tx,
                        BlockBuilderSettings {
                            max_miner_time_ms: 15_000,
                            mempool_settings: MemPoolWalkSettings::default(),
                        },
                        None,
                    )
                    .unwrap();

                    (anchored_block.0, vec![])
                },
            );

            last_block = Some(stacks_block.clone());

            test_debug!("Process tenure {}", 0);

            // should always succeed
            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip_checked(&stacks_block, &vec![])
                .unwrap();
        }
    }

    #[test]
    #[ignore]
    fn test_15s_block_stx_transfers_only() {
        let mut privks = vec![];
        let mut addrs = vec![];
        let mut balances = vec![];
        let deployer = StacksPrivateKey::new();
        let deployer_addr = StacksAddress::from_public_keys(
            C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
            &AddressHashMode::SerializeP2PKH,
            1,
            &vec![StacksPublicKey::from_private(&deployer)],
        )
        .unwrap();

        balances.push((deployer_addr.to_account_principal(), 100000000));

        for _ in 0..5000 {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(
                C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
                &AddressHashMode::SerializeP2PKH,
                1,
                &vec![StacksPublicKey::from_private(&privk)],
            )
            .unwrap();

            privks.push(privk);
            addrs.push(addr);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("bench_microblocks", 2040, 2041);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();
            sn.block_height
        };

        let recipient_addr_str = "ST1RFD5Q2QPK3E0F08HG9XDX7SSC7CNRS0QR0SGEV";
        let recipient = StacksAddress::from_string(recipient_addr_str).unwrap();

        let mut last_block: Option<StacksBlock> = None;
        for tenure_id in 0..2 {
            let tip =
                SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
                    .unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(
                |ref mut miner,
                 ref mut sortdb,
                 ref mut chainstate,
                 vrf_proof,
                 ref parent_opt,
                 ref parent_microblock_header_opt| {
                    let parent_tip = match parent_opt {
                        None => StacksChainState::get_genesis_header_info(chainstate.db()).unwrap(),
                        Some(block) => {
                            let ic = sortdb.index_conn();
                            let snapshot =
                                SortitionDB::get_block_snapshot_for_winning_stacks_block(
                                    &ic,
                                    &tip.sortition_id,
                                    &block.block_hash(),
                                )
                                .unwrap()
                                .unwrap(); // succeeds because we don't fork
                            StacksChainState::get_anchored_block_header_info(
                                chainstate.db(),
                                &snapshot.consensus_hash,
                                &snapshot.winning_stacks_block_hash,
                            )
                            .unwrap()
                            .unwrap()
                        }
                    };

                    let parent_header_hash = parent_tip.anchored_header.block_hash();
                    let parent_consensus_hash = parent_tip.consensus_hash.clone();

                    let mut mempool =
                        MemPoolDB::open_test(false, 0x80000000, &chainstate_path).unwrap();

                    let coinbase_tx = make_coinbase(miner, tenure_id);
                    let sort_ic = sortdb.index_conn();
                    if tenure_id == 0 {
                        // Deploy the NFT contract in the first block
                        let nft_contract = include_str!(
                            "../../../core-contracts/contracts/templates/helper/simple-nft-l2.clar"
                        );
                        let deploy_contract = make_user_contract_publish(
                            &deployer,
                            0,
                            2000,
                            "simple-nft",
                            nft_contract,
                        );
                        mempool
                            .submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &deploy_contract,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            )
                            .unwrap();
                    } else {
                        let mut rng = rand::thread_rng();
                        let mut nonces = vec![0; addrs.len()];
                        // Add a variety of transactions to the mempool
                        for n in 0..20_000 {
                            let sender = rng.gen_range(0, privks.len());
                            let tx = make_user_stacks_transfer(
                                &privks[sender],
                                nonces[sender],
                                500,
                                &recipient.clone().into(),
                                20,
                            );
                            match mempool.submit(
                                chainstate,
                                &parent_consensus_hash,
                                &parent_header_hash,
                                &tx,
                                None,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                            ) {
                                Ok(_) => {}
                                Err(MemPoolRejection::TooMuchChaining {
                                    max_nonce,
                                    actual_nonce,
                                    principal,
                                    is_origin,
                                }) => {}
                                Err(e) => {
                                    eprintln!("Failed to submit tx: {:?}", &e);
                                    panic!();
                                }
                            }

                            nonces[sender] += 1;
                        }
                    }

                    // Mine the block
                    let anchored_block = StacksBlockBuilder::build_anchored_block(
                        chainstate,
                        &sort_ic,
                        &mut mempool,
                        &parent_tip,
                        tip.total_burn,
                        vrf_proof,
                        Hash160([tenure_id as u8; 20]),
                        &coinbase_tx,
                        BlockBuilderSettings {
                            max_miner_time_ms: 15_000,
                            mempool_settings: MemPoolWalkSettings::default(),
                        },
                        None,
                    )
                    .unwrap();

                    (anchored_block.0, vec![])
                },
            );

            last_block = Some(stacks_block.clone());

            test_debug!("Process tenure {}", 0);

            // should always succeed
            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip_checked(&stacks_block, &vec![])
                .unwrap();
        }
    }
}
