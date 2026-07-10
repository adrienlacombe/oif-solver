/// @title BasicSwap7683 (Cairo)
/// @author BootNode (translation by Nethermind)
/// @notice This contract builds on top of Base7683 as a second layer, implementing logic to handle
/// a specific type of order for swapping a single token.
/// @dev This is a component, intended to be injected into a third contract that will function as
/// the messaging layer.
#[starknet::component]
pub mod BasicSwap7683Component {
    use alexandria_bytes::{Bytes, BytesStore};
    use core::num::traits::{Bounded, Zero};
    use oif_starknet::base7683::Base7683Component;
    use oif_starknet::base7683::Base7683Component::{OPENED, Virtual as BaseVirtual};
    use oif_starknet::erc7683::interface::{
        FillInstruction, GaslessCrossChainOrder, OnchainCrossChainOrder, Output,
        ResolvedCrossChainOrder, IBasicSwapExtra,
    };
    use oif_starknet::libraries::order_encoder::{OpenOrderEncoder, OrderData, OrderEncoder};
    use openzeppelin_token::erc20::interface::{IERC20Dispatcher, IERC20DispatcherTrait};
    use starknet::storage::{StoragePathEntry, StoragePointerReadAccess, StoragePointerWriteAccess};
    use starknet::{ContractAddress, get_block_timestamp, get_caller_address};

    /// CONSTANTS ///
    pub const SETTLED: felt252 = 'SETTLED';
    pub const REFUNDED: felt252 = 'REFUNDED';

    /// ERRORS ///
    pub mod Errors {
        pub fn INVALID_ORDER_TYPE(order_type: u256) {
            panic!("Invalid order type: {order_type}");
        }

        pub fn INVALID_ORIGIN_DOMAIN(origin_domain: u32) {
            panic!("Invalid origin domain: {origin_domain}");
        }
        pub const INVALID_ORDER_ID: felt252 = 'Invalid order ID';
        pub const ORDER_FILL_EXPIRED: felt252 = 'Order fill expired';
        pub const INVALID_ORDER_DOMAIN: felt252 = 'Invalid order domain';
        pub const INVALID_SENDER: felt252 = 'Invalid sender';
    }

    /// STORAGE ///
    #[storage]
    pub struct Storage {}

    /// EVENTS ///
    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        Settled: Settled,
        Refunded: Refunded,
    }

    /// Emitted when an order is settled.
    /// @param order_id: The ID of the settled order.
    /// @param receiver: The address of the order's input token receiver.
    #[derive(Drop, starknet::Event)]
    pub struct Settled {
        pub order_id: u256,
        pub receiver: ContractAddress,
    }


    /// Emitted when an order is refunded.
    /// @param order_id: The ID of the refunded order.
    /// @param receiver: The address of the order's input token receiver.
    #[derive(Drop, starknet::Event)]
    pub struct Refunded {
        pub order_id: u256,
        pub receiver: ContractAddress,
    }

    /// VIRTUAL ///
    pub trait Virtual<
        TContractState,
        +BaseVirtual<TContractState>,
        impl Base7683: Base7683Component::HasComponent<TContractState>,
        +HasComponent<TContractState>,
        +Drop<TContractState>,
    > {
        /// @dev Handles settling an individual order, should be called by the inheriting contract
        /// when receiving a setting instruction from a remote chain.
        ///
        /// Parameters:
        /// -`message_origin`: The domain from which the message originates.
        /// -`message_sender`: The address of the sender on the origin domain.
        /// -`order_id`: The ID of the order to settle.
        /// -`receiver`: The receiver address.
        fn _handle_settle_order(
            ref self: ComponentState<TContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
            receiver: ContractAddress,
        );

        /// @dev Handles refunding an individual order, should be called by the inheriting contract
        /// when receiving a refunding instruction from a remote chain.
        /// Parameters:
        /// - `message_origin`: The domain from which the message originates.
        /// - `message_sender`: The address of the sender on the origin domain.
        /// - `order_id`: The ID of the order to refund.
        fn _handle_refund_order(
            ref self: ComponentState<TContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
        );

        /// @dev Should be implemented by the messaging layer for dispatching a settlement
        /// instruction the remote domain where the orders where created.
        ///
        /// Parameters:
        /// - `origin_domain`: The origin domain of the orders.
        /// - `order_ids`: The IDs of the orders to settle.
        /// - `orders_filler_data`: The filler data for the orders.
        fn _dispatch_settle(
            ref self: ComponentState<TContractState>,
            origin_domain: u32,
            order_ids: @Array<u256>,
            orders_filler_data: @Array<Bytes>,
            value: u256,
        );

        /// @dev Should be implemented by the messaging layer for dispatching a refunding
        /// instruction the remote domain where the orders where created.
        ///
        /// Parameters:
        /// - `origin_domain`: The origin domain of the orders.
        /// - `order_ids`: The IDs of the orders to refund.
        fn _dispatch_refund(
            ref self: ComponentState<TContractState>,
            origin_domain: u32,
            order_ids: @Array<u256>,
            value: u256,
        );
    }

    /// INTERNAL ///
    #[generate_trait]
    pub impl InternalImpl<
        TContractState,
        impl Base7683: Base7683Component::HasComponent<TContractState>,
        impl BasicSwap7683: HasComponent<TContractState>,
        +Drop<TContractState>,
        +BaseVirtual<TContractState>,
        +Virtual<TContractState>,
    > of InternalTrait<TContractState> {
        /// This is the default implementation of BasicSwap7683::Virtual::_handle_settle_order
        /// @dev Due to virtual components not being able to call internal functions, the
        /// implementing contract will need to either override the virtual function with custom
        /// logic or call this function directly.
        fn _handle_settle_order(
            ref self: ComponentState<TContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
            receiver: ContractAddress,
        ) {
            // self._initialize('asdf'.try_into().unwrap());
            let (is_elgible, order_data) = self
                ._check_order_elgibility(message_origin, message_sender, order_id);

            if (!is_elgible) {
                return;
            }

            let mut base7683_component = get_dep_component_mut!(ref self, Base7683);
            base7683_component.order_status.entry(order_id).write(SETTLED);

            self._transfer_token_out(order_data.input_token, receiver, order_data.amount_in);

            self.emit(Settled { order_id, receiver });
        }

        /// This is the default implementation of BasicSwap7683::Virtual::_handle_refund_order
        /// @dev Due to virtual components not being able to call internal functions, the
        /// implementing contract will need to either override the virtual function with custom
        /// logic or call this function directly.
        fn _handle_refund_order(
            ref self: ComponentState<TContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
        ) {
            let (is_elgible, order_data) = self
                ._check_order_elgibility(message_origin, message_sender, order_id);

            if (!is_elgible) {
                return;
            }

            let mut base7683_component = get_dep_component_mut!(ref self, Base7683);
            base7683_component.order_status.entry(order_id).write(REFUNDED);

            self
                ._transfer_token_out(
                    order_data.input_token, order_data.sender, order_data.amount_in,
                );

            self.emit(Refunded { order_id, receiver: order_data.sender });
        }


        /// Internal

        /// Checks if order is eligible for settlement or refund .
        /// @dev Order must be OPENED and the message was sent from the appropriated chain and
        /// contract.
        ///
        /// Parameters:
        /// - `message_origin`: The origin domain of the message.
        /// - `message_dender`: The sender identifier of the message.
        /// - `order_id`: The unique identifier of the order.
        ///
        /// Returns: A boolean indicating if the order is valid, and the decoded OrderData
        /// structure.
        fn _check_order_elgibility(
            ref self: ComponentState<TContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
        ) -> (bool, OrderData) {
            let mut order: OrderData = Default::default();

            let mut base7683_component = get_dep_component_mut!(ref self, Base7683);
            let order_status = base7683_component.order_status.entry(order_id).read();
            if (order_status != OPENED) {
                return (false, order);
            }

            let open_order_details: Bytes = base7683_component.open_orders.entry(order_id).read();
            let (_, order_data): (u256, Bytes) = open_order_details.decode();

            order = OrderEncoder::decode(@order_data);

            if (order.destination_domain != message_origin
                || order.destination_settler != message_sender) {
                return (false, order);
            }

            return (true, order);
        }

        /// @dev If _token is the zero address, transfers ETH using a safe method; otherwise,
        /// performs an ERC20 token transfer.
        ///
        /// Parameters:
        /// - `token`: The address of the token to transfer (use address(0) for ETH).
        /// - `to`: The recipient address.
        /// - `amount`: The amount of tokens or ETH to transfer.
        fn _transfer_token_out(
            ref self: ComponentState<TContractState>,
            token: ContractAddress,
            to: ContractAddress,
            amount: u256,
        ) {
            IERC20Dispatcher { contract_address: token }.transfer(to, amount);
        }

        /// @dev Resolves an order into a ResolvedCrossChainOrder structure.
        ///
        /// Parameters:
        /// - `order_type`: The type of the order.
        /// - `sender`: The sender of the order.
        /// - `open_deadline`: The open deadline of the order.
        /// - `fill_deadline`: The fill deadline of the order.
        /// - `order_data`: The data of the order.
        ///
        /// Returns: A tuple containing:
        /// - A ResolvedCrossChainOrder structure.
        /// - The order ID.
        /// - The order nonce.
        fn _resolved_order(
            self: @Base7683Component::ComponentState<TContractState>,
            order_data_type: u256,
            sender: ContractAddress,
            open_deadline: u64,
            fill_deadline: u64,
            order_data: @Bytes,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            if (order_data_type != OrderEncoder::ORDER_DATA_TYPE_HASH) {
                Errors::INVALID_ORDER_TYPE(order_data_type);
            }

            let mut order = OrderEncoder::decode(order_data);
            let local_domain = BaseVirtual::_local_domain(self);
            if (order.origin_domain != local_domain) {
                Errors::INVALID_ORIGIN_DOMAIN(order.origin_domain);
            }

            order.fill_deadline = fill_deadline;
            order.sender = sender;

            let max_spent = array![
                Output {
                    token: order.output_token,
                    amount: order.amount_out,
                    recipient: order.destination_settler,
                    chain_id: order.destination_domain,
                },
            ];

            let min_received = array![
                Output {
                    token: order.input_token,
                    amount: order.amount_in,
                    recipient: Zero::<ContractAddress>::zero(),
                    chain_id: order.origin_domain,
                },
            ];

            let fill_instructions = array![
                FillInstruction {
                    destination_chain_id: order.destination_domain,
                    destination_settler: order.destination_settler,
                    origin_data: OrderEncoder::encode(@order),
                },
            ];

            let order_id = OrderEncoder::id(@order);
            let nonce = order.sender_nonce;

            let resolved_order = ResolvedCrossChainOrder {
                user: sender,
                origin_chain_id: local_domain,
                open_deadline,
                fill_deadline,
                order_id,
                min_received,
                max_spent,
                fill_instructions,
            };

            return (resolved_order, order_id, nonce);
        }


        /// OVERRIDES ///

        /// @dev Computes the ID of an order given its type and data.
        ///
        /// Parameters
        /// - `order_type`: The type of the order.
        /// - `order_data`: The data of the order.
        ///
        /// Returns: The computed order ID.
        fn _get_order_id(
            self: @ComponentState<TContractState>, order_data_type: u256, order_data: Bytes,
        ) -> u256 {
            if (order_data_type != OrderEncoder::ORDER_DATA_TYPE_HASH) {
                Errors::INVALID_ORDER_TYPE(order_data_type);
            }
            let order: OrderData = OrderEncoder::decode(@order_data);
            OrderEncoder::id(@order)
        }


        /// Gets the ID of a GaslessCrossChainOrder
        fn _get_gasless_order_id(
            self: @ComponentState<TContractState>, order: @GaslessCrossChainOrder,
        ) -> u256 {
            self._get_order_id(*order.order_data_type, order.order_data.clone())
        }

        /// Gets the ID of an OnchainCrossChainOrder
        fn _get_onchain_order_id(
            self: @ComponentState<TContractState>, order: @OnchainCrossChainOrder,
        ) -> u256 {
            self._get_order_id(*order.order_data_type, order.order_data.clone())
        }


        /// @dev Fills an order on the current domain.
        ///
        /// Parameters:
        ///
        /// - `order_id`:  The ID of the order to fill.
        /// - `origin_data`: The origin data of the order.
        fn _fill_order(
            ref self: Base7683Component::ComponentState<TContractState>,
            order_id: u256,
            origin_data: @Bytes,
            filler_data: @Bytes,
        ) {
            let order = OrderEncoder::decode(origin_data);

            assert(order_id == OrderEncoder::id(@order), Errors::INVALID_ORDER_ID);
            assert(get_block_timestamp() < order.fill_deadline, Errors::ORDER_FILL_EXPIRED);
            assert(order.destination_domain == self._local_domain(), Errors::INVALID_ORDER_DOMAIN);

            IERC20Dispatcher { contract_address: order.output_token }
                .transfer_from(get_caller_address(), order.recipient, order.amount_out);
        }

        /// OVERRIDES (ACTUAL) ///
        /// @dev Settles multiple orders by dispatching the settlement instructions.
        /// The proper status of all the orders (filled) is validated on the Base7683 before calling
        /// this function. It assumes that all orders were originated in the same originDomain so
        /// it uses the the one from the first one for dispatching the message, but if some order
        /// differs on the originDomain it can be re-settle later.
        ///
        /// Paramters:
        /// -  `order_ids`: The IDs of the orders to settle.
        /// -  `orders_origin_data`: The original data of the orders.
        /// -  `orders_filler_data`: The filler data for the orders.
        fn _settle_orders(
            ref self: ComponentState<TContractState>,
            order_ids: @Array<u256>,
            orders_origin_data: @Array<Bytes>,
            orders_filler_data: @Array<Bytes>,
            value: u256,
        ) {
            self
                ._dispatch_settle(
                    OrderEncoder::decode(orders_origin_data.at(0)).origin_domain,
                    order_ids,
                    orders_filler_data,
                    value,
                );
        }

        /// @dev Refunds multiple OnchainCrossChain orders by dispatching refund instructions. The
        /// proper status of all the orders (NOT filled and expired) is validated on the Base7683
        /// before calling this function. It assumes that all orders were originated in the same
        /// originDomain so it uses the the one from the first one for dispatching the message, but
        /// if some order differs on the originDomain it can be re-refunded later.
        ///
        /// Parameters:
        /// - `orders`: The orders to refund.
        /// - `order_ids`: The IDs of the orders to refund.
        fn _refund_onchain_orders(
            ref self: ComponentState<TContractState>,
            orders: @Array<OnchainCrossChainOrder>,
            order_ids: @Array<u256>,
            value: u256,
        ) {
            self
                ._dispatch_refund(
                    OrderEncoder::decode(orders.at(0).order_data).origin_domain, order_ids, value,
                );
        }

        /// @dev Refunds multiple GaslessCrossChain orders by dispatching refund instructions. The
        /// proper status of all the orders (NOT filled and expired) is validated on the Base7683
        /// before calling this function. It assumes that all orders were originated in the same
        /// originDomain so it uses the the one from the first one for dispatching the message, but
        /// if some order differs on the originDomain it can be re-refunded later.
        ///
        /// Parameters:
        /// - `orders`: The orders to refund.
        /// - `order_ids`: The IDs of the orders to refund.
        fn _refund_gasless_orders(
            ref self: ComponentState<TContractState>,
            orders: @Array<GaslessCrossChainOrder>,
            order_ids: @Array<u256>,
            value: u256,
        ) {
            self
                ._dispatch_refund(
                    OrderEncoder::decode(orders.at(0).order_data).origin_domain, order_ids, value,
                );
        }

        /// @dev Resolves a GaslessCrossChainOrder.
        ///
        /// Parameters:
        /// - `_order`: The GaslessCrossChainOrder to resolve.
        /// - `origin_filler_data` (NOT USED): Any filler-defined data required by the settler
        ///
        /// Returns: A tuple containing:
        /// - A ResolvedCrossChainOrder structure.
        /// - The order ID.
        /// - The order nonce.
        fn _resolve_gasless_order(
            self: @Base7683Component::ComponentState<TContractState>,
            order: @GaslessCrossChainOrder,
            origin_filler_data: @Bytes,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            self
                ._resolved_order(
                    *order.order_data_type,
                    *order.user,
                    *order.open_deadline,
                    *order.fill_deadline,
                    order.order_data,
                )
        }

        /// @dev Resolves a OnchainCrossChainOrder.
        ///
        /// Parameters:
        /// - `_order`: The OnchainCrossChainOrder to resolve.
        ///
        /// Returns: A tuple containing:
        /// - A ResolvedCrossChainOrder structure.
        /// - The order ID.
        /// - The order nonce.
        fn _resolve_onchain_order(
            self: @Base7683Component::ComponentState<TContractState>,
            order: @OnchainCrossChainOrder,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            self
                ._resolved_order(
                    *order.order_data_type,
                    get_caller_address(),
                    Bounded::<u64>::MAX,
                    *order.fill_deadline,
                    order.order_data,
                )
        }
    }

    #[embeddable_as(BasicSwapExtraImpl)]
    pub impl BasicSwapExtra<
        TContractState, +HasComponent<TContractState>,
    > of IBasicSwapExtra<ComponentState<TContractState>> {
        fn REFUNDED(self: @ComponentState<TContractState>) -> felt252 {
            REFUNDED
        }

        fn SETTLED(self: @ComponentState<TContractState>) -> felt252 {
            SETTLED
        }
    }
}

