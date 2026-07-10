use starknet::ContractAddress;

#[starknet::interface]
pub trait IMockPermit2<TState> {
    fn mock_update_amount_and_expiration(
        ref self: TState,
        from: ContractAddress,
        token: ContractAddress,
        spender: ContractAddress,
        amount: u256,
        expiration: u64,
    );

    fn mock_update_all(
        ref self: TState,
        from: ContractAddress,
        token: ContractAddress,
        spender: ContractAddress,
        amount: u256,
        expiration: u64,
        nonce: u64,
    );

    fn use_unordered_nonce(ref self: TState, from: ContractAddress, nonce: felt252);
}

#[starknet::contract]
mod MockPermit2 {
    use permit2::components::allowance_transfer::AllowanceTransferComponent;
    use permit2::components::signature_transfer::SignatureTransferComponent;
    use permit2::components::unordered_nonces::UnorderedNoncesComponent;
    use openzeppelin_utils::cryptography::snip12::{
        StarknetDomain, SNIP12Metadata, StructHash, StructHashStarknetDomainImpl,
    };
    use crate::mocks::interfaces::IDS;


    component!(
        path: AllowanceTransferComponent, storage: allowed_transfer, event: AllowedTransferEvent,
    );
    component!(path: UnorderedNoncesComponent, storage: nonces, event: UnorderedNoncesEvent);
    component!(
        path: SignatureTransferComponent,
        storage: signature_transfer,
        event: SignatureTransferEvent,
    );


    #[abi(embed_v0)]
    impl AllowedTransferImpl =
        AllowanceTransferComponent::AllowanceTransferImpl<ContractState>;

    #[abi(embed_v0)]
    impl SignatureTransferImpl =
        SignatureTransferComponent::SignatureTransferImpl<ContractState>;

    #[abi(embed_v0)]
    impl UnorderedNoncesImpl =
        UnorderedNoncesComponent::UnorderedNoncesImpl<ContractState>;
    impl UnorderedNoncesInternalImpl = UnorderedNoncesComponent::InternalImpl<ContractState>;


    #[storage]
    pub struct Storage {
        #[substorage(v0)]
        allowed_transfer: AllowanceTransferComponent::Storage,
        #[substorage(v0)]
        signature_transfer: SignatureTransferComponent::Storage,
        #[substorage(v0)]
        nonces: UnorderedNoncesComponent::Storage,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    enum Event {
        #[flat]
        AllowedTransferEvent: AllowanceTransferComponent::Event,
        #[flat]
        UnorderedNoncesEvent: UnorderedNoncesComponent::Event,
        #[flat]
        SignatureTransferEvent: SignatureTransferComponent::Event,
    }

    pub impl SNIP12MetadataImpl of SNIP12Metadata {
        /// Returns the name of the SNIP-12 metadata.
        fn name() -> felt252 {
            'Permit2'
        }

        /// Returns the version of the SNIP-12 metadata.
        fn version() -> felt252 {
            'v1'
        }
    }

    #[abi(embed_v0)]
    pub impl IDomainSeparator<impl metadata: SNIP12Metadata> of IDS<ContractState> {
        fn DOMAIN_SEPARATOR(self: @ContractState) -> felt252 {
            let domain = StarknetDomain {
                name: metadata::name(),
                version: metadata::version(),
                chain_id: starknet::get_tx_info().unbox().chain_id,
                revision: 1,
            };

            domain.hash_struct()
        }
    }
}
