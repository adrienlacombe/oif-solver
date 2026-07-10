use alexandria_bytes::{Bytes, BytesStore};
use core::hash::{HashStateExTrait, HashStateTrait};
use core::num::traits::Pow;
use core::poseidon::PoseidonTrait;
use mocks::test_interchain_gas_payment::ITestInterchainGasPaymentDispatcher;
use oif_starknet::base7683::{ArrayFelt252StructHash, SpanFelt252StructHash};
use oif_starknet::erc7683::interface::{
    Base7683ABIDispatcher, Base7683ABIDispatcherTrait, GaslessCrossChainOrder,
    OnchainCrossChainOrder, Open, ResolvedCrossChainOrder,
};
use oif_starknet::libraries::order_encoder::{ContractAddressDefault, OpenOrderEncoder};
use openzeppelin_token::erc20::interface::{IERC20Dispatcher, IERC20DispatcherTrait};
use openzeppelin_utils::cryptography::snip12::{SNIP12HashSpanImpl, StructHash};
use permit2::interfaces::permit2::{IPermit2ABIDispatcher, IPermit2ABIDispatcherTrait};
use permit2::interfaces::signature_transfer::{PermitBatchTransferFrom, TokenPermissions};
use permit2::snip12_utils::permits::{
    PermitBatchStructHash, PermitBatchTransferFromOffChainMessageHashWitness,
    PermitBatchTransferFromStructHash, PermitBatchTransferFromStructHashWitness,
    PermitSingleStructHash, PermitTransferFromOffChainMessageHashWitness,
    PermitTransferFromStructHash, PermitTransferFromStructHashWitness, TokenPermissionsStructHash,
    U256StructHash,
};
use snforge_std::signature::SignerTrait;
use snforge_std::signature::stark_curve::{
    StarkCurveKeyPairImpl, StarkCurveSignerImpl, StarkCurveVerifierImpl,
};
use snforge_std::start_cheat_block_timestamp_global;
use starknet::ContractAddress;
use crate::common::{
    Account, ETH_ADDRESS, deal_multiple, deploy_erc20, deploy_eth, deploy_permit2, generate_account,
};
use crate::mocks::mock_base7683::IMockBase7683Dispatcher;
use crate::mocks::mock_basic_swap7683::IMockBasicSwap7683Dispatcher;
use crate::mocks::mock_hyperlane7683::IMockHyperlane7683Dispatcher;
use crate::mocks::mock_hyperlane_environment::IMockHyperlaneEnvironmentDispatcher;

#[derive(Drop, Clone)]
pub struct Setup {
    pub permit2: ContractAddress,
    pub input_token: IERC20Dispatcher,
    pub output_token: IERC20Dispatcher,
    pub kaka: Account,
    pub karp: Account,
    pub veg: Account,
    pub counterpart: ContractAddress,
    pub origin: u32,
    pub destination: u32,
    pub amount: u256,
    pub DOMAIN_SEPARATOR: felt252,
    pub users: Array<ContractAddress>,
    /// base
    pub base_full: Base7683ABIDispatcher,
    pub base: IMockBase7683Dispatcher,
    /// basic swap
    pub base_swap: IMockBasicSwap7683Dispatcher,
    pub wrong_msg_origin: u32,
    pub wrong_msg_sender: ContractAddress,
    /// hyperlane
    pub admin: ContractAddress,
    pub owner: ContractAddress,
    pub sender: ContractAddress,
    pub environment: IMockHyperlaneEnvironmentDispatcher,
    pub igp: ITestInterchainGasPaymentDispatcher,
    pub origin_router_b32: u256,
    pub destination_router_b32: u256,
    pub destination_router_override_b32: u256,
    pub gas_payment_quote: u256,
    pub gas_payment_quote_override: u256,
    // base
    pub mock_origin_router: IMockHyperlane7683Dispatcher,
    pub mock_destination_router: IMockHyperlane7683Dispatcher,
    // e2e
    pub origin_router: Base7683ABIDispatcher,
    pub destination_router: Base7683ABIDispatcher,
}

#[derive(Drop, Clone)]
pub struct BaseTestSetup {
    pub base_full: Base7683ABIDispatcher,
    pub base: IMockBase7683Dispatcher,
    pub permit2: ContractAddress,
    pub input_token: IERC20Dispatcher,
    pub output_token: IERC20Dispatcher,
    pub kaka: Account,
    pub karp: Account,
    pub veg: Account,
    pub counterpart: ContractAddress,
    pub origin: u32,
    pub destination: u32,
    pub amount: u256,
    pub DOMAIN_SEPARATOR: felt252,
    pub fork_id: u256,
    pub users: Array<ContractAddress>,
}

pub fn setup() -> Setup {
    start_cheat_block_timestamp_global(123456789);
    let permit2 = deploy_permit2();
    let _eth = deploy_eth();
    let input_token = deploy_erc20("Input Token", "IN");
    let output_token = deploy_erc20("Output Token", "OUT");
    let DOMAIN_SEPARATOR = IPermit2ABIDispatcher { contract_address: permit2 }.DOMAIN_SEPARATOR();

    let admin = 'admin'.try_into().unwrap();
    let owner = 'owner'.try_into().unwrap();
    let sender = 'sender'.try_into().unwrap();
    let kaka = generate_account();
    let karp = generate_account();
    let veg = generate_account();
    let counterpart: ContractAddress = 'counterpart'.try_into().unwrap();
    let users = array![
        kaka.account.contract_address,
        karp.account.contract_address,
        veg.account.contract_address,
        counterpart,
    ];

    deal_multiple(
        array![
            input_token.contract_address,
            output_token.contract_address,
            _eth.contract_address,
            _eth.contract_address,
        ],
        array![
            kaka.account.contract_address,
            karp.account.contract_address,
            veg.account.contract_address,
        ],
        1_000_000 * 10_u256.pow(18),
    );

    let default: ContractAddress = Default::default();

    Setup {
        // Base
        permit2,
        input_token,
        output_token,
        kaka,
        karp,
        veg,
        counterpart,
        origin: 1,
        destination: 2,
        amount: 100,
        DOMAIN_SEPARATOR,
        users,
        // BaseTest
        base_full: Base7683ABIDispatcher { contract_address: default },
        base: IMockBase7683Dispatcher { contract_address: default },
        /// BasicSwapTest
        base_swap: IMockBasicSwap7683Dispatcher { contract_address: default },
        wrong_msg_origin: Default::default(),
        wrong_msg_sender: Default::default(),
        /// HyperlaneTest
        admin,
        owner,
        sender,
        environment: IMockHyperlaneEnvironmentDispatcher { contract_address: default },
        igp: ITestInterchainGasPaymentDispatcher { contract_address: default },
        mock_origin_router: IMockHyperlane7683Dispatcher { contract_address: default },
        mock_destination_router: IMockHyperlane7683Dispatcher { contract_address: default },
        origin_router: Base7683ABIDispatcher { contract_address: default },
        destination_router: Base7683ABIDispatcher { contract_address: default },
        origin_router_b32: Default::default(),
        destination_router_b32: Default::default(),
        destination_router_override_b32: Default::default(),
        gas_payment_quote: Default::default(),
        gas_payment_quote_override: Default::default(),
    }
}

pub fn _prepare_onchain_order(
    order_data: Bytes, fill_deadline: u64, order_data_type: u256,
) -> OnchainCrossChainOrder {
    OnchainCrossChainOrder { order_data, fill_deadline, order_data_type }
}

pub fn _prepare_gasless_order(
    origin_settler: ContractAddress,
    user: ContractAddress,
    origin_chain_id: u32,
    order_data: Bytes,
    nonce: felt252,
    open_deadline: u64,
    fill_deadline: u64,
    order_data_type: u256,
) -> GaslessCrossChainOrder {
    GaslessCrossChainOrder {
        origin_settler,
        user,
        origin_chain_id,
        order_data,
        nonce,
        open_deadline,
        fill_deadline,
        order_data_type,
    }
}

pub fn _get_order_id_from_logs(target: ContractAddress) -> (u256, ResolvedCrossChainOrder) {
    let Open {
        order_id, resolved_order,
    }: Open = starknet::testing::pop_log::<Open>(target).expect('Failed to pop Open event');

    (order_id, resolved_order)
}

pub fn _balances(token: IERC20Dispatcher, users: Array<ContractAddress>) -> Array<u256> {
    let mut balances: Array<u256> = array![];
    for user in users.span() {
        balances.append(token.balance_of(*user));
    };
    balances
}

pub fn _eth_balances(users: Array<ContractAddress>) -> Array<u256> {
    let mut balances: Array<u256> = array![];
    let eth = IERC20Dispatcher { contract_address: ETH_ADDRESS() };
    for user in users.span() {
        balances.append(eth.balance_of(*user));
    };
    balances
}

pub fn _TOKEN_PERMISSIONS_TYPE_HASH() -> felt252 {
    selector!(
        "\"Token Permissions\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\")\"u256\"(\"low\":\"u128\",\"high\":\"u128\")\"",
    )
}

pub fn FULL_WITNESS_TYPE_HASH() -> felt252 {
    selector!(
        "\"Permit Witness Transfer From\"(\"Permitted\":\"Token Permissions\",\"Spender\":\"ContractAddress\",\"Nonce\":\"felt\",\"Deadline\":\"u256\",\"Witness\":\"Resolved Cross Chain Order\")\"Bytes\"(\"Size\":\"u128\",\"Data\":\"u128*\")\"Fill Instruction\"(\"Destination Chain ID\":\"u128\",\"Destination Settler\":\"ContractAddress\",\"Origin Data\":\"Bytes\")\"Resolved Cross Chain Order\"(\"User\":\"ContractAddress\",\"Origin Chain ID\":\"u128\",\"Open Deadline\":\"timestamp\",\"Fill Deadline\":\"timestamp\",\"Order ID\":\"u256\",\"Max Spent\":\"Output*\",\"Min Received\":\"Output*\",\"Fill Instructions\":\"Fill Instruction*\")\"Output\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\",\"Recipient\":\"ContractAddress\",\"Chain ID\":\"u128\")\"Token Permissions\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\")\"u256\"(\"low\":\"u128\",\"high\":\"u128\")",
    )
}

pub fn FULL_WITNESS_BATCH_TYPE_HASH() -> felt252 {
    selector!(
        "\"Permit Witness Batch Transfer From\"(\"Permitted\":\"Token Permissions*\",\"Spender\":\"ContractAddress\",\"Nonce\":\"felt\",\"Deadline\":\"u256\",\"Witness\":\"Resolved Cross Chain Order\")\"Bytes\"(\"Size\":\"u128\",\"Data\":\"u128*\")\"Fill Instruction\"(\"Destination Chain ID\":\"u128\",\"Destination Settler\":\"ContractAddress\",\"Origin Data\":\"Bytes\")\"Resolved Cross Chain Order\"(\"User\":\"ContractAddress\",\"Origin Chain ID\":\"u128\",\"Open Deadline\":\"timestamp\",\"Fill Deadline\":\"timestamp\",\"Order ID\":\"u256\",\"Max Spent\":\"Output*\",\"Min Received\":\"Output*\",\"Fill Instructions\":\"Fill Instruction*\")\"Output\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\",\"Recipient\":\"ContractAddress\",\"Chain ID\":\"u128\")\"Token Permissions\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\")\"u256\"(\"low\":\"u128\",\"high\":\"u128\")",
    )
}

pub fn _get_permit_batch_witness_signature(
    signer: Account,
    spender: ContractAddress,
    permit: PermitBatchTransferFrom,
    type_hash: felt252,
    witness: felt252,
    domain_separator: felt252,
) -> (felt252, felt252) {
    let mut hashed_permissions: Array<felt252> = array![];
    for permission in permit.permitted {
        hashed_permissions.append(permission.hash_struct());
    };

    let hashed_permit = PoseidonTrait::new()
        .update_with(type_hash)
        .update_with(hashed_permissions.span())
        .update_with(spender)
        .update_with(permit.nonce)
        .update_with(permit.deadline.hash_struct())
        .update_with(witness)
        .finalize();

    let msg_hash = PoseidonTrait::new()
        .update_with('StarkNet Message')
        .update_with(domain_separator)
        .update_with(signer.account.contract_address)
        .update_with(hashed_permit)
        .finalize();

    signer.key_pair.sign(msg_hash).unwrap()
}


pub fn _default_erc20_permit_multiple(
    tokens: Array<ContractAddress>, nonce: felt252, amount: u256, deadline: u256,
) -> PermitBatchTransferFrom {
    let mut permitted: Array<TokenPermissions> = array![];
    for token in tokens.span() {
        permitted.append(TokenPermissions { token: *token, amount });
    };

    PermitBatchTransferFrom { permitted: permitted.span(), nonce, deadline }
}

pub fn _get_signature(
    signer: Account,
    spender: ContractAddress,
    witness: felt252,
    token: ContractAddress,
    nonce: felt252,
    deadline: u64,
    setup: Setup,
) -> Array<felt252> {
    let permit = _default_erc20_permit_multiple(
        array![token], nonce, setup.amount, deadline.into(),
    );

    let (sig1, sig2) = _get_permit_batch_witness_signature(
        signer, spender, permit, FULL_WITNESS_BATCH_TYPE_HASH(), witness, setup.DOMAIN_SEPARATOR,
    );

    array![sig1, sig2]
}

pub fn _assert_resolved_order(
    resolved_order: ResolvedCrossChainOrder,
    order_data: Bytes,
    user: ContractAddress,
    fill_deadline: u64,
    open_deadline: u64,
    to: ContractAddress,
    destination_settler: ContractAddress,
    origin_chain_id: u32,
    input_token: ContractAddress,
    output_token: ContractAddress,
    setup: Setup,
) {
    assert_eq!(resolved_order.max_spent.len(), 1);
    assert_eq!(*resolved_order.max_spent.at(0).token, output_token);
    assert_eq!(*resolved_order.max_spent.at(0).amount, setup.amount);
    assert_eq!(*resolved_order.max_spent.at(0).recipient, to);
    assert_eq!(*resolved_order.max_spent.at(0).chain_id, setup.destination);

    assert_eq!(resolved_order.min_received.len(), 1);
    assert_eq!(*resolved_order.min_received.at(0).token, input_token);
    assert_eq!(*resolved_order.min_received.at(0).amount, setup.amount);
    assert_eq!(*resolved_order.min_received.at(0).recipient, 0.try_into().unwrap());
    assert_eq!(*resolved_order.min_received.at(0).chain_id, setup.origin);

    assert_eq!(resolved_order.fill_instructions.len(), 1);
    assert_eq!(*resolved_order.fill_instructions.at(0).destination_chain_id, setup.destination);
    assert_eq!(*resolved_order.fill_instructions.at(0).destination_settler, destination_settler);
    assert(
        resolved_order.fill_instructions.at(0).origin_data == @order_data,
        'Fill instructions do not match',
    );

    assert_eq!(resolved_order.user, user);
    assert_eq!(resolved_order.origin_chain_id, origin_chain_id);
    assert_eq!(resolved_order.open_deadline, open_deadline);
    assert_eq!(resolved_order.fill_deadline, fill_deadline);
}


pub fn _order_data_by_id(order_id: u256, setup: Setup) -> Bytes {
    let (_, order_data): (u256, @Bytes) = setup.base_full.open_orders(order_id).decode();

    order_data.clone()
}

pub fn _assert_open_order(
    order_id: u256,
    sender: ContractAddress,
    order_data: Bytes,
    balances_before: Array<u256>,
    user: ContractAddress,
    setup: Setup,
) {
    _assert_open_order_native_option(
        order_id, sender, order_data, balances_before, user, false, setup,
    );
}

pub fn _assert_open_order_native_option(
    order_id: u256,
    sender: ContractAddress,
    order_data: Bytes,
    balances_before: Array<u256>,
    user: ContractAddress,
    native: bool,
    setup: Setup,
) {
    let saved_order_data = _order_data_by_id(order_id, setup.clone());

    assert(setup.base_full.is_valid_nonce(sender, 1) == false, 'nonce shd be invalid');
    assert(saved_order_data == order_data, 'order data does not match');
    _assert_order(
        order_id,
        order_data,
        balances_before,
        setup.input_token,
        user,
        setup.base.contract_address,
        setup.base_full.OPENED(),
        native,
        setup,
    );
}

pub fn _balance_id(user: ContractAddress, setup: Setup) -> usize {
    let kaka = setup.kaka.account.contract_address;
    let karp = setup.karp.account.contract_address;
    let veg = setup.veg.account.contract_address;
    let counter_part = setup.counterpart;
    let base = setup.base.contract_address;

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


pub fn _assert_order(
    order_id: u256,
    order_data: Bytes,
    balances_before: Array<u256>,
    token: IERC20Dispatcher,
    sender: ContractAddress,
    to: ContractAddress,
    expected_status: felt252,
    native: bool,
    setup: Setup,
) {
    let saved_order_data = _order_data_by_id(order_id, setup.clone());
    let status = setup.base_full.order_status(order_id);

    assert(saved_order_data == order_data, 'order data does not match');
    assert_eq!(status, expected_status);

    let balances_after = match native {
        true => _eth_balances(setup.users.clone()),
        false => _balances(token, setup.users.clone()),
    };
    assert_eq!(
        *balances_before.at(_balance_id(sender, setup.clone())) - setup.amount,
        *balances_after.at(_balance_id(sender, setup.clone())),
    );
    assert_eq!(
        *balances_before.at(_balance_id(to, setup.clone())) + setup.amount,
        *balances_after.at(_balance_id(to, setup)),
    );
}

