#[starknet::contract]
pub mod Hyperlane7683 {
    use alexandria_bytes::{Bytes, BytesTrait};
    use contracts::client::gas_router_component::GasRouterComponent;
    use contracts::client::mailboxclient_component::MailboxclientComponent;
    use contracts::client::router_component::RouterComponent;
    use contracts::client::router_component::RouterComponent::IMessageRecipientInternalHookTrait;
    use oif_starknet::base7683::Base7683Component;
    use oif_starknet::base7683::Base7683Component::{DestinationSettler, OriginSettler};
    use oif_starknet::basic_swap7683::BasicSwap7683Component;
    use oif_starknet::erc7683::interface::{
        GaslessCrossChainOrder, OnchainCrossChainOrder, ResolvedCrossChainOrder,
    };
    use oif_starknet::libraries::hyperlane7683_message::Hyperlane7683Message;
    use openzeppelin_access::ownable::OwnableComponent;
    use openzeppelin_utils::cryptography::snip12::StructHashStarknetDomainImpl;
    use starknet::ContractAddress;

    /// COMPONENT INJECTION ///
    component!(path: Base7683Component, storage: base7683, event: Base7683Event);
    component!(path: BasicSwap7683Component, storage: basic_swap7683, event: BasicSwap7683Event);
    component!(path: OwnableComponent, storage: ownable, event: OwnableEvent);
    component!(path: RouterComponent, storage: router, event: RouterEvent);
    component!(path: GasRouterComponent, storage: gas_router, event: GasRouterEvent);
    component!(path: MailboxclientComponent, storage: mailbox_client, event: MailboxClientEvent);

    /// EXTERNAL ///

    /// Base7683
    #[abi(embed_v0)]
    pub impl OriginSettlerImpl =
        Base7683Component::OriginSettlerImpl<ContractState>;
    #[abi(embed_v0)]
    impl DestinationSettlerImpl =
        Base7683Component::DestinationSettlerImpl<ContractState>;
    #[abi(embed_v0)]
    impl Base7683Extra = Base7683Component::ERC7683ExtraImpl<ContractState>;
    impl BaseInternalImpl = Base7683Component::InternalImpl<ContractState>;

    /// BasicSwap7683
    #[abi(embed_v0)]
    impl BasicSwapExtraImpl =
        BasicSwap7683Component::BasicSwapExtraImpl<ContractState>;
    impl BasicSwapInternalImpl = BasicSwap7683Component::InternalImpl<ContractState>;


    // Ownable
    #[abi(embed_v0)]
    impl OwnableImpl = OwnableComponent::OwnableImpl<ContractState>;
    impl OwnableInternalImpl = OwnableComponent::InternalImpl<ContractState>;

    /// Mailbox Client
    #[abi(embed_v0)]
    impl MailboxClientImpl =
        MailboxclientComponent::MailboxClientImpl<ContractState>;
    impl MailboxClientInternalImpl =
        MailboxclientComponent::MailboxClientInternalImpl<ContractState>;

    /// Router
    #[abi(embed_v0)]
    impl RouterImpl = RouterComponent::RouterImpl<ContractState>;
    impl RouterInternalImpl = RouterComponent::RouterComponentInternalImpl<ContractState>;

    /// Gas Router
    #[abi(embed_v0)]
    impl GasRouterImpl = GasRouterComponent::GasRouterImpl<ContractState>;
    impl GasRouterInternalImpl = GasRouterComponent::GasRouterInternalImpl<ContractState>;

    /// STORAGE ///
    #[storage]
    pub struct Storage {
        #[substorage(v0)]
        base7683: Base7683Component::Storage,
        #[substorage(v0)]
        #[allow(starknet::colliding_storage_paths)]
        basic_swap7683: BasicSwap7683Component::Storage,
        #[substorage(v0)]
        ownable: OwnableComponent::Storage,
        #[substorage(v0)]
        router: RouterComponent::Storage,
        #[substorage(v0)]
        gas_router: GasRouterComponent::Storage,
        #[substorage(v0)]
        mailbox_client: MailboxclientComponent::Storage,
    }

    /// CONSTRUCTOR ///
    #[constructor]
    fn constructor(
        ref self: ContractState,
        permit2: ContractAddress,
        mailbox: ContractAddress,
        owner: ContractAddress,
        hook: ContractAddress,
        interchain_security_module: ContractAddress,
    ) {
        self.ownable.initializer(owner);
        self.base7683._initialize(permit2);
        self
            .mailbox_client
            .initialize(mailbox, Option::Some(hook), Option::Some(interchain_security_module));
    }

    /// EVENTS ///
    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        #[flat]
        Base7683Event: Base7683Component::Event,
        #[flat]
        BasicSwap7683Event: BasicSwap7683Component::Event,
        #[flat]
        OwnableEvent: OwnableComponent::Event,
        #[flat]
        RouterEvent: RouterComponent::Event,
        #[flat]
        GasRouterEvent: GasRouterComponent::Event,
        #[flat]
        MailboxClientEvent: MailboxclientComponent::Event,
    }

    /// BASE OVERRIDES ///
    pub impl Base7683VirtualImpl of Base7683Component::Virtual<ContractState> {
        fn _fill_order(
            ref self: Base7683Component::ComponentState<ContractState>,
            order_id: u256,
            origin_data: @Bytes,
            filler_data: @Bytes,
        ) {
            BasicSwapInternalImpl::_fill_order(ref self, order_id, origin_data, filler_data);
        }

        fn _resolve_onchain_order(
            self: @Base7683Component::ComponentState<ContractState>, order: @OnchainCrossChainOrder,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            BasicSwapInternalImpl::_resolve_onchain_order(self, order)
        }

        fn _resolve_gasless_order(
            self: @Base7683Component::ComponentState<ContractState>,
            order: @GaslessCrossChainOrder,
            origin_filler_data: @Bytes,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            BasicSwapInternalImpl::_resolve_gasless_order(self, order, origin_filler_data)
        }

        fn _settle_orders(
            ref self: Base7683Component::ComponentState<ContractState>,
            order_ids: @Array<u256>,
            orders_origin_data: @Array<Bytes>,
            orders_filler_data: @Array<Bytes>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            BasicSwapInternalImpl::_settle_orders(
                ref self.basic_swap7683, order_ids, orders_origin_data, orders_filler_data, value,
            );
        }


        fn _refund_onchain_orders(
            ref self: Base7683Component::ComponentState<ContractState>,
            orders: @Array<OnchainCrossChainOrder>,
            order_ids: @Array<u256>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            BasicSwapInternalImpl::_refund_onchain_orders(
                ref self.basic_swap7683, orders, order_ids, value,
            );
        }

        fn _refund_gasless_orders(
            ref self: Base7683Component::ComponentState<ContractState>,
            orders: @Array<GaslessCrossChainOrder>,
            order_ids: @Array<u256>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            BasicSwapInternalImpl::_refund_gasless_orders(
                ref self.basic_swap7683, orders, order_ids, value,
            );
        }

        fn _local_domain(self: @Base7683Component::ComponentState<ContractState>) -> u32 {
            MailboxClientImpl::get_local_domain(self.get_contract())
        }

        fn _get_gasless_order_id(
            self: @Base7683Component::ComponentState<ContractState>, order: @GaslessCrossChainOrder,
        ) -> u256 {
            let self = self.get_contract();
            BasicSwapInternalImpl::_get_gasless_order_id(self.basic_swap7683, order)
        }

        fn _get_onchain_order_id(
            self: @Base7683Component::ComponentState<ContractState>, order: @OnchainCrossChainOrder,
        ) -> u256 {
            let self = self.get_contract();
            BasicSwapInternalImpl::_get_onchain_order_id(self.basic_swap7683, order)
        }
    }

    /// BASIC SWAP OVERRIDES ///
    pub impl BasicSwap7683VirtualImpl of BasicSwap7683Component::Virtual<ContractState> {
        /// Dispatches a settlement message to the specified domain.
        /// @dev Encodes the settle message using Hyperlane7683Message and dispatches it via the
        /// GasRouter.
        ///
        /// Parameters:
        /// - `origin_domain`: The domain to which the settlement message is sent.
        /// - `order_ids`: The IDs of the orders to settle.
        /// - `orders_filler_data`: The filler data for the orders.
        fn _dispatch_settle(
            ref self: BasicSwap7683Component::ComponentState<ContractState>,
            origin_domain: u32,
            order_ids: @Array<u256>,
            orders_filler_data: @Array<Bytes>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            self
                .gas_router
                ._Gas_router_dispatch(
                    origin_domain.try_into().unwrap(),
                    value,
                    Hyperlane7683Message::encode_settle(
                        order_ids.span(), orders_filler_data.span(),
                    ),
                    self.mailbox_client.get_hook(),
                );
        }

        /// Dispatches a refund message to the specified domain.
        /// @dev Encodes the refund message using Hyperlane7683Message and dispatches it via the
        /// GasRouter.
        ///
        /// Parameters:
        /// - `origin_domain`: The domain to which the refund message is sent.
        /// - `order_dds`: The IDs of the orders to refund.
        fn _dispatch_refund(
            ref self: BasicSwap7683Component::ComponentState<ContractState>,
            origin_domain: u32,
            order_ids: @Array<u256>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            self
                .gas_router
                ._Gas_router_dispatch(
                    origin_domain.try_into().unwrap(),
                    value,
                    Hyperlane7683Message::encode_refund(order_ids.span()),
                    self.mailbox_client.get_hook(),
                );
        }

        fn _handle_settle_order(
            ref self: BasicSwap7683Component::ComponentState<ContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
            receiver: ContractAddress,
        ) {
            let mut self = self.get_contract_mut();

            BasicSwapInternalImpl::_handle_settle_order(
                ref self.basic_swap7683, message_origin, message_sender, order_id, receiver,
            );
        }

        fn _handle_refund_order(
            ref self: BasicSwap7683Component::ComponentState<ContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
        ) {
            let mut self = self.get_contract_mut();
            BasicSwapInternalImpl::_handle_refund_order(
                ref self.basic_swap7683, message_origin, message_sender, order_id,
            );
        }
    }

    /// MESSAGE RECIPIENT INTERNAL OVERRIDES ///
    impl MessageRecipientInternalHookImpl of IMessageRecipientInternalHookTrait<ContractState> {
        fn _handle(
            ref self: RouterComponent::ComponentState<ContractState>,
            origin: u32,
            sender: u256,
            message: Bytes,
        ) {
            let mut self = self.get_contract_mut();
            let (settle, order_ids, orders_filler_data) = Hyperlane7683Message::decode(message);
            let sender: ContractAddress = TryInto::<u256, felt252>::try_into(sender)
                .expect('Err casting u256 -> felt252')
                .try_into()
                .expect('Err casting felt252 -> Address');

            for i in 0..order_ids.len() {
                match settle {
                    true => {
                        let (_, receiver) = orders_filler_data
                            .get(i)
                            .unwrap()
                            .unbox()
                            .read_address(0);

                        BasicSwap7683VirtualImpl::_handle_settle_order(
                            ref self.basic_swap7683,
                            origin.try_into().unwrap(),
                            sender.try_into().unwrap(),
                            *order_ids.at(i),
                            receiver,
                        );
                    },
                    false => {
                        BasicSwap7683VirtualImpl::_handle_refund_order(
                            ref self.basic_swap7683,
                            origin.try_into().unwrap(),
                            sender.try_into().unwrap(),
                            *order_ids.at(i),
                        );
                    },
                };
            }
        }
    }
}
