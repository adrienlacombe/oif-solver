use alexandria_bytes::{Bytes, BytesTrait, BytesStore};
use crate::common::{deal, deploy_mock_basic_swap7683};
use core::num::traits::Bounded;
use snforge_std::signature::stark_curve::{
    StarkCurveKeyPairImpl, StarkCurveSignerImpl, StarkCurveVerifierImpl,
};
use permit2::snip12_utils::permits::{TokenPermissionsStructHash, U256StructHash};
use openzeppelin_utils::cryptography::snip12::{SNIP12HashSpanImpl};
use oif_starknet::libraries::order_encoder::{OrderData, OrderEncoder};
use oif_starknet::base7683::{SpanFelt252StructHash, ArrayFelt252StructHash};
use oif_starknet::erc7683::interface::{
    GaslessCrossChainOrder, Base7683ABIDispatcher, Base7683ABIDispatcherTrait,
};
use oif_starknet::basic_swap7683::BasicSwap7683Component;
use oif_starknet::libraries::order_encoder::{BytesDefault};
use openzeppelin_token::erc20::interface::{IERC20DispatcherTrait};
use starknet::ContractAddress;
use snforge_std::{
    start_cheat_caller_address, EventSpyAssertionsTrait, stop_cheat_caller_address, spy_events,
};
use crate::mocks::mock_basic_swap7683::IMockBasicSwap7683DispatcherTrait;
use crate::base_test::{
    _assert_resolved_order, Setup, setup as super_setup,
    _prepare_gasless_order as __prepare_gasless_order, _balances, _prepare_onchain_order,
};

fn setup() -> Setup {
    let mut setup = super_setup();
    let base_swap = deploy_mock_basic_swap7683(setup.permit2);
    let base_full = Base7683ABIDispatcher { contract_address: base_swap.contract_address };

    setup.base_full = base_full;
    setup.base_swap = base_swap;
    setup.wrong_msg_origin = 678.try_into().unwrap();
    setup.wrong_msg_sender = 'wrongMsgSender'.try_into().unwrap();
    setup.users.append(base_swap.contract_address);

    setup
}

fn _balance_id(user: ContractAddress, setup: Setup) -> usize {
    let kaka = setup.kaka.account.contract_address;
    let karp = setup.karp.account.contract_address;
    let veg = setup.veg.account.contract_address;
    let counter_part = setup.counterpart;
    let base = setup.base_swap.contract_address;

    if user == kaka {
        0
    } else if user == karp {
        1
    } else if user == veg {
        2
    } else if user == counter_part {
        3
    } else if user == base {
        4
    } else {
        999999999
    }
}

fn prepare_order_data(setup: Setup) -> OrderData {
    OrderData {
        sender: setup.kaka.account.contract_address,
        recipient: setup.karp.account.contract_address,
        input_token: setup.input_token.contract_address,
        output_token: setup.output_token.contract_address,
        amount_in: setup.amount,
        amount_out: setup.amount,
        sender_nonce: 1,
        origin_domain: setup.origin,
        destination_domain: setup.destination,
        destination_settler: setup.counterpart,
        fill_deadline: starknet::get_block_timestamp() + 100,
        data: BytesTrait::new_empty(),
    }
}

fn _prepare_gasless_order(
    setup: Setup, order_data: Bytes, permit_nonce: felt252, open_deadline: u64, fill_deadline: u64,
) -> GaslessCrossChainOrder {
    __prepare_gasless_order(
        setup.base_swap.contract_address,
        setup.kaka.account.contract_address,
        setup.origin,
        order_data,
        permit_nonce,
        open_deadline,
        fill_deadline,
        OrderEncoder::order_data_type_hash(),
    )
}

#[test]
fn test__settle_orders_works() {
    let setup = setup();
    let order_data1 = prepare_order_data(setup.clone());
    let mut order_data2 = prepare_order_data(setup.clone());
    order_data2.origin_domain = setup.destination;

    let order_ids: Array<u256> = array!['order1'.into(), 'order2'.into()];
    let orders_origin_data: Array<Bytes> = array![
        OrderEncoder::encode(@order_data1), OrderEncoder::encode(@order_data2),
    ];
    let orders_filler_data: Array<Bytes> = array![
        Into::<ByteArray, Bytes>::into("some filler data1"),
        Into::<ByteArray, Bytes>::into("some filler data2"),
    ];

    setup
        .base_swap
        .settle_orders(
            order_ids.clone(), orders_origin_data.clone(), orders_filler_data.clone(), 0,
        );

    assert_eq!(setup.base_swap.dispatched_origin_domain(), setup.origin);
    assert_eq!(setup.base_swap.dispatched_order_ids()[0], order_ids[0]);
    assert_eq!(setup.base_swap.dispatched_order_ids()[1], order_ids[1]);
    assert(
        setup.base_swap.dispatched_orders_filler_data()[0] == orders_filler_data[0],
        'Origin data mismatch 1',
    );
    assert(
        setup.base_swap.dispatched_orders_filler_data()[1] == orders_filler_data[1],
        'Origin data mismatch 2',
    );
}

#[test]
fn test__refund_orders_onchain_works() {
    let setup = setup();
    let order_data1 = prepare_order_data(setup.clone());
    let mut order_data2 = prepare_order_data(setup.clone());
    order_data2.origin_domain = setup.destination;

    let order1 = _prepare_onchain_order(
        OrderEncoder::encode(@order_data1),
        order_data1.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    let order2 = _prepare_onchain_order(
        OrderEncoder::encode(@order_data2),
        order_data2.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    let order_ids: Array<u256> = array!['order1'.into(), 'order2'.into()];
    let orders = array![order1.clone(), order2.clone()];
    setup.base_swap.refund_onchain_orders(orders.clone(), order_ids.clone(), 0);

    assert_eq!(setup.base_swap.dispatched_origin_domain(), setup.origin);
    assert_eq!(setup.base_swap.dispatched_order_ids()[0], order_ids[0]);
    assert_eq!(setup.base_swap.dispatched_order_ids()[1], order_ids[1]);
}

#[test]
fn test__refund_orders_gasless_works() {
    let setup = setup();
    let permit_nonce = 0;
    let order_data1 = prepare_order_data(setup.clone());
    let mut order_data2 = prepare_order_data(setup.clone());
    order_data2.origin_domain = setup.destination;

    let order1 = _prepare_gasless_order(
        setup.clone(), OrderEncoder::encode(@order_data1), permit_nonce, 0, 0,
    );

    let order2 = _prepare_gasless_order(
        setup.clone(), OrderEncoder::encode(@order_data2), permit_nonce, 0, 0,
    );

    let order_ids: Array<u256> = array!['order1'.into(), 'order2'.into()];
    let orders = array![order1.clone(), order2.clone()];

    setup.base_swap.refund_gasless_orders(orders.clone(), order_ids.clone(), 0);
    setup.base_swap.refund_gasless_orders(orders.clone(), order_ids.clone(), 0);

    assert_eq!(setup.base_swap.dispatched_origin_domain(), setup.origin);
    assert_eq!(setup.base_swap.dispatched_order_ids()[0], order_ids[0]);
    assert_eq!(setup.base_swap.dispatched_order_ids()[1], order_ids[1]);
}

#[test]
fn test__handle_settle_order_works() {
    let setup = setup();
    let order_data = prepare_order_data(setup.clone());
    let order_id = 'order1'.into();

    // Set order to opened
    setup.base_swap.set_order_opened(order_id, order_data);

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    let mut spy = spy_events();
    setup
        .base_swap
        .handle_settle_order(
            setup.destination, setup.counterpart, order_id, setup.karp.account.contract_address,
        );
    spy
        .assert_emitted(
            @array![
                (
                    setup.base_swap.contract_address,
                    BasicSwap7683Component::Event::Settled(
                        BasicSwap7683Component::Settled {
                            order_id: order_id.clone(),
                            receiver: setup.karp.account.contract_address,
                        },
                    ),
                ),
            ],
        );

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.SETTLED());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())]
            - setup.amount,
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())]
            + setup.amount,
    );
}

#[test]
fn test__handle_settle_order_not_OPENED() {
    let setup = setup();
    let order_id = 'order1'.into();
    // don't set the order as opened

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    setup
        .base_swap
        .handle_settle_order(
            setup.destination, setup.counterpart, order_id, setup.karp.account.contract_address,
        );

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.UNKNOWN());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())],
    );
}

#[test]
fn test__handle_settle_order_wrong_msg_origin() {
    let setup = setup();
    let order_id = 'order1'.into();
    // don't set the order as opened

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    setup
        .base_swap
        .handle_settle_order(
            setup.wrong_msg_origin,
            setup.counterpart,
            order_id,
            setup.karp.account.contract_address,
        );

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.UNKNOWN());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())],
    );
}

#[test]
fn test__handle_settle_order_wrong_msg_sender() {
    let setup = setup();
    let order_id = 'order1'.into();
    // don't set the order as opened

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    setup
        .base_swap
        .handle_settle_order(
            setup.destination,
            setup.wrong_msg_sender,
            order_id,
            setup.karp.account.contract_address,
        );

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.UNKNOWN());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())],
    );
}

#[test]
fn test__handle_refund_order_works() {
    let setup = setup();
    let order_data = prepare_order_data(setup.clone());
    let order_id = 'order1'.into();

    // set the order as opened
    setup.base_swap.set_order_opened(order_id, order_data);

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    let mut spy = spy_events();
    setup.base_swap.handle_refund_order(setup.destination, setup.counterpart, order_id);
    spy
        .assert_emitted(
            @array![
                (
                    setup.base_swap.contract_address,
                    BasicSwap7683Component::Event::Refunded(
                        BasicSwap7683Component::Refunded {
                            order_id: order_id.clone(),
                            receiver: setup.kaka.account.contract_address,
                        },
                    ),
                ),
            ],
        );

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.REFUNDED());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())]
            - setup.amount,
    );
    assert_eq!(
        *balances_after[_balance_id(setup.kaka.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.kaka.account.contract_address, setup.clone())]
            + setup.amount,
    );
}

#[test]
fn test__handle_refund_order_not_OPENED() {
    let setup = setup();
    let order_id = 'order1'.into();

    // don't set the order as opened

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    setup.base_swap.handle_refund_order(setup.destination, setup.counterpart, order_id);

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.UNKNOWN());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())],
    );
}

#[test]
fn test__handle_refund_order_wrong_msg_origin() {
    let setup = setup();
    let order_id = 'order1'.into();

    // don't set the order as opened

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    setup.base_swap.handle_refund_order(setup.wrong_msg_origin, setup.counterpart, order_id);

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.UNKNOWN());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())],
    );
}

#[test]
fn test__handle_refund_order_wrong_msg_sender() {
    let setup = setup();
    let order_id = 'order1'.into();

    // don't set the order as opened

    deal(setup.input_token.contract_address, setup.base_swap.contract_address, 1_000_000);
    let balances_before = _balances(setup.input_token, setup.users.clone());

    setup.base_swap.handle_refund_order(setup.destination, setup.wrong_msg_sender, order_id);

    let balances_after = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.base_full.order_status(order_id), setup.base_full.UNKNOWN());
    assert_eq!(
        *balances_after[_balance_id(setup.base_swap.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.base_swap.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())],
    );
}

#[test]
fn test__resolve_order_onchain_works() {
    let setup = setup();
    let order_data = prepare_order_data(setup.clone());
    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data),
        order_data.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    start_cheat_caller_address(
        setup.base_swap.contract_address, setup.kaka.account.contract_address,
    );
    let (resolved_order, _, _) = setup.base_swap.resolve_onchain_order(order.clone());

    _assert_resolved_order(
        resolved_order,
        order.order_data,
        setup.kaka.account.contract_address,
        order_data.fill_deadline,
        Bounded::<u64>::MAX,
        setup.counterpart,
        setup.counterpart,
        1,
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );
}

#[test]
fn test__resolve_order_gasless_works() {
    let setup = setup();
    let permit_nonce = 0;
    let order_data = prepare_order_data(setup.clone());
    let open_deadline = starknet::get_block_timestamp() + 10;
    let order = _prepare_gasless_order(
        setup.clone(),
        OrderEncoder::encode(@order_data),
        permit_nonce,
        open_deadline,
        order_data.fill_deadline,
    );

    let (resolved_order, _, _) = setup
        .base_swap
        .resolve_gasless_order(order.clone(), BytesTrait::new_empty());

    _assert_resolved_order(
        resolved_order,
        order.order_data,
        setup.kaka.account.contract_address,
        order_data.fill_deadline,
        open_deadline,
        setup.counterpart,
        setup.counterpart,
        1,
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );
}

#[test]
#[should_panic(expected: "Invalid order type: 2422673239666974525189380806111333")]
fn test__resolve_order_INVALID_ORDER_TYPE() {
    let setup = setup();
    let wrong_order_type = 'wrongOrderType';
    let order_data = prepare_order_data(setup.clone());
    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data), order_data.fill_deadline, wrong_order_type,
    );

    setup.base_swap.resolve_onchain_order(order);
}

#[test]
#[should_panic(expected: "Invalid origin domain: 0")]
fn test__resolve_order_INVALID_ORIGIN_DOMAIN() {
    let setup = setup();
    let mut order_data = prepare_order_data(setup.clone());
    order_data.origin_domain = 0;
    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data),
        order_data.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    start_cheat_caller_address(
        setup.base_swap.contract_address, setup.kaka.account.contract_address,
    );
    setup.base_swap.resolve_onchain_order(order);
    stop_cheat_caller_address(setup.base_swap.contract_address);
}

#[test]
fn test__get_order_id_gasless_works() {
    let setup = setup();
    let order_data = prepare_order_data(setup.clone());

    let order = _prepare_gasless_order(setup.clone(), OrderEncoder::encode(@order_data), 0, 0, 0);

    assert_eq!(setup.base_swap.get_gasless_order_id(order), OrderEncoder::id(@order_data));
}

#[test]
fn test__get_order_id_onchain_works() {
    let setup = setup();
    let order_data = prepare_order_data(setup.clone());

    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data),
        order_data.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    assert_eq!(setup.base_swap.get_onchain_order_id(order), OrderEncoder::id(@order_data));
}

#[test]
#[should_panic(expected: "Invalid order type: 2422673239666974525189380806111333")]
fn test__get_order_id_onchain_INVALID_ORDER_TYPE() {
    let setup = setup();
    let wrong_order_type = 'wrongOrderType';
    let order_data = prepare_order_data(setup.clone());

    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data), order_data.fill_deadline, wrong_order_type,
    );

    setup.base_swap.get_onchain_order_id(order);
}


#[test]
fn test__fill_order_ERC20_works() {
    let setup = setup();
    let mut order_data = prepare_order_data(setup.clone());
    order_data.destination_domain = setup.origin;
    let order_id = OrderEncoder::id(@order_data);
    let origin_data = OrderEncoder::encode(@order_data);

    let balances_before = _balances(setup.output_token, setup.users.clone());

    start_cheat_caller_address(
        setup.output_token.contract_address, setup.kaka.account.contract_address,
    );
    setup.output_token.approve(setup.base_swap.contract_address, setup.amount);
    stop_cheat_caller_address(setup.output_token.contract_address);

    start_cheat_caller_address(
        setup.base_swap.contract_address, setup.kaka.account.contract_address,
    );
    setup.base_swap.fill_order(order_id, origin_data, BytesTrait::new_empty());
    stop_cheat_caller_address(setup.base_swap.contract_address);

    let balances_after = _balances(setup.output_token, setup.users.clone());

    assert_eq!(
        *balances_after[_balance_id(setup.kaka.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.kaka.account.contract_address, setup.clone())]
            - setup.amount,
    );
    assert_eq!(
        *balances_after[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before[_balance_id(setup.karp.account.contract_address, setup.clone())]
            + setup.amount,
    );
}

#[test]
#[should_panic(expected: 'Invalid order ID')]
fn test__fill_order_INVALID_ORDER_ID() {
    let setup = setup();
    let mut order_data = prepare_order_data(setup.clone());
    order_data.destination_domain = setup.origin;
    let order_id = 'wrongId'.into();
    let origin_data = OrderEncoder::encode(@order_data);

    start_cheat_caller_address(
        setup.base_swap.contract_address, setup.kaka.account.contract_address,
    );
    setup.base_swap.fill_order(order_id, origin_data, BytesTrait::new_empty());
    stop_cheat_caller_address(setup.base_swap.contract_address);
}

#[test]
#[should_panic(expected: 'Order fill expired')]
fn test__fill_order_ORDER_FILL_EXPIRED() {
    let setup = setup();
    let mut order_data = prepare_order_data(setup.clone());
    order_data.fill_deadline = starknet::get_block_timestamp() - 1;
    order_data.destination_domain = setup.origin;
    let order_id = OrderEncoder::id(@order_data);
    let origin_data = OrderEncoder::encode(@order_data);

    start_cheat_caller_address(
        setup.base_swap.contract_address, setup.kaka.account.contract_address,
    );
    setup.base_swap.fill_order(order_id, origin_data, BytesTrait::new_empty());
    stop_cheat_caller_address(setup.base_swap.contract_address);
}

#[test]
#[should_panic(expected: 'Invalid order domain')]
fn test__fill_order_INVALID_ORDER_DOMAIN() {
    let setup = setup();
    let order_data = prepare_order_data(setup.clone());
    let order_id = OrderEncoder::id(@order_data);
    let origin_data = OrderEncoder::encode(@order_data);

    start_cheat_caller_address(
        setup.base_swap.contract_address, setup.kaka.account.contract_address,
    );
    setup.base_swap.fill_order(order_id, origin_data, BytesTrait::new_empty());
    stop_cheat_caller_address(setup.base_swap.contract_address);
}

