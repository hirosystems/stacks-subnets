use crate::burnchains::commitment::{calculate_fee_rate_adjustment, compute_fee_from_response_and_transaction, FeeCalculationError};
use clarity::vm::costs::ExecutionCost;
use clarity::vm::types::PrincipalData;
use clarity::vm::types::PrincipalData::Standard;
use clarity::vm::Value::Sequence;
use clarity::vm::{ClarityName, ContractName};
use stacks::chainstate::stacks::SinglesigHashMode::P2PKH;
use stacks::chainstate::stacks::TransactionAnchorMode::Any;
use stacks::chainstate::stacks::TransactionPayload::ContractCall;
use stacks::chainstate::stacks::TransactionPostConditionMode::Allow;
use stacks::chainstate::stacks::TransactionPublicKeyEncoding::Compressed;
use stacks::chainstate::stacks::{
    StacksTransaction, TransactionAuth, TransactionContractCall, TransactionPayload,
    TransactionVersion,
};
use stacks::net::{RPCFeeEstimate, RPCFeeEstimateResponse};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::deps_common::bitcoin::network::constants::Network::Testnet;
use stacks_common::types::chainstate::StacksAddress;

/// Sample commitment transaction json taken from a mocknet run.
const example_transaction_json: &str = r#"
    {
   "version":"Testnet",
   "chain_id":2147483648,
   "auth":{
      "Standard":{
         "Singlesig":{
            "hash_mode":"P2PKH",
            "signer":"34a9b5f70954906d393566632ff4a13a17429b12",
            "nonce":5,
            "tx_fee":100000,
            "key_encoding":"Compressed",
            "signature":"007a3d4764f650cc613ad051207dfb4730869d94fceb94dabe05dac286b5cda5195ef56e4ddd9c34fa787c1b566ca7aea4563a56b3a937bed3fde91ebc56639b04"
         }
      }
   },
   "anchor_mode":"Any",
   "post_condition_mode":"Allow",
   "post_conditions":[],
   "payload":{
      "ContractCall":{
         "address":{
            "version":26,
            "bytes":"34a9b5f70954906d393566632ff4a13a17429b12"
         },
         "contract_name":"hc-alpha",
         "function_name":"commit-block",
         "function_args":[
            {
               "Sequence":{
                  "Buffer":{
                     "data":[ 206, 16, 11, 250, 147, 180, 249, 254, 15, 252, 33, 212, 98, 113, 21, 249, 22, 108, 102, 94, 211, 35, 86, 167, 64, 74, 86, 114, 188, 68, 253, 58 ]
                  }
               }
            },
            {
               "Sequence":{
                  "Buffer":{
                     "data":[ 36, 6, 105, 133, 53, 249, 193, 126, 100, 137, 12, 91, 125, 105, 94, 221, 76, 201, 130, 116, 143, 99, 157, 81, 165, 55, 72, 130, 252, 64, 129, 200 ]
                  }
               }
            },
            {
               "Sequence":{
                  "Buffer":{
                     "data":[ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 ]
                  }
               }
            }
         ]
      }
   }
}
    "#;

#[test]
fn test_extract_estimate_works() {
    let transaction: StacksTransaction = serde_json::from_str(example_transaction_json).unwrap();
    let mut transaction_bytes = vec![];
    transaction
        .consensus_serialize(&mut transaction_bytes).expect("Could not deserialize.");

    let final_size = transaction_bytes.len();
    assert_eq!(274, final_size);
    let estimated_size = transaction.payload.serialize_to_vec().len();
    assert_eq!(159, estimated_size);

    assert_eq!(Ok(115), calculate_fee_rate_adjustment(&transaction, 0, 1.0, 1.0));
    assert_eq!(Ok(230), calculate_fee_rate_adjustment(&transaction, 0, 2.0, 1.0));
    assert_eq!(Ok(460), calculate_fee_rate_adjustment(&transaction, 0, 2.0, 2.0));

    assert_eq!(Ok(125), calculate_fee_rate_adjustment(&transaction, 10, 1.0, 1.0));
    assert_eq!(Ok(240), calculate_fee_rate_adjustment(&transaction, 10, 2.0, 1.0));
    assert_eq!(Ok(470), calculate_fee_rate_adjustment(&transaction, 10, 2.0, 2.0));
}

/// Make a response with `num_estimations` estimations, where the i'th element has `fee_rate = fee = i`.
fn make_dummy_response_with_num_estimations(num_estimations:u64) -> RPCFeeEstimateResponse {
    let mut estimations = vec![];
    for i in 1..(num_estimations + 1) {
        estimations.push(
            RPCFeeEstimate {
                fee_rate: i as f64,
                fee: i,
            }
        );
    }
    RPCFeeEstimateResponse {
        estimated_cost: ExecutionCost {
            write_length: 0,
            write_count: 0,
            read_length: 28084,
            read_count: 7,
            runtime: 28429000,
        },
        estimated_cost_scalar: 6,
        estimations,
        cost_scalar_change_by_byte: 1.0,
    }
}
#[test]
/// If there are 0 estimates, it's an error.
fn test_extract_estimate_fails_no_estimates() {
    let transaction: StacksTransaction = serde_json::from_str(example_transaction_json).unwrap();
    assert_eq!(
        Err(FeeCalculationError::NoEstimatesReturned),
        compute_fee_from_response_and_transaction(&transaction, &Ok(make_dummy_response_with_num_estimations(0)))
        );
}

#[test]
/// Try different numbers of estimates to see that each one works.
fn test_extract_estimate_fails_works_many_estimates() {
    let transaction: StacksTransaction = serde_json::from_str(example_transaction_json).unwrap();

    // Answer in each case is `fee + size_delta * fee_rate`, where `fee = fee_rate = num_estimations`.
    assert_eq!(
        Ok(116),
        compute_fee_from_response_and_transaction(&transaction, &Ok(make_dummy_response_with_num_estimations(1)))
    );
    assert_eq!(
        Ok(232),
        compute_fee_from_response_and_transaction(&transaction, &Ok(make_dummy_response_with_num_estimations(2)))
    );
    assert_eq!(
        Ok(232),
        compute_fee_from_response_and_transaction(&transaction, &Ok(make_dummy_response_with_num_estimations(3)))
    );
    assert_eq!(
        Ok(348),
        compute_fee_from_response_and_transaction(&transaction, &Ok(make_dummy_response_with_num_estimations(4)))
    );
}
