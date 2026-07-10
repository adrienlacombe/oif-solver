use alexandria_bytes::{Bytes, BytesTrait, BytesStore};
use core::num::traits::Bounded;
use core::keccak::compute_keccak_byte_array;
use snforge_std::signature::stark_curve::{
    StarkCurveKeyPairImpl, StarkCurveSignerImpl, StarkCurveVerifierImpl,
};
use crate::common::{pop_event};
use permit2::snip12_utils::permits::{TokenPermissionsStructHash, U256StructHash};
use openzeppelin_utils::cryptography::snip12::{SNIP12HashSpanImpl};
use oif_starknet::base7683::{SpanFelt252StructHash, ArrayFelt252StructHash, Base7683Component};
use oif_starknet::erc7683::interface::{
    Base7683ABIDispatcher, Base7683ABIDispatcherTrait, FilledOrder, GaslessCrossChainOrder, Open,
};
use oif_starknet::libraries::order_encoder::{BytesDefault};
use openzeppelin_token::erc20::interface::{IERC20DispatcherTrait};
use snforge_std::{
    start_cheat_caller_address, start_cheat_block_timestamp_global,
    stop_cheat_block_timestamp_global, EventSpyAssertionsTrait, stop_cheat_caller_address,
    spy_events, EventSpyTrait,
};
use crate::mocks::mock_base7683::{IMockBase7683DispatcherTrait};
use crate::base_test::{
    setup as super_setup, Setup, _prepare_onchain_order, _balances, _assert_open_order,
    _assert_resolved_order, _get_signature,
};
use crate::common::{deploy_mock_base7683};

pub fn setup() -> Setup {
    let mut setup = super_setup();

    let base = deploy_mock_base7683(
        setup.permit2,
        setup.origin,
        setup.destination,
        setup.input_token.contract_address,
        setup.output_token.contract_address,
    );
    let base_full = Base7683ABIDispatcher { contract_address: base.contract_address };

    setup.base_full = base_full;
    setup.base = base;

    setup.users.append(base.contract_address);

    setup
}

pub fn _prepare_gasless_order(
    order_data: Bytes,
    nonce: felt252,
    open_deadline: u64,
    fill_deadline: u64,
    order_data_type: u256,
    setup: Setup,
) -> GaslessCrossChainOrder {
    GaslessCrossChainOrder {
        origin_settler: setup.base.contract_address,
        user: setup.kaka.account.contract_address,
        origin_chain_id: setup.origin,
        order_data,
        nonce,
        open_deadline,
        fill_deadline,
        order_data_type,
    }
}

#[test]
#[fuzzer]
fn test_open_works(fill_deadline: u64) {
    let setup = setup();
    let order_data: Bytes = Into::<ByteArray, Bytes>::into("some order data");
    let order_type: u256 = 'some order type'.into();
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, order_type);

    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    setup.input_token.approve(setup.base.contract_address, setup.amount);
    stop_cheat_caller_address(setup.input_token.contract_address);

    start_cheat_caller_address(setup.base.contract_address, setup.kaka.account.contract_address);
    assert(
        setup.base_full.is_valid_nonce(setup.kaka.account.contract_address, 1),
        'Nonce is not valid',
    );
    let balances_before = _balances(setup.input_token, setup.users.clone().into());

    /// Open order and catch event
    let mut spy = spy_events();
    setup.base_full.open(order);
    let Open {
        order_id, resolved_order,
    } =
        pop_event::<Open>(setup.base.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    _assert_resolved_order(
        resolved_order,
        order_data.clone(),
        setup.kaka.account.contract_address,
        fill_deadline,
        Bounded::<u64>::MAX,
        setup.base.counterpart(),
        setup.base.counterpart(),
        setup.base.local_domain(),
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );

    _assert_open_order(
        order_id,
        setup.kaka.account.contract_address,
        order_data,
        balances_before,
        setup.kaka.account.contract_address,
        setup.clone(),
    );

    stop_cheat_caller_address(setup.base.contract_address);
}


#[test]
#[fuzzer]
#[should_panic(expected: 'Invalid nonce')]
fn test_open_INVALID_NONCE(fill_deadline: u64) {
    let setup = setup();
    let order_data: Bytes = Into::<ByteArray, Bytes>::into("some order data");
    let order_type: u256 = 'some order type'.into();
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, order_type);

    start_cheat_caller_address(setup.base.contract_address, setup.kaka.account.contract_address);
    setup.base_full.invalidate_nonces(1);
    setup.base_full.open(order);
    stop_cheat_caller_address(setup.input_token.contract_address);
}

#[test]
#[fuzzer]
fn test_open_for_works(mut open_deadline: u64, fill_deadline: u64) {
    let setup = setup();

    // Assume block.timestamp < open_deadline
    start_cheat_block_timestamp_global(open_deadline - 1);

    // Approve base to spend input token
    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    setup.input_token.approve(setup.permit2, Bounded::<u256>::MAX);
    stop_cheat_caller_address(setup.input_token.contract_address);

    let nonce = 0;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_data_type = 'some order data';
    let order = _prepare_gasless_order(
        order_data.clone(), nonce, open_deadline, fill_deadline, order_data_type, setup.clone(),
    );
    let witness = setup
        .base_full
        .witness_hash(setup.base_full.resolve_for(order.clone(), Default::default()));
    let sig = _get_signature(
        setup.kaka,
        setup.base.contract_address,
        witness,
        setup.input_token.contract_address,
        nonce,
        open_deadline,
        setup.clone(),
    );
    let balances_before = _balances(setup.input_token, setup.users.clone().into());

    assert(
        setup.base_full.is_valid_nonce(setup.kaka.account.contract_address, nonce),
        'Nonce shd be valid',
    );

    /// Open order and catch event
    let mut spy = spy_events();
    start_cheat_caller_address(setup.base.contract_address, setup.karp.account.contract_address);
    setup.base_full.open_for(order, sig, Default::default());
    let Open {
        order_id, resolved_order,
    } =
        pop_event::<Open>(setup.base.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    _assert_resolved_order(
        resolved_order,
        order_data.clone(),
        setup.kaka.account.contract_address,
        fill_deadline,
        open_deadline,
        setup.base.counterpart(),
        setup.base.counterpart(),
        setup.base.local_domain(),
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );

    _assert_open_order(
        order_id,
        setup.kaka.account.contract_address,
        order_data,
        balances_before,
        setup.kaka.account.contract_address,
        setup.clone(),
    );

    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[fuzzer]
#[should_panic(expected: 'Order open expired')]
fn test_open_for_ORDER_OPEN_EXPIRED(mut open_deadline: u64, fill_deadline: u64) {
    let setup = setup();

    // Assume block.timestamp > open_deadline
    start_cheat_block_timestamp_global(open_deadline + 1);
    let nonce = 0;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_data_type = 'some order data';
    let order = _prepare_gasless_order(
        order_data.clone(), nonce, open_deadline, fill_deadline, order_data_type, setup.clone(),
    );
    let sig: Array<felt252> = array![];
    let origin_filler_data: Bytes = Default::default();

    /// Open order and catch event
    start_cheat_caller_address(setup.base.contract_address, setup.karp.account.contract_address);
    setup.base_full.open_for(order, sig, origin_filler_data);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[fuzzer]
#[should_panic(expected: 'Invalid gasless order settler')]
fn test_open_for_INVALID_GASLESS_ORDER_SETTLER(mut open_deadline: u64, fill_deadline: u64) {
    let setup = setup();

    // Assume block.timestamp < open_deadline
    start_cheat_block_timestamp_global(open_deadline - 1);
    let nonce = 0;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_data_type = 'some order data';
    let mut order = _prepare_gasless_order(
        order_data.clone(), nonce, open_deadline, fill_deadline, order_data_type, setup.clone(),
    );
    order.origin_settler = 'other'.try_into().unwrap();
    let sig: Array<felt252> = array![];
    let origin_filler_data: Bytes = Default::default();

    /// Open order and catch event
    start_cheat_caller_address(setup.base.contract_address, setup.karp.account.contract_address);
    setup.base_full.open_for(order, sig, origin_filler_data);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[fuzzer]
#[should_panic(expected: 'Invalid gasless order origin')]
fn test_open_for_INVALID_GASLESS_ORDER_ORIGIN(mut open_deadline: u64, fill_deadline: u64) {
    let setup = setup();

    // Assume block.timestamp < open_deadline
    start_cheat_block_timestamp_global(open_deadline - 1);
    let nonce = 0;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_data_type = 'some order data';
    let mut order = _prepare_gasless_order(
        order_data.clone(), nonce, open_deadline, fill_deadline, order_data_type, setup.clone(),
    );
    order.origin_chain_id = 3;
    let sig: Array<felt252> = array![];
    let origin_filler_data: Bytes = Default::default();

    /// Open order and catch event
    start_cheat_caller_address(setup.base.contract_address, setup.karp.account.contract_address);
    setup.base_full.open_for(order, sig, origin_filler_data);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[fuzzer]
#[should_panic(expected: 'Invalid nonce')]
fn test_open_for_INVALID_NONCE(mut open_deadline: u64, fill_deadline: u64) {
    let setup = setup();

    // Assume block.timestamp < open_deadline
    start_cheat_block_timestamp_global(open_deadline - 1);
    let nonce = 0;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_data_type = 'some order data';
    let mut order = _prepare_gasless_order(
        order_data.clone(), nonce, open_deadline, fill_deadline, order_data_type, setup.clone(),
    );
    start_cheat_caller_address(setup.base.contract_address, setup.kaka.account.contract_address);
    setup.base_full.invalidate_nonces(1);
    stop_cheat_caller_address(setup.base.contract_address);
    let sig: Array<felt252> = array![];
    let origin_filler_data: Bytes = Default::default();

    /// Open order and catch event
    start_cheat_caller_address(setup.base.contract_address, setup.karp.account.contract_address);
    setup.base_full.open_for(order, sig, origin_filler_data);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[fuzzer]
fn test_resolve_works(fill_deadline: u64) {
    let setup = setup();
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_data_type = 'some order data';
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, order_data_type);

    start_cheat_caller_address(setup.base.contract_address, setup.kaka.account.contract_address);
    let resolved_order = setup.base_full.resolve(order.clone());

    _assert_resolved_order(
        resolved_order,
        order_data.clone(),
        setup.kaka.account.contract_address,
        fill_deadline,
        Bounded::<u64>::MAX,
        setup.base.counterpart(),
        setup.base.counterpart(),
        setup.base.local_domain(),
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );

    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
#[fuzzer]
fn test_resolve_for_works(mut open_deadline: u64, fill_deadline: u64) {
    let setup = setup();
    let nonce = 0;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_data_type = 'some order data';
    let order = _prepare_gasless_order(
        order_data.clone(), nonce, open_deadline, fill_deadline, order_data_type, setup.clone(),
    );
    let origin_filler_data: Bytes = Default::default();

    start_cheat_caller_address(setup.base.contract_address, setup.karp.account.contract_address);
    let resolved_order = setup.base_full.resolve_for(order.clone(), origin_filler_data.clone());

    _assert_resolved_order(
        resolved_order,
        order_data,
        setup.kaka.account.contract_address,
        fill_deadline,
        open_deadline,
        setup.base.counterpart(),
        setup.base.counterpart(),
        setup.base.local_domain(),
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );
    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
fn test_fill_works() {
    let setup = setup();

    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_id = 'someId'.into();
    let mut filler_data: Bytes = BytesTrait::new_empty();
    filler_data.append_address(setup.veg.account.contract_address);

    let mut spy = spy_events();

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    setup.base_full.fill(order_id, order_data.clone(), filler_data.clone());
    let FilledOrder {
        origin_data: _origin_data, filler_data: _filler_data,
    } = setup.base_full.filled_orders(order_id);

    spy
        .assert_emitted(
            @array![
                (
                    setup.base.contract_address,
                    Base7683Component::Event::Filled(
                        Base7683Component::Filled {
                            order_id,
                            origin_data: order_data.clone(),
                            filler_data: filler_data.clone(),
                        },
                    ),
                ),
            ],
        );

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.FILLED());

    assert(_origin_data == order_data, 'Origin data does not match');
    assert(_filler_data == filler_data, 'Filler data does not match');

    assert_eq!(setup.base.filled_id(), order_id);
    assert(setup.base.filled_origin_data() == order_data, 'Origin data does not match');
    assert(setup.base.filled_filler_data() == filler_data, 'Filler data does not match');

    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
#[should_panic(expected: 'Invalid order status')]
fn test_fill_INVALID_ORDER_STATUS_FILLED() {
    let setup = setup();
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_id = 'someId'.into();
    let mut filler_data: Bytes = BytesTrait::new_empty();
    filler_data.append_address(setup.veg.account.contract_address);

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);

    // Try to fill the order a second time
    setup.base_full.fill(order_id, order_data.clone(), filler_data.clone());
    setup.base_full.fill(order_id, order_data.clone(), filler_data.clone());

    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
#[fuzzer]
#[should_panic(expected: 'Invalid order status')]
fn test_fill_INVALID_ORDER_STATUS_OPENED(fill_deadline: u64) {
    let setup = setup();
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order_id = 'someId'.into();
    let order_type: u256 = 'some order type'.into();
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, order_type);

    // Open order
    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    setup.input_token.approve(setup.base.contract_address, setup.amount);
    stop_cheat_caller_address(setup.input_token.contract_address);

    start_cheat_caller_address(setup.base.contract_address, setup.kaka.account.contract_address);
    setup.base_full.open(order); // fails
    stop_cheat_caller_address(setup.base.contract_address);

    // Try to fill open order
    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    let mut filler_data: Bytes = BytesTrait::new_empty();
    filler_data.append_address(setup.veg.account.contract_address);
    setup.base_full.fill(order_id, order_data.clone(), filler_data.clone());
    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
fn test_settle_works() {
    let setup = setup();
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let filler_data = Into::<ByteArray, Bytes>::into("some filler data");
    let order_id = 'someOrderId';

    // Fill order, then settle order
    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    setup.base_full.fill(order_id, order_data.clone(), filler_data.clone());

    let order_ids = array![order_id];
    let orders_filler_data = array![filler_data.clone()];

    let mut spy = spy_events();
    setup.base_full.settle(order_ids.clone(), 0);
    spy
        .assert_emitted(
            @array![
                (
                    setup.base.contract_address,
                    Base7683Component::Event::Settle(
                        Base7683Component::Settle {
                            order_ids: order_ids.clone(), orders_filler_data,
                        },
                    ),
                ),
            ],
        );

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.FILLED());
    assert_eq!(setup.base.settled_order_ids()[0], @order_id);
    assert(setup.base.settled_orders_origin_data()[0] == @order_data, 'Order data incorrect');
    assert(setup.base.settled_orders_filler_data()[0] == @filler_data, 'Filler data incorrect');

    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
fn test_settle_multiple_works() {
    let setup = setup();
    let order_data1 = Into::<ByteArray, Bytes>::into("some order data 1");
    let filler_data1 = Into::<ByteArray, Bytes>::into("some order data 1");
    let order_id1 = 'someOrderId 1'.into();
    let order_data2 = Into::<ByteArray, Bytes>::into("some order data 2");
    let filler_data2 = Into::<ByteArray, Bytes>::into("some order data 2");
    let order_id2 = 'someOrderId 2'.into();

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    setup.base_full.fill(order_id1, order_data1.clone(), filler_data1.clone());
    setup.base_full.fill(order_id2, order_data2.clone(), filler_data2.clone());

    let order_ids = array![order_id1, order_id2];
    let orders_filler_data = array![filler_data1.clone(), filler_data2.clone()];

    let mut spy = spy_events();
    setup.base_full.settle(order_ids.clone(), 0);
    spy
        .assert_emitted(
            @array![
                (
                    setup.base.contract_address,
                    Base7683Component::Event::Settle(
                        Base7683Component::Settle {
                            order_ids: order_ids.clone(),
                            orders_filler_data: orders_filler_data.clone(),
                        },
                    ),
                ),
            ],
        );

    assert_eq!(setup.base_full.order_status(order_id1), setup.base_full.FILLED());
    assert_eq!(*setup.base.settled_order_ids()[0], order_id1);
    assert(setup.base.settled_orders_origin_data()[0] == @order_data1, 'Order data incorrect 1');
    assert(setup.base.settled_orders_filler_data()[0] == @filler_data1, 'Filler data incorrect 1');

    assert_eq!(setup.base_full.order_status(order_id2), setup.base_full.FILLED());
    assert_eq!(*setup.base.settled_order_ids()[1], order_id2);
    assert(setup.base.settled_orders_origin_data()[1] == @order_data2, 'Order data incorrect 2');
    assert(setup.base.settled_orders_filler_data()[1] == @filler_data2, 'Filler data incorrect 2');

    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
#[should_panic(expected: 'Invalid order status')]
fn test_settle_INVALID_ORDER_STATUS() {
    let setup = setup();
    let order_id = 'someOrderId'.into();

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    let order_ids = array![order_id];
    setup.base_full.settle(order_ids, 0);
    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
fn test_refund_onChain_works() {
    let setup = setup();
    start_cheat_block_timestamp_global(123);
    let fill_deadline = starknet::get_block_timestamp() - 1;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, 'someOrderType');
    let orders = array![order];
    let order_id = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data.clone()));
    let order_ids = array![order_id];

    let mut spy = spy_events();
    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    setup.base_full.refund_onchain_cross_chain_order(orders, 0);
    spy
        .assert_emitted(
            @array![
                (
                    setup.base.contract_address,
                    Base7683Component::Event::Refund(
                        Base7683Component::Refund { order_ids: order_ids.clone() },
                    ),
                ),
            ],
        );

    assert_eq!(
        setup.base_full.order_status(order_id), setup.base_full.UNKNOWN(),
    ); // refunding does not change the status
    assert_eq!(*setup.base.refunded_order_ids()[0], order_id);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
fn test_refund_multi_onChain_works() {
    let setup = setup();
    start_cheat_block_timestamp_global(123);
    let fill_deadline = starknet::get_block_timestamp() - 1;
    let order_data = Into::<ByteArray, Bytes>::into("some order data 1");
    let order_data2 = Into::<ByteArray, Bytes>::into("some order data 2");
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, 'someOrderType');
    let order2 = _prepare_onchain_order(order_data2.clone(), fill_deadline, 'someOrderType');
    let orders = array![order, order2];
    let order_id = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data.clone()));
    let order_id2 = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data2.clone()));
    let order_ids = array![order_id, order_id2];

    let mut spy = spy_events();
    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    setup.base_full.refund_onchain_cross_chain_order(orders, 0);
    spy
        .assert_emitted(
            @array![
                (
                    setup.base.contract_address,
                    Base7683Component::Event::Refund(
                        Base7683Component::Refund { order_ids: order_ids.clone() },
                    ),
                ),
            ],
        );

    assert_eq!(
        setup.base_full.order_status(order_id), setup.base_full.UNKNOWN(),
    ); // refunding does not change the status
    assert_eq!(*setup.base.refunded_order_ids()[0], order_id);
    assert_eq!(
        setup.base_full.order_status(order_id2), setup.base_full.UNKNOWN(),
    ); // refunding does not change the status
    assert_eq!(*setup.base.refunded_order_ids()[1], order_id2);

    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[should_panic(expected: 'Invalid order status')]
fn test_refund_onChain_INVALID_ORDER_STATUS() {
    let setup = setup();
    start_cheat_block_timestamp_global(123);
    let fill_deadline = starknet::get_block_timestamp() - 1;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, 'someOrderType');
    let order_id = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data.clone()));
    let filler_data = Into::<ByteArray, Bytes>::into("some filler data");

    setup.base_full.fill(order_id, order_data.clone(), filler_data);

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    let orders = array![order];
    setup.base_full.refund_onchain_cross_chain_order(orders, 0);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[should_panic(expected: 'Order fill not expired')]
fn test_refund_onChain_ORDER_FILL_NOT_EXPIRED() {
    let setup = setup();
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let fill_deadline = starknet::get_block_timestamp() + 1;
    let order = _prepare_onchain_order(order_data.clone(), fill_deadline, 'someOrderType');

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    let orders = array![order];
    setup.base_full.refund_onchain_cross_chain_order(orders, 0);
    stop_cheat_caller_address(setup.base.contract_address);
}

#[test]
fn test_refund_gasless_works() {
    let setup = setup();
    let permit_nonce = 0;
    start_cheat_block_timestamp_global(123);
    let fill_deadline = starknet::get_block_timestamp() - 1;
    let open_deadline = starknet::get_block_timestamp() - 10;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order = _prepare_gasless_order(
        order_data.clone(),
        permit_nonce,
        open_deadline,
        fill_deadline,
        'someOrderType',
        setup.clone(),
    );
    let orders = array![order];
    let order_id = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data.clone()));
    let order_ids = array![order_id];
    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);

    let mut spy = spy_events();
    setup.base_full.refund_gasless_cross_chain_order(orders, 0);
    spy
        .assert_emitted(
            @array![
                (
                    setup.base.contract_address,
                    Base7683Component::Event::Refund(
                        Base7683Component::Refund { order_ids: order_ids.clone() },
                    ),
                ),
            ],
        );

    assert_eq!(
        setup.base_full.order_status(order_id), setup.base_full.UNKNOWN(),
    ); // refunding does not change the status
    assert_eq!(*setup.base.refunded_order_ids()[0], order_id);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
fn test_refund_multi_gasless_work() {
    let setup = setup();
    let permit_nonce = 0;
    start_cheat_block_timestamp_global(123);
    let fill_deadline = starknet::get_block_timestamp() - 1;
    let open_deadline = starknet::get_block_timestamp() - 10;
    let order_data1 = Into::<ByteArray, Bytes>::into("some order data 1");
    let order_id1 = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data1.clone()));
    let order1 = _prepare_gasless_order(
        order_data1.clone(),
        permit_nonce,
        open_deadline,
        fill_deadline,
        'someOrderType',
        setup.clone(),
    );
    let order_data2 = Into::<ByteArray, Bytes>::into("some order data2");
    let order_id2 = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data2.clone()));
    let order2 = _prepare_gasless_order(
        order_data2.clone(),
        permit_nonce,
        open_deadline,
        fill_deadline,
        'someOrderType',
        setup.clone(),
    );
    let orders = array![order1, order2];
    let order_ids = array![order_id1, order_id2];

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    let mut spy = spy_events();
    setup.base_full.refund_gasless_cross_chain_order(orders, 0);

    spy
        .assert_emitted(
            @array![
                (
                    setup.base.contract_address,
                    Base7683Component::Event::Refund(
                        Base7683Component::Refund { order_ids: order_ids.clone() },
                    ),
                ),
            ],
        );

    assert_eq!(
        setup.base_full.order_status(order_id1), setup.base_full.UNKNOWN(),
    ); // refunding does not change the status
    assert_eq!(*setup.base.refunded_order_ids()[0], order_id1);

    assert_eq!(
        setup.base_full.order_status(order_id2), setup.base_full.UNKNOWN(),
    ); // refunding does not change the status
    assert_eq!(*setup.base.refunded_order_ids()[1], order_id2);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[should_panic(expected: 'Invalid order status')]
fn test_refund_gasless_INVALID_ORDER_STATUS() {
    let setup = setup();
    let permit_nonce = 0;
    start_cheat_block_timestamp_global(123);
    let fill_deadline = starknet::get_block_timestamp() - 1;
    let open_deadline = starknet::get_block_timestamp() - 10;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order = _prepare_gasless_order(
        order_data.clone(),
        permit_nonce,
        open_deadline,
        fill_deadline,
        'someOrderType',
        setup.clone(),
    );
    let order_id = compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order_data.clone()));
    let filler_data = Into::<ByteArray, Bytes>::into("some filler data");

    setup.base_full.fill(order_id, order_data.clone(), filler_data);

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    let orders = array![order];
    setup.base_full.refund_gasless_cross_chain_order(orders, 0);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

#[test]
#[should_panic(expected: 'Order fill not expired')]
fn test_refund_gasless_ORDER_FILL_NOT_EXPIRED() {
    let setup = setup();
    let permit_nonce = 0;
    start_cheat_block_timestamp_global(123);
    let fill_deadline = starknet::get_block_timestamp() + 2;
    let open_deadline = starknet::get_block_timestamp() - 10;
    let order_data = Into::<ByteArray, Bytes>::into("some order data");
    let order = _prepare_gasless_order(
        order_data.clone(),
        permit_nonce,
        open_deadline,
        fill_deadline,
        'someOrderType',
        setup.clone(),
    );
    let orders = array![order];

    start_cheat_caller_address(setup.base.contract_address, setup.veg.account.contract_address);
    setup.base_full.refund_gasless_cross_chain_order(orders, 0);
    stop_cheat_caller_address(setup.base.contract_address);
    stop_cheat_block_timestamp_global();
}

