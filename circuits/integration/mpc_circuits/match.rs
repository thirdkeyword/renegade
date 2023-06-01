//! Groups integration tests for the match circuitry

use circuits::{
    mpc_circuits::r#match::compute_match,
    traits::{BaseType, LinkableBaseType, MpcBaseType, MpcType, MultiproverCircuitBaseType},
    types::{
        balance::Balance,
        order::{AuthenticatedOrder, Order},
        r#match::MatchResult,
    },
    zk_circuits::valid_match_mpc::{AuthenticatedValidMatchMpcWitness, ValidMatchMpcCircuit},
    zk_gadgets::fixed_point::FixedPoint,
};
use curve25519_dalek::scalar::Scalar;
use integration_helpers::types::IntegrationTest;
use merlin::Transcript;
use mpc_bulletproof::{r1cs_mpc::MpcProver, PedersenGens};
use num_bigint::BigUint;
use rand_core::OsRng;

use crate::{IntegrationTestArgs, TestWrapper};

// --------------
// | Test Cases |
// --------------

/// Tests the match function with non overlapping orders for a variety of failure cases
fn test_match_no_match(test_args: &IntegrationTestArgs) -> Result<(), String> {
    // Convenience selector for brevity
    let mut rng = OsRng {};
    let party_id = test_args.party_id;
    macro_rules! sel {
        ($a:expr, $b:expr) => {
            if party_id == 0 {
                $a
            } else {
                $b
            }
        };
    }

    // Give a balance to each party and allocate it in the network
    let my_balance = sel!(
        Balance {
            mint: BigUint::from(1u8),
            amount: 200
        },
        Balance {
            mint: BigUint::from(2u8),
            amount: 200
        }
    )
    .to_linkable();

    let balance1 = my_balance
        .allocate(0 /* owning_party */, test_args.mpc_fabric.clone())
        .map_err(|err| format!("Error allocating balance1 in the network: {:?}", err))?;
    let balance2 = my_balance
        .allocate(1 /* owning_party */, test_args.mpc_fabric.clone())
        .map_err(|err| format!("Error allocating balance2 in the network: {:?}", err))?;

    // Build the test cases for different invalid match pairs
    let mut test_cases: Vec<Vec<u64>> = vec![
        // Quote mints different
        vec![
            sel!(0, 1),   /* quote_mint */
            2,            /* base_mint */
            sel!(0, 1),   /* side */
            sel!(20, 30), /* amount */
            10,           /* price */
        ],
        // Base mints different
        vec![
            1,            /* quote_mint */
            sel!(1, 2),   /* base_mint */
            sel!(0, 1),   /* side */
            sel!(20, 30), /* amount */
            10,           /* price */
        ],
        // Both orders on the same side (buy)
        vec![
            1,            /* quote_mint */
            2,            /* base_mint */
            0,            /* side (both buy) */
            sel!(20, 30), /* amount */
            10,           /* price */
        ],
        // Prices differ between orders
        vec![
            1,            /* quote_mint */
            2,            /* base_mint */
            sel!(0, 1),   /* side */
            sel!(20, 30), /* amount */
            sel!(5, 10),  /* price */
        ],
    ];

    let timestamp = 0;
    for case in test_cases.iter_mut() {
        // The price is the last field in the test case
        let my_price = case.pop().unwrap();
        case.push(timestamp);

        // Marshal into an order
        let my_order =
            Order::from_scalars(&mut case.iter().map(|x| Scalar::from(*x))).to_linkable();

        // Allocate the orders in the network
        let linkable_order1 = my_order
            .allocate(0 /* owning_party */, test_args.mpc_fabric.clone())
            .map_err(|err| format!("Error allocating order1 in the network: {:?}", err))?;
        let linkable_order2 = my_order
            .allocate(1 /* owning_party */, test_args.mpc_fabric.clone())
            .map_err(|err| format!("Error allocating order2 in the network: {:?}", err))?;

        // Allocate the price in the network
        let price1 = FixedPoint::from_integer(my_price)
            .allocate(0 /* owning_party */, test_args.mpc_fabric.clone())
            .map_err(|err| format!("Error allocating price in the network: {:?}", err))?;
        let price2 = FixedPoint::from_integer(my_price)
            .allocate(1 /* owning_party */, test_args.mpc_fabric.clone())
            .map_err(|err| format!("Error allocating price in the network: {:?}", err))?;

        let order1: AuthenticatedOrder<_, _> = AuthenticatedOrder::from_authenticated_scalars(
            &mut linkable_order1
                .clone()
                .to_authenticated_scalars()
                .into_iter(),
        );
        let order2: AuthenticatedOrder<_, _> = AuthenticatedOrder::from_authenticated_scalars(
            &mut linkable_order2
                .clone()
                .to_authenticated_scalars()
                .into_iter(),
        );

        // Compute matches
        let res = compute_match(
            &order1,
            &order2,
            &order1.amount,
            &order2.amount,
            &price1, // Use the first party's price
            test_args.mpc_fabric.clone(),
        )
        .map_err(|err| format!("Error computing order match: {:?}", err))?;

        // Assert that match verification fails
        let pc_gens = PedersenGens::default();
        let mut transcript = Transcript::new(b"test");
        let mut dummy_prover =
            MpcProver::new_with_fabric(test_args.mpc_fabric.clone().0, &mut transcript, &pc_gens);

        let witness = AuthenticatedValidMatchMpcWitness {
            order1: linkable_order1,
            amount1: order1.amount,
            price1: price1.clone(),
            order2: linkable_order2,
            amount2: order2.amount,
            price2: price2.clone(),
            balance1: balance1.clone(),
            balance2: balance2.clone(),
            match_res: res.link_commitments(test_args.mpc_fabric.clone()),
        };
        let (witness_var, _) = witness.commit_shared(&mut rng, &mut dummy_prover).unwrap();

        ValidMatchMpcCircuit::matching_engine_check(
            witness_var,
            test_args.mpc_fabric.clone(),
            &mut dummy_prover,
        )
        .unwrap();

        if dummy_prover.constraints_satisfied().unwrap() {
            return Err("Constraints satisfied".to_string());
        }
    }

    Ok(())
}

/// Tests that a valid match is found when one exists
fn test_match_valid_match(test_args: &IntegrationTestArgs) -> Result<(), String> {
    // Convenience selector for brevity, simpler to redefine per test than to
    // pass in party_id from the environment
    let party_id = test_args.party_id;
    macro_rules! sel {
        ($a:expr, $b:expr) => {
            if party_id == 0 {
                $a
            } else {
                $b
            }
        };
    }

    let mut test_cases: Vec<Vec<u64>> = vec![
        // Different amounts
        vec![
            1,            /* quote_mint */
            2,            /* base_mint */
            sel!(0, 1),   /* side */
            sel!(20, 30), /* amount */
            10,           /* price */
        ],
        // Same amount
        vec![
            1,          /* quote_mint */
            2,          /* base_mint */
            sel!(1, 0), /* side */
            15,         /* amount */
            10,         /* price */
        ],
    ];

    // Stores the expected result for each test case as a vector
    //      [party1_buy_mint, party1_buy_amount, party2_buy_mint, party2_buy_amount]
    let expected_results = vec![
        MatchResult {
            quote_mint: BigUint::from(1u8),
            base_mint: BigUint::from(2u8),
            quote_amount: 200,
            base_amount: 20,
            direction: 0,
            max_minus_min_amount: 10,
            min_amount_order_index: 0,
        },
        MatchResult {
            quote_mint: BigUint::from(1u8),
            base_mint: BigUint::from(2u8),
            quote_amount: 150,
            base_amount: 15,
            direction: 1,
            max_minus_min_amount: 0,
            min_amount_order_index: 1,
        },
    ];

    let timestamp = 0; // dummy value
    for (case, expected_res) in test_cases.iter_mut().zip(expected_results.iter()) {
        // Price is the last field in the test case
        let my_price = case.pop().unwrap();
        case.push(timestamp);

        // Marshal into an order
        let my_order = Order::from_scalars(&mut case.iter().map(|x| Scalar::from(*x)));

        // Allocate the prices in the network
        let price1 = FixedPoint::from_integer(my_price)
            .allocate(0 /* owning_party */, test_args.mpc_fabric.clone())
            .map_err(|err| format!("Error allocating price1 in the network: {:?}", err))?;

        // Allocate the orders in the network
        let order1 = my_order
            .allocate(0 /* owning_party */, test_args.mpc_fabric.clone())
            .map_err(|err| format!("Error allocating order1 in the network: {:?}", err))?;
        let order2 = my_order
            .allocate(1 /* owning_party */, test_args.mpc_fabric.clone())
            .map_err(|err| format!("Error allocating order2 in the network: {:?}", err))?;

        // Compute matches
        let res = compute_match(
            &order1,
            &order2,
            &order1.amount,
            &order2.amount,
            &price1,
            test_args.mpc_fabric.clone(),
        )
        .map_err(|err| format!("Error computing order match: {:?}", err))?
        .open_and_authenticate(test_args.mpc_fabric.clone())
        .map_err(|err| format!("Error opening match result: {:?}", err))?;

        // Assert that no match occurred
        if res != expected_res.clone() {
            return Err(format!(
                "Match result {:?} does not match expected result {:?}",
                res, expected_res
            ));
        }
    }

    Ok(())
}

// Take inventory
inventory::submit!(TestWrapper(IntegrationTest {
    name: "mpc_circuits::test_match_no_match",
    test_fn: test_match_no_match
}));

inventory::submit!(TestWrapper(IntegrationTest {
    name: "mpc_circuits::test_match_valid_match",
    test_fn: test_match_valid_match
}));
