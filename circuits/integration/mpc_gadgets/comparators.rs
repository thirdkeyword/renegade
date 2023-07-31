//! Groups integration tests for comparators

use std::slice::Concat;

use bigdecimal::num_traits::NumOps;
use circuits::mpc_gadgets::comparators::{
    cond_select, cond_select_vec, eq, eq_zero, greater_than, greater_than_equal, kary_or,
    less_than, less_than_equal,
};
use crypto::fields::scalar_to_u64;
use itertools::Itertools;
use mpc_stark::{
    algebra::{authenticated_scalar::AuthenticatedScalarResult, scalar::Scalar},
    PARTY0, PARTY1,
};
use rand::{seq::SliceRandom, thread_rng, Rng, RngCore};
use test_helpers::{
    mpc_network::{await_result, await_result_batch_with_error, await_result_with_error},
    types::IntegrationTest,
};

use crate::{IntegrationTestArgs, TestWrapper};

use super::{assert_scalar_batch_eq, assert_scalar_eq};

/// Tests all the inequality comparators
fn test_inequalities(test_args: &IntegrationTestArgs) -> Result<(), String> {
    // Do not use all bits to avoid overflow, for the sake of testing this is okay
    let fabric = &test_args.mpc_fabric;
    let my_random_value = (thread_rng().next_u32() / 4) as u64;

    let shared_a = fabric.share_scalar(my_random_value, PARTY0);
    let shared_b = fabric.share_scalar(my_random_value, PARTY1);

    let opened_a = scalar_to_u64(&await_result(shared_a.open()));
    let opened_b = scalar_to_u64(&await_result(shared_b.open()));

    // Test <
    let lt_result = await_result_with_error(
        less_than::<250>(&shared_a, &shared_b, test_args.mpc_fabric.clone()).open_authenticated(),
    )?;
    let mut expected_result = opened_a < opened_b;

    assert_scalar_eq(&lt_result, &expected_result.into())?;

    // Test <= with equal values
    let mut lte_result = await_result_with_error(
        less_than_equal::<250>(&shared_a, &shared_a, test_args.mpc_fabric.clone())
            .open_authenticated(),
    )?;
    assert_scalar_eq(&lte_result, &Scalar::one())?;

    // Test <= with random values
    lte_result = await_result_with_error(
        less_than_equal::<250>(&shared_a, &shared_b, test_args.mpc_fabric.clone())
            .open_authenticated(),
    )?;
    expected_result = opened_a <= opened_b;
    assert_scalar_eq(&lte_result, &expected_result.into())?;

    // Test >
    let gt_result = await_result_with_error(
        greater_than::<250>(&shared_a, &shared_b, test_args.mpc_fabric.clone())
            .open_authenticated(),
    )?;
    expected_result = opened_a > opened_b;
    assert_scalar_eq(&gt_result, &expected_result.into())?;

    // Test >= with equal values
    let gte_result = await_result_with_error(
        greater_than_equal::<250>(&shared_a, &shared_b, test_args.mpc_fabric.clone())
            .open_authenticated(),
    )?;
    expected_result = opened_a >= opened_b;
    assert_scalar_eq(&gte_result, &expected_result.into())?;

    Ok(())
}

/// Tests the equality comparators
fn test_equalities(test_args: &IntegrationTestArgs) -> Result<(), String> {
    let fabric = &test_args.mpc_fabric;
    // 0 == 0
    let shared_zero = fabric.zero_authenticated();
    let mut res = await_result_with_error(
        eq_zero::<250>(&shared_zero, test_args.mpc_fabric.clone()).open_authenticated(),
    )?;

    assert_scalar_eq(&res, &Scalar::one())?;

    // random == 0
    let mut rng = thread_rng();
    let shared_random = fabric.share_scalar(rng.next_u32() as u64, PARTY0);
    res = await_result_with_error(
        eq_zero::<250>(&shared_random, test_args.mpc_fabric.clone()).open_authenticated(),
    )?;

    assert_scalar_eq(&res, &Scalar::zero())?;

    // random_1 == random_1
    let shared_random = fabric.share_scalar(rng.next_u32(), PARTY0);
    res = await_result_with_error(
        eq::<250>(&shared_random, &shared_random, test_args.mpc_fabric.clone())
            .open_authenticated(),
    )?;

    assert_scalar_eq(&res, &Scalar::one())?;

    // random_1 == random_2
    let shared_random1 = fabric.share_scalar(rng.next_u32(), PARTY0);
    let shared_random2 = fabric.share_scalar(rng.next_u32(), PARTY1);

    res = await_result_with_error(
        eq::<250>(
            &shared_random1,
            &shared_random2,
            test_args.mpc_fabric.clone(),
        )
        .open_authenticated(),
    )?;

    assert_scalar_eq(&res, &Scalar::zero())
}

/// Tests the k-ary or boolean operator
fn test_kary_or(test_args: &IntegrationTestArgs) -> Result<(), String> {
    const N: usize = 10;
    let fabric = &test_args.mpc_fabric;

    // All zeros
    let zeros: [AuthenticatedScalarResult; N] = fabric.zeros_authenticated(N).try_into().unwrap();
    let res = await_result_with_error(
        kary_or::<N>(&zeros, test_args.mpc_fabric.clone()).open_authenticated(),
    )?;

    assert_scalar_eq(&res, &Scalar::zero())?;

    // A random amount of ones
    let mut rng = thread_rng();
    let num_ones = rng.gen_range(1..N);
    let mut values = [
        vec![Scalar::one(); num_ones],
        vec![Scalar::zero(); N - num_ones],
    ]
    .concat();

    // Randomly permute the array and share with the counterparty
    values.shuffle(&mut rng);
    let shared_bits: [AuthenticatedScalarResult; N] = fabric
        .batch_share_scalar(values, PARTY0)
        .try_into()
        .unwrap();

    let res = await_result_with_error(
        kary_or(&shared_bits, test_args.mpc_fabric.clone()).open_authenticated(),
    )?;
    assert_scalar_eq(&res, &Scalar::one())
}

/// Tests the conditional select gadget
fn test_cond_select(test_args: &IntegrationTestArgs) -> Result<(), String> {
    let fabric = &test_args.mpc_fabric;
    let value1 = fabric.share_scalar(5, PARTY0);
    let value2 = fabric.share_scalar(10, PARTY1);

    // Select `value1`
    let res = await_result_with_error(
        cond_select(&fabric.one_authenticated(), &value1, &value2).open_authenticated(),
    )?;
    assert_scalar_eq(&res, &5.into())?;

    // Select `value2`
    let res = await_result_with_error(
        cond_select(&fabric.zero_authenticated(), &value1, &value2).open_authenticated(),
    )?;
    assert_scalar_eq(&res, &10.into())
}

/// Tests the conditional vector select gadget
fn test_cond_select_vector(test_args: &IntegrationTestArgs) -> Result<(), String> {
    let fabric = &test_args.mpc_fabric;
    let values1 = fabric.batch_share_scalar(vec![1, 2, 3], PARTY0);
    let values2 = fabric.batch_share_scalar(vec![4, 5, 6], PARTY1);

    // Select `values1`
    let res = cond_select_vec(&fabric.one_authenticated(), &values1, &values2);
    let res_open =
        await_result_batch_with_error(&AuthenticatedScalarResult::open_authenticated_batch(&res))?;
    assert_scalar_batch_eq(&res_open, &[1.into(), 2.into(), 3.into()]);

    // Select `values2`

    assert_scalar_batch_eq(&res_open, &[4, 5, 6])
}

inventory::submit!(TestWrapper(IntegrationTest {
    name: "mpc_gadgets::test_inequalities",
    test_fn: test_inequalities
}));

inventory::submit!(TestWrapper(IntegrationTest {
    name: "mpc_gadgets::test_equalities",
    test_fn: test_equalities
}));

inventory::submit!(TestWrapper(IntegrationTest {
    name: "mpc_gadgets::test_kary_or",
    test_fn: test_kary_or
}));

inventory::submit!(TestWrapper(IntegrationTest {
    name: "mpc_gadgets::test_cond_select",
    test_fn: test_cond_select,
}));

inventory::submit!(TestWrapper(IntegrationTest {
    name: "mpc_gadgets::test_cond_select_vector",
    test_fn: test_cond_select_vector
}));
