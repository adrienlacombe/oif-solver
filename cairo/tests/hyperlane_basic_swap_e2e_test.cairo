use alexandria_bytes::{Bytes, BytesTrait, BytesStore};
use crate::common::{
    pop_event, deploy_environment, deploy_igp, ContractAddressIntoBytes, declare_mock_mailbox,
    declare_test_ism,
};
use crate::common::{ETH_ADDRESS, deploy_hyperlane7683};
use oif_starknet::libraries::order_encoder::{OrderData, OrderEncoder};
use core::num::traits::Bounded;
use contracts::client::router_component::{IRouterDispatcher, IRouterDispatcherTrait};
use contracts::client::gas_router_component::{IGasRouterDispatcher, IGasRouterDispatcherTrait};
use snforge_std::{start_cheat_block_timestamp_global};
use snforge_std::signature::stark_curve::{
    StarkCurveKeyPairImpl, StarkCurveSignerImpl, StarkCurveVerifierImpl,
};
use permit2::snip12_utils::permits::{TokenPermissionsStructHash, U256StructHash};
use openzeppelin_utils::cryptography::snip12::SNIP12HashSpanImpl;
use openzeppelin_token::erc20::interface::{IERC20Dispatcher, IERC20DispatcherTrait};
use openzeppelin_token::erc20::erc20::ERC20Component::{Transfer};
use oif_starknet::libraries::order_encoder::ContractAddressDefault;
use oif_starknet::base7683::{SpanFelt252StructHash, Base7683Component, ArrayFelt252StructHash};
use oif_starknet::basic_swap7683::{BasicSwap7683Component};
use oif_starknet::erc7683::interface::{
    Open, FilledOrder, Base7683ABIDispatcherTrait, GaslessCrossChainOrder,
};
use oif_starknet::libraries::order_encoder::{BytesDefault};
use starknet::ContractAddress;
use snforge_std::{
    start_cheat_caller_address, EventSpyAssertionsTrait, stop_cheat_caller_address, spy_events,
    EventSpyTrait,
};
use crate::mocks::mock_hyperlane_environment::{IMockHyperlaneEnvironmentDispatcherTrait};
use crate::base_test::{
    _assert_open_order, _assert_resolved_order, setup as super_setup, Setup, _get_signature,
    _prepare_gasless_order as __prepare_gasless_order, _balances, _prepare_onchain_order,
};
use mocks::test_interchain_gas_payment::{ITestInterchainGasPaymentDispatcherTrait};
use contracts::interfaces::{IMailboxDispatcher, IMailboxDispatcherTrait};

const GAS_LIMIT: u256 = 60_000;

pub fn _balance_id(user: ContractAddress, setup: Setup) -> usize {
    let kaka = setup.kaka.account.contract_address;
    let karp = setup.karp.account.contract_address;
    let veg = setup.veg.account.contract_address;
    let counter_part = setup.counterpart;
    let origin_router = setup.origin_router.contract_address;
    let destination_router = setup.destination_router.contract_address;
    let igp = setup.igp.contract_address;

    if user == kaka {
        0
    } else if user == karp {
        1
    } else if user == veg {
        2
    } else if user == counter_part {
        3
    } else if user == origin_router {
        4
    } else if user == destination_router {
        5
    } else if user == igp {
        6
    } else {
        999999999
    }
}

pub fn setup() -> Setup {
    let mut setup = super_setup();

    // Deploy TestInterchainGasPayment
    let igp = deploy_igp();
    let gas_payment_quote = igp.quote_gas_payment(GAS_LIMIT);
    setup.igp = igp;
    setup.gas_payment_quote = gas_payment_quote;

    // Deploy hyperlane environment
    let mock_mailbox_class_hash = declare_mock_mailbox();
    let mock_ism_class_hash = declare_test_ism();
    let environment = deploy_environment(
        setup.origin, setup.destination, mock_mailbox_class_hash, mock_ism_class_hash,
    );
    setup.environment = environment;

    // Deploy origin and destination routers
    let origin_router = deploy_hyperlane7683(
        setup.permit2,
        environment.mailboxes(setup.origin).contract_address,
        setup.owner,
        igp.contract_address,
        environment.isms(setup.origin).contract_address,
    );
    let destination_router = deploy_hyperlane7683(
        setup.permit2,
        environment.mailboxes(setup.destination).contract_address,
        setup.owner,
        igp.contract_address,
        environment.isms(setup.destination).contract_address,
    );
    setup.origin_router = origin_router;
    setup.destination_router = destination_router;
    setup.base_full = origin_router.clone();

    let origin_router_b32: u256 = Into::<
        felt252, u256,
    >::into(origin_router.contract_address.into());
    let destination_router_b32: u256 = Into::<
        felt252, u256,
    >::into(destination_router.contract_address.into());
    let destination_router_override_b32: u256 = Default::default();

    setup.origin_router_b32 = origin_router_b32;
    setup.destination_router_b32 = destination_router_b32;
    setup.origin_router_b32 = origin_router_b32;
    setup.destination_router_override_b32 = destination_router_override_b32;
    setup.destination_router_override_b32 = destination_router_override_b32;

    setup.users.append(origin_router.contract_address);
    setup.users.append(destination_router.contract_address);
    setup.users.append(igp.contract_address);

    // Set default and required hooks for the mailbox dispatchers
    IMailboxDispatcher { contract_address: environment.mailboxes(setup.origin).contract_address }
        .set_default_hook(igp.contract_address);
    IMailboxDispatcher { contract_address: environment.mailboxes(setup.origin).contract_address }
        .set_required_hook(igp.contract_address);

    IMailboxDispatcher {
        contract_address: environment.mailboxes(setup.destination).contract_address,
    }
        .set_default_hook(igp.contract_address);
    IMailboxDispatcher {
        contract_address: environment.mailboxes(setup.destination).contract_address,
    }
        .set_required_hook(igp.contract_address);

    start_cheat_caller_address(origin_router.contract_address, setup.owner);
    IRouterDispatcher { contract_address: origin_router.contract_address }
        .enroll_remote_router(setup.destination, destination_router_b32);
    IGasRouterDispatcher { contract_address: origin_router.contract_address }
        .set_destination_gas(
            Option::None, Option::Some(setup.destination), Option::Some(GAS_LIMIT),
        );
    stop_cheat_caller_address(origin_router.contract_address);

    start_cheat_caller_address(destination_router.contract_address, setup.owner);
    IRouterDispatcher { contract_address: destination_router.contract_address }
        .enroll_remote_router(setup.origin, origin_router_b32);
    IGasRouterDispatcher { contract_address: destination_router.contract_address }
        .set_destination_gas(Option::None, Option::Some(setup.origin), Option::Some(GAS_LIMIT));
    stop_cheat_caller_address(destination_router.contract_address);

    setup
}

fn _prepare_order_data(setup: Setup) -> OrderData {
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
        destination_settler: setup.destination_router.contract_address,
        fill_deadline: starknet::get_block_timestamp() + 100,
        data: BytesTrait::new_empty(),
    }
}

fn _prepare_gasless_order(
    origin_data: Bytes, permit_nonce: felt252, open_deadline: u64, fill_deadline: u64, setup: Setup,
) -> GaslessCrossChainOrder {
    __prepare_gasless_order(
        setup.origin_router.contract_address,
        setup.kaka.account.contract_address,
        setup.origin,
        origin_data.clone(),
        permit_nonce,
        open_deadline,
        fill_deadline,
        OrderEncoder::order_data_type_hash(),
    )
}

impl TransferDefault of Default<Transfer> {
    fn default() -> Transfer {
        Transfer { from: Default::default(), to: Default::default(), value: Default::default() }
    }
}

#[test]
fn test_open_fill_settle() {
    let setup = setup();
    let mut spy = spy_events();
    let order_data: OrderData = _prepare_order_data(setup.clone());
    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data),
        order_data.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    // Set allownace for origin router to spend input tokens
    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    IERC20Dispatcher { contract_address: setup.input_token.contract_address }
        .approve(setup.origin_router.contract_address, setup.amount);
    stop_cheat_caller_address(setup.input_token.contract_address);

    let balances_before = _balances(setup.input_token, setup.users.clone());

    /// Open order and catch event
    start_cheat_caller_address(
        setup.origin_router.contract_address, setup.kaka.account.contract_address,
    );
    setup.origin_router.open(order.clone());
    stop_cheat_caller_address(setup.origin_router.contract_address);

    let Open {
        order_id, resolved_order,
    } =
        pop_event::<
            Open,
        >(setup.origin_router.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    //    let balances_after_open = _balances(setup.input_token, setup.users.clone());

    _assert_resolved_order(
        resolved_order.clone(),
        order.order_data.clone(),
        setup.kaka.account.contract_address,
        order_data.fill_deadline,
        Bounded::<u64>::MAX,
        setup.destination_router.contract_address,
        setup.destination_router.contract_address,
        setup.origin,
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );

    _assert_open_order(
        order_id,
        setup.kaka.account.contract_address,
        order.order_data.clone(),
        balances_before.clone(),
        setup.kaka.account.contract_address,
        setup.clone(),
    );

    // fill
    start_cheat_caller_address(
        setup.output_token.contract_address, setup.veg.account.contract_address,
    );
    IERC20Dispatcher { contract_address: setup.output_token.contract_address }
        .approve(setup.destination_router.contract_address, setup.amount);
    stop_cheat_caller_address(setup.output_token.contract_address);

    let balances_before_fill = _balances(setup.output_token, setup.users.clone());

    let filler_data: Bytes = setup.veg.account.contract_address.into();

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.veg.account.contract_address,
    );
    setup
        .destination_router
        .fill(
            order_id,
            (resolved_order.clone().fill_instructions[0]).origin_data.clone(),
            filler_data.clone(),
        );
    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Filled(
                        Base7683Component::Filled {
                            order_id,
                            origin_data: resolved_order.fill_instructions[0].origin_data.clone(),
                            filler_data: filler_data.clone(),
                        },
                    ),
                ),
            ],
        );
    stop_cheat_caller_address(setup.destination_router.contract_address);

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.FILLED());

    let FilledOrder {
        origin_data: _origin_data, filler_data: _filler_data,
    } = setup.destination_router.filled_orders(order_id);

    assert(
        @_origin_data == resolved_order.fill_instructions[0].origin_data, 'Origin data mismatch',
    );
    assert(_filler_data == filler_data, 'Filler data mismatch');

    let balances_after_fill = _balances(setup.output_token, setup.users.clone());
    //   let balances_after_fill_i = _balances(setup.input_token, setup.users.clone());

    assert_eq!(
        *balances_after_fill[_balance_id(setup.veg.account.contract_address, setup.clone())],
        *balances_before_fill[_balance_id(setup.veg.account.contract_address, setup.clone())]
            - setup.amount,
    );

    assert_eq!(
        *balances_after_fill[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before_fill[_balance_id(setup.karp.account.contract_address, setup.clone())]
            + setup.amount,
    );

    // settle
    let order_ids = array![order_id];
    let orders_filler_data = array![filler_data.clone()];

    // Set allownace for hyperlane7683 to spend gas
    start_cheat_caller_address(ETH_ADDRESS(), setup.veg.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.veg.account.contract_address,
    );
    setup.destination_router.settle(order_ids.clone(), setup.gas_payment_quote);
    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Settle(
                        Base7683Component::Settle {
                            order_ids: order_ids.clone(),
                            orders_filler_data: orders_filler_data.clone(),
                        },
                    ),
                ),
            ],
        );
    stop_cheat_caller_address(setup.destination_router.contract_address);

    let balances_before_settle = _balances(setup.input_token, setup.users.clone());

    setup.environment.process_next_pending_message_from_destination();

    let balances_after_settle = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.FILLED());
    assert_eq!(
        *balances_after_settle[_balance_id(setup.origin_router.contract_address, setup.clone())],
        *balances_before_settle[_balance_id(setup.origin_router.contract_address, setup.clone())]
            - setup.amount,
    );
    assert_eq!(
        *balances_after_settle[_balance_id(setup.veg.account.contract_address, setup.clone())],
        *balances_before_settle[_balance_id(setup.veg.account.contract_address, setup.clone())]
            + setup.amount,
    );
}

// test_native_open_fill_settle

#[test]
fn test_open_for_fill_settle() {
    let setup = setup();
    let permit_nonce = 0;
    let order_data: OrderData = _prepare_order_data(setup.clone());
    let open_deadline = starknet::get_block_timestamp() + 10;
    let order = _prepare_gasless_order(
        OrderEncoder::encode(@order_data),
        permit_nonce,
        open_deadline,
        order_data.fill_deadline,
        setup.clone(),
    );

    // open

    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    setup.input_token.approve(setup.permit2, Bounded::<u256>::MAX);
    stop_cheat_caller_address(setup.input_token.contract_address);

    let witness = setup
        .origin_router
        .witness_hash(setup.origin_router.resolve_for(order.clone(), BytesTrait::new_empty()));
    let sig = _get_signature(
        setup.kaka,
        setup.origin_router.contract_address,
        witness,
        setup.input_token.contract_address,
        permit_nonce,
        open_deadline,
        setup.clone(),
    );

    let mut spy = spy_events();

    let balances_before_open = _balances(setup.input_token, setup.users.clone());

    start_cheat_caller_address(
        setup.origin_router.contract_address, setup.kaka.account.contract_address,
    );
    setup.origin_router.open_for(order.clone(), sig, BytesTrait::new_empty());
    stop_cheat_caller_address(setup.origin_router.contract_address);

    let Open {
        order_id, resolved_order,
    } =
        pop_event::<
            Open,
        >(setup.origin_router.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    _assert_resolved_order(
        resolved_order.clone(),
        order.order_data.clone(),
        setup.kaka.account.contract_address,
        order_data.fill_deadline,
        open_deadline,
        setup.destination_router.contract_address,
        setup.destination_router.contract_address,
        setup.origin,
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );

    _assert_open_order(
        order_id,
        setup.kaka.account.contract_address,
        order.order_data.clone(),
        balances_before_open.clone(),
        setup.kaka.account.contract_address,
        setup.clone(),
    );

    // fill

    start_cheat_caller_address(
        setup.output_token.contract_address, setup.veg.account.contract_address,
    );
    setup.output_token.approve(setup.destination_router.contract_address, setup.amount);
    stop_cheat_caller_address(setup.output_token.contract_address);

    let balances_before_fill = _balances(setup.output_token, setup.users.clone());

    let filler_data: Bytes = setup.veg.account.contract_address.into();

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.veg.account.contract_address,
    );
    setup
        .destination_router
        .fill(
            order_id,
            (resolved_order.clone().fill_instructions[0]).origin_data.clone(),
            filler_data.clone(),
        );
    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Filled(
                        Base7683Component::Filled {
                            order_id,
                            origin_data: resolved_order.fill_instructions[0].origin_data.clone(),
                            filler_data: filler_data.clone(),
                        },
                    ),
                ),
            ],
        );
    stop_cheat_caller_address(setup.destination_router.contract_address);

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.FILLED());

    let FilledOrder {
        origin_data: _origin_data, filler_data: _filler_data,
    } = setup.destination_router.filled_orders(order_id);

    assert(
        @_origin_data == resolved_order.fill_instructions[0].origin_data, 'Origin data mismatch',
    );
    assert(_filler_data == filler_data, 'Filler data mismatch');

    let balances_after_fill = _balances(setup.output_token, setup.users.clone());
    //   let balances_after_fill_i = _balances(setup.input_token, setup.users.clone());

    assert_eq!(
        *balances_after_fill[_balance_id(setup.veg.account.contract_address, setup.clone())],
        *balances_before_fill[_balance_id(setup.veg.account.contract_address, setup.clone())]
            - setup.amount,
    );

    assert_eq!(
        *balances_after_fill[_balance_id(setup.karp.account.contract_address, setup.clone())],
        *balances_before_fill[_balance_id(setup.karp.account.contract_address, setup.clone())]
            + setup.amount,
    );

    // settle

    let order_ids = array![order_id];
    let orders_filler_data = array![filler_data.clone()];

    // Set allownace for hyperlane7683 to spend gas
    start_cheat_caller_address(ETH_ADDRESS(), setup.veg.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.veg.account.contract_address,
    );

    setup.destination_router.settle(order_ids.clone(), setup.gas_payment_quote);
    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Settle(
                        Base7683Component::Settle {
                            order_ids: order_ids.clone(),
                            orders_filler_data: orders_filler_data.clone(),
                        },
                    ),
                ),
            ],
        );
    stop_cheat_caller_address(setup.destination_router.contract_address);

    let balances_before_settle = _balances(setup.input_token, setup.users.clone());

    setup.environment.process_next_pending_message_from_destination();

    let balances_after_settle = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.FILLED());
    assert_eq!(
        *balances_after_settle[_balance_id(setup.origin_router.contract_address, setup.clone())],
        *balances_before_settle[_balance_id(setup.origin_router.contract_address, setup.clone())]
            - setup.amount,
    );
    assert_eq!(
        *balances_after_settle[_balance_id(setup.veg.account.contract_address, setup.clone())],
        *balances_before_settle[_balance_id(setup.veg.account.contract_address, setup.clone())]
            + setup.amount,
    );
}


#[test]
fn test_open_refund() {
    let setup = setup();
    let order_data: OrderData = _prepare_order_data(setup.clone());
    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data),
        order_data.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    // open

    // Set allownace for origin router to spend input tokens
    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    IERC20Dispatcher { contract_address: setup.input_token.contract_address }
        .approve(setup.origin_router.contract_address, setup.amount);
    stop_cheat_caller_address(setup.input_token.contract_address);

    let balances_before_open = _balances(setup.input_token, setup.users.clone());

    // Open order and catch event
    let mut spy = spy_events();
    start_cheat_caller_address(
        setup.origin_router.contract_address, setup.kaka.account.contract_address,
    );
    setup.origin_router.open(order.clone());
    stop_cheat_caller_address(setup.origin_router.contract_address);

    let Open {
        order_id, resolved_order,
    } =
        pop_event::<
            Open,
        >(setup.origin_router.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    _assert_resolved_order(
        resolved_order.clone(),
        order.order_data.clone(),
        setup.kaka.account.contract_address,
        order_data.fill_deadline,
        Bounded::<u64>::MAX,
        setup.destination_router.contract_address,
        setup.destination_router.contract_address,
        setup.origin,
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );

    _assert_open_order(
        order_id,
        setup.kaka.account.contract_address,
        order.order_data.clone(),
        balances_before_open.clone(),
        setup.kaka.account.contract_address,
        setup.clone(),
    );

    // refund

    start_cheat_block_timestamp_global(order_data.fill_deadline + 1);
    let order_ids = array![order_id];
    let orders = array![order.clone()];

    // Set allownace for hyperlane7683 to spend gas
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .destination_router
        .refund_onchain_cross_chain_order(orders.clone(), setup.gas_payment_quote);
    stop_cheat_caller_address(setup.destination_router.contract_address);

    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Refund(Base7683Component::Refund { order_ids }),
                ),
            ],
        );

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.UNKNOWN());

    let balances_before_refund = _balances(setup.input_token, setup.users.clone());

    setup.environment.process_next_pending_message_from_destination();
    spy
        .assert_emitted(
            @array![
                (
                    setup.origin_router.contract_address,
                    BasicSwap7683Component::Event::Refunded(
                        BasicSwap7683Component::Refunded {
                            order_id, receiver: setup.kaka.account.contract_address,
                        },
                    ),
                ),
            ],
        );

    let balances_after_refund = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.origin_router.order_status(order_id), setup.origin_router.REFUNDED());
    assert_eq!(
        *balances_after_refund[_balance_id(setup.origin_router.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.origin_router.contract_address, setup.clone())]
            - setup.amount,
    );
    assert_eq!(
        *balances_after_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())]
            + setup.amount,
    );
}

impl RefundDefault of Default<Base7683Component::Refund> {
    fn default() -> Base7683Component::Refund {
        Base7683Component::Refund { order_ids: array![] }
    }
}

#[test]
fn test_open_refund_wrong_msg_origin() { //
    let setup = setup();
    let mut order_data: OrderData = _prepare_order_data(setup.clone());
    order_data.destination_domain = setup.wrong_msg_origin;
    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data),
        order_data.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    // open

    // Set allownace for origin router to spend input tokens
    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    IERC20Dispatcher { contract_address: setup.input_token.contract_address }
        .approve(setup.origin_router.contract_address, setup.amount);
    stop_cheat_caller_address(setup.input_token.contract_address);

    start_cheat_caller_address(
        setup.origin_router.contract_address, setup.kaka.account.contract_address,
    );
    let mut spy = spy_events();
    setup.origin_router.open(order.clone());
    stop_cheat_caller_address(setup.origin_router.contract_address);

    let Open {
        order_id, resolved_order: _,
    } =
        pop_event::<
            Open,
        >(setup.origin_router.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    // refund

    start_cheat_block_timestamp_global(order_data.fill_deadline + 1);
    let order_ids = array![order_id];
    let orders = array![order.clone()];

    // Set allownace for hyperlane7683 to spend gas
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .destination_router
        .refund_onchain_cross_chain_order(orders.clone(), setup.gas_payment_quote);
    stop_cheat_caller_address(setup.destination_router.contract_address);

    let Base7683Component::Refund {
        order_ids: _order_ids,
    } =
        pop_event::<
            Base7683Component::Refund,
        >(setup.destination_router.contract_address, selector!("Refund"), spy.get_events().events)
            .unwrap();

    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Refund(
                        Base7683Component::Refund { order_ids: order_ids.clone() },
                    ),
                ),
            ],
        );

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.UNKNOWN());

    let balances_before_refund = _balances(setup.input_token, setup.users.clone());

    setup.environment.process_next_pending_message_from_destination();

    let balances_after_refund = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.origin_router.order_status(order_id), setup.origin_router.OPENED());
    assert_eq!(
        *balances_after_refund[_balance_id(setup.origin_router.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.origin_router.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())],
    );
}

#[test]
fn test_open_refund_wrong_msg_sender() {
    //
    let setup = setup();
    let mut order_data: OrderData = _prepare_order_data(setup.clone());
    order_data.destination_settler = setup.wrong_msg_sender;
    let order = _prepare_onchain_order(
        OrderEncoder::encode(@order_data),
        order_data.fill_deadline,
        OrderEncoder::order_data_type_hash(),
    );

    // open

    // Set allownace for origin router to spend input tokens
    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    IERC20Dispatcher { contract_address: setup.input_token.contract_address }
        .approve(setup.origin_router.contract_address, setup.amount);
    stop_cheat_caller_address(setup.input_token.contract_address);

    start_cheat_caller_address(
        setup.origin_router.contract_address, setup.kaka.account.contract_address,
    );
    let mut spy = spy_events();
    setup.origin_router.open(order.clone());
    stop_cheat_caller_address(setup.origin_router.contract_address);

    let Open {
        order_id, resolved_order: _,
    } =
        pop_event::<
            Open,
        >(setup.origin_router.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    // refund

    start_cheat_block_timestamp_global(order_data.fill_deadline + 1);
    let order_ids = array![order_id];
    let orders = array![order.clone()];

    // Set allownace for hyperlane7683 to spend gas
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .destination_router
        .refund_onchain_cross_chain_order(orders.clone(), setup.gas_payment_quote);
    stop_cheat_caller_address(setup.destination_router.contract_address);

    let Base7683Component::Refund {
        order_ids: _order_ids,
    } =
        pop_event::<
            Base7683Component::Refund,
        >(setup.destination_router.contract_address, selector!("Refund"), spy.get_events().events)
            .unwrap();

    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Refund(
                        Base7683Component::Refund { order_ids: order_ids.clone() },
                    ),
                ),
            ],
        );

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.UNKNOWN());

    let balances_before_refund = _balances(setup.input_token, setup.users.clone());

    setup.environment.process_next_pending_message_from_destination();

    let balances_after_refund = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.origin_router.order_status(order_id), setup.origin_router.OPENED());
    assert_eq!(
        *balances_after_refund[_balance_id(setup.origin_router.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.origin_router.contract_address, setup.clone())],
    );
    assert_eq!(
        *balances_after_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())],
    );
}

// test_native_open_refund

#[test]
fn test_open_for_refund() {
    let setup = setup();
    let permit_nonce = 0;
    let order_data: OrderData = _prepare_order_data(setup.clone());
    let open_deadline = starknet::get_block_timestamp() + 10;
    let order = _prepare_gasless_order(
        OrderEncoder::encode(@order_data),
        permit_nonce,
        open_deadline,
        order_data.fill_deadline,
        setup.clone(),
    );

    // open

    start_cheat_caller_address(
        setup.input_token.contract_address, setup.kaka.account.contract_address,
    );
    setup.input_token.approve(setup.permit2, Bounded::<u256>::MAX);
    stop_cheat_caller_address(setup.input_token.contract_address);

    let witness = setup
        .origin_router
        .witness_hash(setup.origin_router.resolve_for(order.clone(), BytesTrait::new_empty()));
    let sig = _get_signature(
        setup.kaka,
        setup.origin_router.contract_address,
        witness,
        setup.input_token.contract_address,
        permit_nonce,
        open_deadline,
        setup.clone(),
    );

    let mut spy = spy_events();
    let balances_before_open = _balances(setup.input_token, setup.users.clone());

    start_cheat_caller_address(
        setup.origin_router.contract_address, setup.kaka.account.contract_address,
    );
    setup.origin_router.open_for(order.clone(), sig, BytesTrait::new_empty());
    stop_cheat_caller_address(setup.origin_router.contract_address);

    let Open {
        order_id, resolved_order,
    } =
        pop_event::<
            Open,
        >(setup.origin_router.contract_address, selector!("Open"), spy.get_events().events)
            .expect('Open event not found');

    _assert_resolved_order(
        resolved_order.clone(),
        order.order_data.clone(),
        setup.kaka.account.contract_address,
        order_data.fill_deadline,
        open_deadline,
        setup.destination_router.contract_address,
        setup.destination_router.contract_address,
        setup.origin,
        setup.input_token.contract_address,
        setup.output_token.contract_address,
        setup.clone(),
    );

    _assert_open_order(
        order_id,
        setup.kaka.account.contract_address,
        order.order_data.clone(),
        balances_before_open.clone(),
        setup.kaka.account.contract_address,
        setup.clone(),
    );

    // refund
    start_cheat_block_timestamp_global(order_data.fill_deadline + 1);

    let order_ids = array![order_id];
    let orders = array![order];

    // Set allowance for hyperlane7683 to spend gas
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.destination_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .destination_router
        .refund_gasless_cross_chain_order(orders.clone(), setup.gas_payment_quote);
    stop_cheat_caller_address(setup.destination_router.contract_address);

    spy
        .assert_emitted(
            @array![
                (
                    setup.destination_router.contract_address,
                    Base7683Component::Event::Refund(Base7683Component::Refund { order_ids }),
                ),
            ],
        );

    assert_eq!(setup.destination_router.order_status(order_id), setup.destination_router.UNKNOWN());

    let balances_before_refund = _balances(setup.input_token, setup.users.clone());

    setup.environment.process_next_pending_message_from_destination();
    spy
        .assert_emitted(
            @array![
                (
                    setup.origin_router.contract_address,
                    BasicSwap7683Component::Event::Refunded(
                        BasicSwap7683Component::Refunded {
                            order_id, receiver: setup.kaka.account.contract_address,
                        },
                    ),
                ),
            ],
        );

    let balances_after_refund = _balances(setup.input_token, setup.users.clone());

    assert_eq!(setup.origin_router.order_status(order_id), setup.origin_router.REFUNDED());
    assert_eq!(
        *balances_after_refund[_balance_id(setup.origin_router.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.origin_router.contract_address, setup.clone())]
            - setup.amount,
    );
    assert_eq!(
        *balances_after_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())],
        *balances_before_refund[_balance_id(setup.kaka.account.contract_address, setup.clone())]
            + setup.amount,
    );
}
