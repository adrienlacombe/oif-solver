use alexandria_bytes::{Bytes, BytesStore};
use crate::common::{
    deploy_environment, deploy_igp, declare_mock_mailbox, declare_test_ism, ETH_ADDRESS, deal_eth,
    deploy_mock_hyperlane7683, ContractAddressIntoBytes,
};
use contracts::client::router_component::{IRouterDispatcher, IRouterDispatcherTrait};
use contracts::client::gas_router_component::{IGasRouterDispatcher, IGasRouterDispatcherTrait};
use snforge_std::{start_cheat_caller_address_global, stop_cheat_caller_address_global};
use snforge_std::signature::stark_curve::{
    StarkCurveKeyPairImpl, StarkCurveSignerImpl, StarkCurveVerifierImpl,
};
use permit2::snip12_utils::permits::{TokenPermissionsStructHash, U256StructHash};
use openzeppelin_utils::cryptography::snip12::SNIP12HashSpanImpl;
use openzeppelin_token::erc20::interface::{IERC20Dispatcher, IERC20DispatcherTrait};
use oif_starknet::libraries::order_encoder::ContractAddressDefault;
use oif_starknet::libraries::hyperlane7683_message::{Hyperlane7683Message};
use oif_starknet::base7683::{SpanFelt252StructHash, ArrayFelt252StructHash};
use oif_starknet::libraries::order_encoder::{BytesDefault};
use starknet::ContractAddress;
use snforge_std::{start_cheat_caller_address, stop_cheat_caller_address};
use crate::base_test::{Setup, setup as super_setup};

use contracts::interfaces::{
    IMailboxClientDispatcher, IMailboxClientDispatcherTrait, IMailboxDispatcher,
    IMailboxDispatcherTrait,
};
use crate::mocks::mock_mailbox::{Call, IMockMailboxDispatcherTrait};
use contracts::hooks::libs::standard_hook_metadata::standard_hook_metadata::StandardHookMetadata;
use mocks::test_interchain_gas_payment::ITestInterchainGasPaymentDispatcherTrait;
use crate::mocks::mock_hyperlane7683::IMockHyperlane7683DispatcherTrait;
use crate::mocks::mock_hyperlane_environment::IMockHyperlaneEnvironmentDispatcherTrait;

const GAS_LIMIT: u256 = 60_000;

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
    let origin_router = deploy_mock_hyperlane7683(
        setup.permit2,
        environment.mailboxes(setup.origin).contract_address,
        setup.owner,
        igp.contract_address,
        environment.isms(setup.origin).contract_address,
    );
    let destination_router = deploy_mock_hyperlane7683(
        setup.permit2,
        environment.mailboxes(setup.destination).contract_address,
        setup.owner,
        igp.contract_address,
        environment.isms(setup.destination).contract_address,
    );

    let origin_router_b32: u256 = Into::<
        felt252, u256,
    >::into(origin_router.contract_address.into());
    let destination_router_b32: u256 = Into::<
        felt252, u256,
    >::into(destination_router.contract_address.into());
    let destination_router_override_b32: u256 = Default::default();

    setup.mock_origin_router = origin_router;
    setup.mock_destination_router = destination_router;
    setup.origin_router_b32 = origin_router_b32;
    setup.destination_router_b32 = destination_router_b32;
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

    setup
}

fn _balance_id(user: ContractAddress, setup: Setup) -> usize {
    let kaka = setup.kaka.account.contract_address;
    let karp = setup.karp.account.contract_address;
    let veg = setup.veg.account.contract_address;
    let counter_part = setup.counterpart;
    let origin_router = setup.mock_origin_router.contract_address;
    let destination_router = setup.mock_destination_router.contract_address;
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

fn enroll_routers(setup: Setup) {
    let origin_router_address = setup.mock_origin_router.contract_address;
    let destination_router_address = setup.mock_destination_router.contract_address;

    start_cheat_caller_address(origin_router_address, setup.owner);
    IRouterDispatcher { contract_address: origin_router_address }
        .enroll_remote_router(setup.destination, setup.destination_router_b32);
    IGasRouterDispatcher { contract_address: origin_router_address }
        .set_destination_gas(Option::None, Option::Some(setup.destination), Option::Some(60_000));
    stop_cheat_caller_address(origin_router_address);

    start_cheat_caller_address(destination_router_address, setup.owner);
    IRouterDispatcher { contract_address: destination_router_address }
        .enroll_remote_router(setup.origin, setup.origin_router_b32);
    IGasRouterDispatcher { contract_address: destination_router_address }
        .set_destination_gas(Option::None, Option::Some(setup.origin), Option::Some(60_000));
    stop_cheat_caller_address(destination_router_address);
}

#[test]
fn test_local_domain() {
    let setup = setup();

    assert_eq!(setup.mock_origin_router.get_7683_local_domain(), setup.origin);
    assert_eq!(setup.mock_destination_router.get_7683_local_domain(), setup.destination);
}

#[test]
#[fuzzer]
fn test_fuzz_enroll_remote_routers(mut count: u8, mut domain: u32, mut router: u256) { //
    let setup = setup();

    if (count.into() >= router) {
        router = count.into() + 1
    }
    if (count.into() >= domain) {
        domain = count.into() + 1
    }
    if (router == 0) {
        router = 1;
    }

    let mut domains: Array<u32> = array![];
    let mut routers: Array<u256> = array![];

    for i in 0..count {
        domains.append(domain - i.into());
        routers.append(router - i.into());
    };

    start_cheat_caller_address(setup.mock_origin_router.contract_address, setup.owner);
    IRouterDispatcher { contract_address: setup.mock_origin_router.contract_address }
        .enroll_remote_routers(domains.clone(), routers.clone());
    stop_cheat_caller_address(setup.mock_origin_router.contract_address);

    let actual_domains = IRouterDispatcher {
        contract_address: setup.mock_origin_router.contract_address,
    }
        .domains();

    assert_eq!(actual_domains.len(), domains.len());
    assert_eq!(
        IRouterDispatcher { contract_address: setup.mock_origin_router.contract_address }.domains(),
        domains,
    );
    for i in 0..count {
        let actual_router = IRouterDispatcher {
            contract_address: setup.mock_origin_router.contract_address,
        }
            .routers(*domains.at(i.into()));

        assert_eq!(@actual_router, routers.at(i.into()));
        assert_eq!(actual_domains.at(i.into()), domains.at(i.into()));
    }
}

#[test]
fn test__dispatch_settle_works() {
    let setup = setup();
    enroll_routers(setup.clone());
    deal_eth(setup.kaka.account.contract_address, 1_000_000);

    let receiver1: ContractAddress = 'receiver1'.try_into().unwrap();
    let receiver2: ContractAddress = 'receiver2'.try_into().unwrap();
    let orders_filler_data: Array<Bytes> = array![receiver1.into(), receiver2.into()];
    let order_ids: Array<u256> = array!['someOrderId1'.into(), 'someOrderId2'.into()];

    // Set allownace for hyperlane7683 to spend ETH tokens
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.mock_origin_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.mock_origin_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .mock_origin_router
        .dispatch_settle(
            setup.destination,
            order_ids.clone(),
            orders_filler_data.clone(),
            setup.gas_payment_quote,
        );

    stop_cheat_caller_address(setup.mock_origin_router.contract_address);

    start_cheat_caller_address_global(setup.kaka.account.contract_address);
    let expected_metadata = StandardHookMetadata::override_gas_limits(
        IGasRouterDispatcher { contract_address: setup.mock_origin_router.contract_address }
            .destination_gas(setup.destination),
    );
    stop_cheat_caller_address_global();

    let expected_call = Call {
        destination_domain: setup.destination,
        recipient_address: Into::<
            felt252, u256,
        >::into(setup.mock_destination_router.contract_address.into()),
        message_body: Hyperlane7683Message::encode_settle(
            order_ids.span(), orders_filler_data.span(),
        ),
        fee_amount: setup.gas_payment_quote,
        metadata: Option::Some(expected_metadata),
        hook: Option::Some(
            IMailboxClientDispatcher { contract_address: setup.mock_origin_router.contract_address }
                .get_hook(),
        ),
    };

    let actual_call = setup.environment.mailboxes(setup.origin).latest_call();

    assert(expected_call == actual_call, 'Dispatch call does not match');
}

#[test]
fn test__dispatch_refund_works() {
    let setup = setup();
    enroll_routers(setup.clone());
    deal_eth(setup.kaka.account.contract_address, 1_000_000);

    let order_ids: Array<u256> = array!['someOrderId1'.into(), 'someOrderId2'.into()];

    // Set allownace for hyperlane7683 to spend ETH tokens
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.mock_origin_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.mock_origin_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .mock_origin_router
        .dispatch_refund(setup.destination, order_ids.clone(), setup.gas_payment_quote);

    stop_cheat_caller_address(setup.mock_origin_router.contract_address);

    start_cheat_caller_address_global(setup.kaka.account.contract_address);
    let expected_metadata = StandardHookMetadata::override_gas_limits(
        IGasRouterDispatcher { contract_address: setup.mock_origin_router.contract_address }
            .destination_gas(setup.destination),
    );
    stop_cheat_caller_address_global();

    let expected_call = Call {
        destination_domain: setup.destination,
        recipient_address: Into::<
            felt252, u256,
        >::into(setup.mock_destination_router.contract_address.into()),
        message_body: Hyperlane7683Message::encode_refund(order_ids.span()),
        fee_amount: setup.gas_payment_quote,
        metadata: Option::Some(expected_metadata),
        hook: Option::Some(
            IMailboxClientDispatcher { contract_address: setup.mock_origin_router.contract_address }
                .get_hook(),
        ),
    };

    let actual_call = setup.environment.mailboxes(setup.origin).latest_call();

    assert(expected_call == actual_call, 'Dispatch call does not match');
}


#[test]
fn test__handle_settle_works() {
    let setup = setup();
    enroll_routers(setup.clone());
    deal_eth(setup.kaka.account.contract_address, 1_000_000);

    let receiver1: ContractAddress = 'receiver1'.try_into().unwrap();
    let receiver2: ContractAddress = 'receiver2'.try_into().unwrap();
    let orders_filler_data: Array<Bytes> = array![receiver1.into(), receiver2.into()];
    let order_ids: Array<u256> = array!['someOrderId1'.into(), 'someOrderId2'.into()];

    // Set allownace for hyperlane7683 to spend ETH tokens
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.mock_destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.mock_destination_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .mock_destination_router
        .dispatch_settle(
            setup.origin, order_ids.clone(), orders_filler_data.clone(), setup.gas_payment_quote,
        );
    stop_cheat_caller_address(setup.mock_origin_router.contract_address);

    setup.environment.process_next_pending_message_from_destination();

    assert_eq!(*setup.mock_origin_router.settled_message_origin()[0], setup.destination);
    assert_eq!(*setup.mock_origin_router.settled_message_origin()[1], setup.destination);

    assert_eq!(
        *setup.mock_origin_router.settled_message_sender()[0],
        setup.mock_destination_router.contract_address,
    );
    assert_eq!(
        *setup.mock_origin_router.settled_message_sender()[1],
        setup.mock_destination_router.contract_address,
    );
    assert_eq!(*setup.mock_origin_router.settled_order_id()[0], *order_ids[0]);
    assert_eq!(*setup.mock_origin_router.settled_order_id()[1], *order_ids[1]);

    assert_eq!(*setup.mock_origin_router.settled_order_receiver()[0], receiver1);
    assert_eq!(*setup.mock_origin_router.settled_order_receiver()[1], receiver2);
}

#[test]
fn test__handle_refund_works() {
    let setup = setup();
    enroll_routers(setup.clone());
    deal_eth(setup.kaka.account.contract_address, 1_000_000);

    let order_ids: Array<u256> = array!['someOrderId1'.into(), 'someOrderId2'.into()];

    // Set allownace for hyperlane7683 to spend ETH tokens
    start_cheat_caller_address(ETH_ADDRESS(), setup.kaka.account.contract_address);
    IERC20Dispatcher { contract_address: ETH_ADDRESS() }
        .approve(setup.mock_destination_router.contract_address, 1_000_000);
    stop_cheat_caller_address(ETH_ADDRESS());

    start_cheat_caller_address(
        setup.mock_destination_router.contract_address, setup.kaka.account.contract_address,
    );
    setup
        .mock_destination_router
        .dispatch_refund(setup.origin, order_ids.clone(), setup.gas_payment_quote);
    stop_cheat_caller_address(setup.mock_origin_router.contract_address);

    setup.environment.process_next_pending_message_from_destination();

    assert_eq!(*setup.mock_origin_router.refunded_order_id()[0], *order_ids[0]);
    assert_eq!(*setup.mock_origin_router.refunded_order_id()[1], *order_ids[1]);
}

