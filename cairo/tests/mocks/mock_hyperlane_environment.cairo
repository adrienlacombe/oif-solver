use crate::mocks::mock_mailbox::IMockMailboxDispatcher;
use crate::mocks::test_interchain_gas_payment::ITestInterchainGasPaymentDispatcher;
use crate::mocks::test_ism::ITestISMDispatcher;

#[starknet::interface]
pub trait IMockHyperlaneEnvironment<TState> {
    fn origin_domain(self: @TState) -> u32;
    fn destination_domain(self: @TState) -> u32;
    fn mailboxes(self: @TState, domain: u32) -> IMockMailboxDispatcher;
    fn igps(self: @TState, domain: u32) -> ITestInterchainGasPaymentDispatcher;
    fn isms(self: @TState, domain: u32) -> ITestISMDispatcher;
    fn process_next_pending_message(ref self: TState);
    fn process_next_pending_message_from_destination(ref self: TState);
}

#[starknet::contract]
pub mod MockHyperlaneEnvironment {
    use openzeppelin_access::ownable::interface::{IOwnableDispatcher, IOwnableDispatcherTrait};
    use starknet::storage::{
        Map, StoragePathEntry, StoragePointerReadAccess, StoragePointerWriteAccess,
    };
    use super::*;
    use crate::mocks::mock_mailbox::{IMockMailboxDispatcher, IMockMailboxDispatcherTrait};
    use crate::common::ETH_ADDRESS;
    use crate::mocks::test_interchain_gas_payment::{ITestInterchainGasPaymentDispatcher};
    use crate::mocks::test_ism::{ITestISMDispatcher};
    use starknet::syscalls::{deploy_syscall};
    use starknet::{ClassHash, ContractAddress};


    #[storage]
    pub struct Storage {
        pub origin_domain: u32,
        pub destination_domain: u32,
        pub mailboxes: Map<usize, IMockMailboxDispatcher>,
        pub igps: Map<usize, ITestInterchainGasPaymentDispatcher>,
        pub isms: Map<usize, ITestISMDispatcher>,
    }


    #[constructor]
    fn constructor(
        ref self: ContractState,
        origin_domain: u32,
        destination_domain: u32,
        mailbox_class_hash: ClassHash,
        ism_class_hash: ClassHash,
    ) {
        self.origin_domain.write(origin_domain);
        self.destination_domain.write(destination_domain);

        // Deploy ISMs
        let (_ism_o_addr, _): (ContractAddress, Span<felt252>) = deploy_syscall(
            ism_class_hash, 'some salt', array![].span(), false,
        )
            .expect('ism o failed');
        let (_ism_d_addr, _): (ContractAddress, Span<felt252>) = deploy_syscall(
            ism_class_hash, 'some more salt', array![].span(), false,
        )
            .expect('ism d failed');

        // Deploy Mailboxes
        let (_mailbox_o_addr, _): (ContractAddress, Span<felt252>) = deploy_syscall(
            mailbox_class_hash,
            'some salt',
            array![origin_domain.into(), 0, 0, ETH_ADDRESS().into()].span(),
            false,
        )
            .expect('mailbox o failed');
        let (_mailbox_d_addr, _): (ContractAddress, Span<felt252>) = deploy_syscall(
            mailbox_class_hash,
            'some salt',
            array![destination_domain.into(), 0, 0, ETH_ADDRESS().into()].span(),
            false,
        )
            .expect('mailbox d failed');

        assert(_mailbox_o_addr != _mailbox_d_addr, 'mailboxes must be different');

        let origin_mailbox = IMockMailboxDispatcher { contract_address: _mailbox_o_addr };
        let destination_mailbox = IMockMailboxDispatcher { contract_address: _mailbox_d_addr };
        let origin_ism = ITestISMDispatcher { contract_address: _ism_o_addr };
        let destination_ism = ITestISMDispatcher { contract_address: _ism_d_addr };

        origin_mailbox.add_remote_mail_box(destination_domain, _mailbox_d_addr);
        destination_mailbox.add_remote_mail_box(origin_domain, _mailbox_o_addr);

        origin_mailbox.set_default_ism(_ism_o_addr);
        destination_mailbox.set_default_ism(_ism_d_addr);

        IOwnableDispatcher { contract_address: origin_mailbox.contract_address }
            .transfer_ownership(starknet::get_caller_address());

        IOwnableDispatcher { contract_address: destination_mailbox.contract_address }
            .transfer_ownership(starknet::get_caller_address());

        self.mailboxes.entry(origin_domain).write(origin_mailbox);
        self.mailboxes.entry(destination_domain).write(destination_mailbox);
        self.isms.entry(origin_domain).write(origin_ism);
        self.isms.entry(destination_domain).write(destination_ism);
    }

    #[abi(embed_v0)]
    pub impl MockHyperlaneEnvironment of super::IMockHyperlaneEnvironment<ContractState> {
        fn origin_domain(self: @ContractState) -> u32 {
            self.origin_domain.read()
        }

        fn destination_domain(self: @ContractState) -> u32 {
            self.destination_domain.read()
        }

        fn mailboxes(self: @ContractState, domain: u32) -> IMockMailboxDispatcher {
            self.mailboxes.entry(domain).read()
        }

        fn igps(self: @ContractState, domain: u32) -> ITestInterchainGasPaymentDispatcher {
            self.igps.entry(domain).read()
        }

        fn isms(self: @ContractState, domain: u32) -> ITestISMDispatcher {
            self.isms.entry(domain).read()
        }

        fn process_next_pending_message(ref self: ContractState) {
            self
                .mailboxes
                .entry(self.destination_domain.read())
                .read()
                .process_next_inbound_message();
        }

        fn process_next_pending_message_from_destination(ref self: ContractState) {
            self.mailboxes.entry(self.origin_domain.read()).read().process_next_inbound_message();
        }
    }
}
