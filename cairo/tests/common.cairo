use alexandria_bytes::{Bytes, BytesTrait};
use openzeppelin_account::interface::AccountABIDispatcher;
use openzeppelin_token::erc20::interface::{IERC20Dispatcher};
use snforge_std::signature::stark_curve::{
    StarkCurveKeyPairImpl, StarkCurveSignerImpl, StarkCurveVerifierImpl,
};
use snforge_std::signature::{KeyPair, KeyPairTrait};
use snforge_std::{Event, ContractClassTrait, DeclareResultTrait, declare};
use starknet::{ContractAddress, ClassHash};
use starknet::event::Event as _Event;
use oif_starknet::erc7683::interface::{Base7683ABIDispatcher as IHyperlaneDispatcher};
use crate::mocks::mock_base7683::{IMockBase7683Dispatcher};
use crate::mocks::mock_basic_swap7683::{IMockBasicSwap7683Dispatcher};
use crate::mocks::mock_hyperlane7683::{IMockHyperlane7683Dispatcher};
use crate::mocks::interfaces::{IMintableDispatcher, IMintableDispatcherTrait};
use crate::mocks::mock_hyperlane_environment::IMockHyperlaneEnvironmentDispatcher;
use mocks::test_interchain_gas_payment::ITestInterchainGasPaymentDispatcher;


/// Consts ///

pub fn ETH_ADDRESS() -> ContractAddress {
    0x049D36570D4e46f48e99674bd3fcc84644DdD6b96F7C741B1562B82f9e004dC7.try_into().unwrap()
}

/// Accounts ///

#[derive(Drop, Copy)]
pub struct Account {
    pub account: AccountABIDispatcher,
    pub key_pair: KeyPair<felt252, felt252>,
}

pub fn generate_account() -> Account {
    let mock_account_contract = declare("MockAccount").unwrap().contract_class();
    let key_pair = KeyPairTrait::<felt252, felt252>::generate();
    let (account_address, _) = mock_account_contract.deploy(@array![key_pair.public_key]).unwrap();
    let account = AccountABIDispatcher { contract_address: account_address };
    Account { account, key_pair }
}

pub fn deal_eth(to: ContractAddress, amount: u256) {
    IMintableDispatcher { contract_address: ETH_ADDRESS() }.mint(to, amount);
}

pub fn deal(token: ContractAddress, to: ContractAddress, amount: u256) {
    IMintableDispatcher { contract_address: token }.mint(to, amount);
}

pub fn deal_multiple(tokens: Array<ContractAddress>, tos: Array<ContractAddress>, amount: u256) {
    let mut i = 1;
    for token in tokens.span() {
        let mut j = 0;
        for to in tos.span() {
            deal(*token, *to, i * (amount + j));
            j += 1;
        };
        i += 1;
    };
}

/// Contract Deployment ///

pub fn deploy_eth() -> IERC20Dispatcher {
    let mock_erc20_contract = declare("MockERC20").unwrap().contract_class();
    let mut ctor_calldata: Array<felt252> = array![];
    let name: ByteArray = "Ethereum";
    let symbol: ByteArray = "ETH";
    name.serialize(ref ctor_calldata);
    symbol.serialize(ref ctor_calldata);

    let (erc20_address, _) = mock_erc20_contract.deploy_at(@ctor_calldata, ETH_ADDRESS()).unwrap();
    IERC20Dispatcher { contract_address: erc20_address }
}

pub fn deploy_erc20(name: ByteArray, symbol: ByteArray) -> IERC20Dispatcher {
    let mock_erc20_contract = declare("MockERC20").unwrap().contract_class();
    let mut ctor_calldata: Array<felt252> = array![];
    name.serialize(ref ctor_calldata);
    symbol.serialize(ref ctor_calldata);

    let (erc20_address, _) = mock_erc20_contract.deploy(@ctor_calldata).unwrap();

    IERC20Dispatcher { contract_address: erc20_address }
}

pub fn deploy_permit2() -> ContractAddress {
    let mock_permit2_contract = declare("MockPermit2").unwrap().contract_class();
    let (mock_permit2_address, _) = mock_permit2_contract
        .deploy(@array![])
        .expect('mock permit2 deployment failed');

    mock_permit2_address
}

pub fn deploy_mock_base7683(
    permit2: ContractAddress,
    local: u32,
    remote: u32,
    input_token: ContractAddress,
    output_token: ContractAddress,
) -> IMockBase7683Dispatcher {
    let contract = declare("MockBase7683").unwrap().contract_class();
    let mut ctor_calldata: Array<felt252> = array![];
    permit2.serialize(ref ctor_calldata);
    local.serialize(ref ctor_calldata);
    remote.serialize(ref ctor_calldata);
    input_token.serialize(ref ctor_calldata);
    output_token.serialize(ref ctor_calldata);

    let (contract_address, _) = contract
        .deploy(@ctor_calldata)
        .expect('mock permit2 deployment failed');

    IMockBase7683Dispatcher { contract_address }
}

pub fn deploy_mock_basic_swap7683(permit2: ContractAddress) -> IMockBasicSwap7683Dispatcher {
    let contract = declare("MockBasicSwap7683").unwrap().contract_class();
    let mut ctor_calldata: Array<felt252> = array![];
    permit2.serialize(ref ctor_calldata);

    let (contract_address, _) = contract.deploy(@ctor_calldata).expect('mock basic swap failed');

    IMockBasicSwap7683Dispatcher { contract_address }
}

pub fn deploy_mock_hyperlane7683(
    permit2: ContractAddress,
    mailbox: ContractAddress,
    owner: ContractAddress,
    hook: ContractAddress,
    ism: ContractAddress,
) -> IMockHyperlane7683Dispatcher {
    let contract = declare("MockHyperlane7683").unwrap().contract_class();
    let mut ctor_calldata: Array<felt252> = array![];
    permit2.serialize(ref ctor_calldata);
    mailbox.serialize(ref ctor_calldata);
    owner.serialize(ref ctor_calldata);
    hook.serialize(ref ctor_calldata);
    ism.serialize(ref ctor_calldata);

    let (contract_address, _) = contract.deploy(@ctor_calldata).expect('mock hyperlane failed');

    IMockHyperlane7683Dispatcher { contract_address }
}

pub fn deploy_hyperlane7683(
    permit2: ContractAddress,
    mailbox: ContractAddress,
    owner: ContractAddress,
    hook: ContractAddress,
    ism: ContractAddress,
) -> IHyperlaneDispatcher {
    let contract = declare("Hyperlane7683").unwrap().contract_class();
    let mut ctor_calldata: Array<felt252> = array![];
    permit2.serialize(ref ctor_calldata);
    mailbox.serialize(ref ctor_calldata);
    owner.serialize(ref ctor_calldata);
    hook.serialize(ref ctor_calldata);
    ism.serialize(ref ctor_calldata);

    let (contract_address, _) = contract.deploy(@ctor_calldata).expect('mock hyperlane failed');

    IHyperlaneDispatcher { contract_address }
}

pub fn deploy_environment(
    origin: u32, destination: u32, mailbox_class_hash: ClassHash, ism_class_hash: ClassHash,
) -> IMockHyperlaneEnvironmentDispatcher {
    let contract = declare("MockHyperlaneEnvironment").unwrap().contract_class();
    let mut ctor_calldata: Array<felt252> = array![];
    origin.serialize(ref ctor_calldata);
    destination.serialize(ref ctor_calldata);
    mailbox_class_hash.serialize(ref ctor_calldata);
    ism_class_hash.serialize(ref ctor_calldata);

    let (contract_address, _) = contract.deploy(@ctor_calldata).expect('mock env failed');

    IMockHyperlaneEnvironmentDispatcher { contract_address }
}

pub fn deploy_igp() -> ITestInterchainGasPaymentDispatcher {
    let contract = declare("TestInterchainGasPayment").unwrap().contract_class();
    let (contract_address, _) = contract.deploy(@array![]).expect('mock igp env failed');

    ITestInterchainGasPaymentDispatcher { contract_address }
}

pub fn declare_mock_mailbox() -> ClassHash {
    let contract = declare("MockMailbox").unwrap().contract_class();
    *contract.class_hash
}

pub fn declare_test_ism() -> ClassHash {
    let contract = declare("TestISM").unwrap().contract_class();
    *contract.class_hash
}

/// Utils ///

pub fn pop_event<T, +Drop<T>, +Default<T>, +PartialEq<T>, impl TEvent: starknet::Event<T>>(
    target: ContractAddress, selector: felt252, events: Array<(ContractAddress, Event)>,
) -> Option<T> {
    let mut popped: Option<T> = Option::None;
    for (source, e) in events {
        if (source == target) {
            let Event { mut keys, mut data } = e;
            if (*keys[0] == selector) {
                let _ = keys.pop_front();
                let mut keys = keys.span();
                let mut data = data.span();
                popped =
                    Option::Some(
                        _Event::<T>::deserialize(ref keys, ref data)
                            .expect('Failed to build event'),
                    );
            }
        }
    };
    popped
}

/// Impls ///

pub impl ContractAddressIntoBytes of Into<ContractAddress, Bytes> {
    fn into(self: ContractAddress) -> Bytes {
        let mut bytes = BytesTrait::new_empty();
        bytes.append_address(self.into());
        bytes
    }
}

