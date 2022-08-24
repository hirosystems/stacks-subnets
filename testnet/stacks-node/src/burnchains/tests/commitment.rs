use crate::burnchains::commitment::compute_fee_from_response;
use clarity::vm::costs::ExecutionCost;
use stacks::net::{RPCFeeEstimate, RPCFeeEstimateResponse};

#[test]
fn test_extract_estimate_works() {
    let response = RPCFeeEstimateResponse {
        estimated_cost: ExecutionCost {
            write_length: 0,
            write_count: 0,
            read_length: 28084,
            read_count: 7,
            runtime: 28429000,
        },
        estimated_cost_scalar: 6,
        estimations: vec![
            RPCFeeEstimate {
                fee_rate: 8434.552311435524,
                fee: 1,
            },
            RPCFeeEstimate {
                fee_rate: 8818.458333333334,
                fee: 2,
            },
            RPCFeeEstimate {
                fee_rate: 8818.458333333334,
                fee: 3,
            },
        ],
        cost_scalar_change_by_byte: 0.00476837158203125,
    };

    let computed = compute_fee_from_response(&Ok(response));
    assert_eq!(Some(2), computed);
}

#[test]
/// If there aren't 3 estimates, return None.
fn test_extract_estimate_fails_no_estimates() {
    let response = RPCFeeEstimateResponse {
        estimated_cost: ExecutionCost {
            write_length: 0,
            write_count: 0,
            read_length: 28084,
            read_count: 7,
            runtime: 28429000,
        },
        estimated_cost_scalar: 6,
        estimations: vec![],
        cost_scalar_change_by_byte: 0.00476837158203125,
    };

    let computed = compute_fee_from_response(&Ok(response));
    assert_eq!(None, computed);
}

#[test]
/// If there is more than 3 estimates, still use the second.
fn test_extract_estimate_fails_works_many_estimates() {
    let response = RPCFeeEstimateResponse {
        estimated_cost: ExecutionCost {
            write_length: 0,
            write_count: 0,
            read_length: 28084,
            read_count: 7,
            runtime: 28429000,
        },
        estimated_cost_scalar: 6,
        estimations: vec![
            RPCFeeEstimate {
                fee_rate: 8434.552311435524,
                fee: 1,
            },
            RPCFeeEstimate {
                fee_rate: 8818.458333333334,
                fee: 2,
            },
            RPCFeeEstimate {
                fee_rate: 8818.458333333334,
                fee: 3,
            },
            RPCFeeEstimate {
                fee_rate: 8818.458333333334,
                fee: 4,
            },
        ],
        cost_scalar_change_by_byte: 0.00476837158203125,
    };

    let computed = compute_fee_from_response(&Ok(response));
    assert_eq!(Some(2), computed);
}
