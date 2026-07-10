use alexandria_bytes::Bytes;
use starknet::ContractAddress;
use oif_starknet::libraries::order_encoder::{
    OrderData, OrderEncoder, OpenOrderEncoderImpl, OpenOrderEncoderImplAt,
};
use oif_starknet::erc7683::interface::{
    GaslessCrossChainOrder, OnchainCrossChainOrder, ResolvedCrossChainOrder,
};
#[starknet::interface]
pub trait IMockBasicSwap7683<TState> {
    fn fill_order(ref self: TState, order_id: u256, origin_data: Bytes, _empty: Bytes);
    fn settle_orders(
        ref self: TState,
        order_ids: Array<u256>,
        orders_origin_data: Array<Bytes>,
        orders_filler_data: Array<Bytes>,
        value: u256,
    );

    fn refund_gasless_orders(
        ref self: TState,
        orders: Array<GaslessCrossChainOrder>,
        order_ids: Array<u256>,
        value: u256,
    );

    fn refund_onchain_orders(
        ref self: TState,
        orders: Array<OnchainCrossChainOrder>,
        order_ids: Array<u256>,
        value: u256,
    );

    fn resolve_gasless_order(
        self: @TState, order: GaslessCrossChainOrder, _dummy: Bytes,
    ) -> (ResolvedCrossChainOrder, u256, felt252);

    fn resolve_onchain_order(
        self: @TState, order: OnchainCrossChainOrder,
    ) -> (ResolvedCrossChainOrder, u256, felt252);

    fn resolved_order(
        self: @TState,
        order_type: u256,
        sender: ContractAddress,
        open_deadline: u64,
        fill_deadline: u64,
        order_data: Bytes,
    ) -> ResolvedCrossChainOrder;

    fn handle_settle_order(
        ref self: TState,
        message_origin: u32,
        message_sender: ContractAddress,
        order_id: u256,
        receiver: ContractAddress,
    );

    fn handle_refund_order(
        ref self: TState, message_origin: u32, message_sender: ContractAddress, order_id: u256,
    );

    fn set_order_opened(ref self: TState, order_id: u256, order_data: OrderData);

    fn get_gasless_order_id(self: @TState, order: GaslessCrossChainOrder) -> u256;

    fn get_onchain_order_id(self: @TState, order: OnchainCrossChainOrder) -> u256;

    fn dispatched_origin_domain(self: @TState) -> u32;

    fn dispatched_order_ids(self: @TState) -> Array<u256>;

    fn dispatched_orders_filler_data(self: @TState) -> Array<Bytes>;
}

#[starknet::contract]
pub mod MockBasicSwap7683 {
    use alexandria_bytes::{Bytes, BytesStore};
    use core::keccak::compute_keccak_byte_array;
    use oif_starknet::base7683::Base7683Component;
    use oif_starknet::base7683::Base7683Component::{DestinationSettler, OriginSettler};
    use oif_starknet::basic_swap7683::BasicSwap7683Component;
    use oif_starknet::erc7683::interface::{
        GaslessCrossChainOrder, OnchainCrossChainOrder, ResolvedCrossChainOrder,
    };
    use openzeppelin_utils::cryptography::snip12::StructHashStarknetDomainImpl;
    use starknet::ContractAddress;
    use starknet::storage::{
        Map, StoragePathEntry, StoragePointerReadAccess, StoragePointerWriteAccess,
    };
    use super::{*};

    /// COMPONENT INJECTION ///
    component!(path: Base7683Component, storage: base7683, event: Base7683Event);
    component!(path: BasicSwap7683Component, storage: basic_swap7683, event: BasicSwap7683Event);

    /// EXTERNAL ///
    #[abi(embed_v0)]
    pub impl OriginSettlerImpl =
        Base7683Component::OriginSettlerImpl<ContractState>;
    #[abi(embed_v0)]
    impl DestinationSettlerImpl =
        Base7683Component::DestinationSettlerImpl<ContractState>;
    #[abi(embed_v0)]
    pub impl BaseExtraImpl = Base7683Component::ERC7683ExtraImpl<ContractState>;
    #[abi(embed_v0)]
    pub impl BasicSwapExtraImpl =
        BasicSwap7683Component::BasicSwapExtraImpl<ContractState>;

    /// INTERNAL ///
    impl BaseInternalImpl = Base7683Component::InternalImpl<ContractState>;
    impl BasicSwap7683Impl = BasicSwap7683Component::InternalImpl<ContractState>;

    /// STORAGE ///
    #[storage]
    pub struct Storage {
        dispatched_origin_domain: u32,
        dispatched_order_ids: Map<usize, u256>,
        dispatched_orders_filler_data: Map<usize, Bytes>,
        dispatched_order_ids_len: usize,
        dispatched_orders_filler_data_len: usize,
        /// COMPONENT STORAGE ///
        #[substorage(v0)]
        base7683: Base7683Component::Storage,
        #[substorage(v0)]
        basic_swap7683: BasicSwap7683Component::Storage,
    }

    /// CONSTRUCTOR ///
    #[constructor]
    fn constructor(ref self: ContractState, permit2: ContractAddress) {
        self.base7683._initialize(permit2);
    }

    /// EVENTS ///
    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        #[flat]
        Base7683Event: Base7683Component::Event,
        #[flat]
        BasicSwap7683Event: BasicSwap7683Component::Event,
    }

    /// EXTRA PUBLIC ///
    #[abi(embed_v0)]
    pub impl MockBasicSwap7683Impl of super::IMockBasicSwap7683<ContractState> {
        fn dispatched_origin_domain(self: @ContractState) -> u32 {
            self.dispatched_origin_domain.read()
        }

        fn dispatched_order_ids(self: @ContractState) -> Array<u256> {
            let mut order_ids = array![];
            let len = self.dispatched_order_ids_len.read();
            for i in 0..len {
                order_ids.append(self.dispatched_order_ids.entry(i).read());
            };
            order_ids
        }

        fn dispatched_orders_filler_data(self: @ContractState) -> Array<Bytes> {
            let mut orders_filler_data = array![];
            let len = self.dispatched_orders_filler_data_len.read();
            for i in 0..len {
                orders_filler_data.append(self.dispatched_orders_filler_data.entry(i).read());
            };
            orders_filler_data
        }


        fn fill_order(ref self: ContractState, order_id: u256, origin_data: Bytes, _empty: Bytes) {
            BasicSwap7683Component::InternalImpl::_fill_order(
                ref self.base7683, order_id, @origin_data, @_empty,
            );
        }

        fn settle_orders(
            ref self: ContractState,
            order_ids: Array<u256>,
            orders_origin_data: Array<Bytes>,
            orders_filler_data: Array<Bytes>,
            value: u256,
        ) {
            Base7686VirtualImpl::_settle_orders(
                ref self.base7683, @order_ids, @orders_origin_data, @orders_filler_data, value,
            );
        }

        fn refund_gasless_orders(
            ref self: ContractState,
            orders: Array<GaslessCrossChainOrder>,
            order_ids: Array<u256>,
            value: u256,
        ) {
            self.basic_swap7683._refund_gasless_orders(@orders, @order_ids, value);
        }

        fn refund_onchain_orders(
            ref self: ContractState,
            orders: Array<OnchainCrossChainOrder>,
            order_ids: Array<u256>,
            value: u256,
        ) {
            self.basic_swap7683._refund_onchain_orders(@orders, @order_ids, value);
        }

        fn resolve_gasless_order(
            self: @ContractState, order: GaslessCrossChainOrder, _dummy: Bytes,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            BasicSwap7683Component::InternalImpl::_resolve_gasless_order(
                self.base7683, @order, @_dummy,
            )
        }

        fn resolve_onchain_order(
            self: @ContractState, order: OnchainCrossChainOrder,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            BasicSwap7683Component::InternalImpl::_resolve_onchain_order(self.base7683, @order)
        }

        fn resolved_order(
            self: @ContractState,
            order_type: u256,
            sender: ContractAddress,
            open_deadline: u64,
            fill_deadline: u64,
            order_data: Bytes,
        ) -> ResolvedCrossChainOrder {
            let (resolved_order, _, _) = BasicSwap7683Component::InternalImpl::_resolved_order(
                self.base7683, order_type, sender, open_deadline, fill_deadline, @order_data,
            );
            resolved_order
        }

        fn handle_settle_order(
            ref self: ContractState,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
            receiver: ContractAddress,
        ) {
            BasicSwap7683Component::InternalImpl::_handle_settle_order(
                ref self.basic_swap7683, message_origin, message_sender, order_id, receiver,
            )
        }

        fn handle_refund_order(
            ref self: ContractState,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
        ) {
            BasicSwap7683Component::InternalImpl::_handle_refund_order(
                ref self.basic_swap7683, message_origin, message_sender, order_id,
            )
        }

        fn set_order_opened(ref self: ContractState, order_id: u256, order_data: OrderData) {
            let order = OrderEncoder::encode(@order_data);
            let order_data_type = OrderEncoder::order_data_type_hash();
            let order_as_bytes = (order_data_type, order).encode();

            self.base7683.open_orders.entry(order_id).write(order_as_bytes);
            self.base7683.order_status.entry(order_id).write(Base7683Component::OPENED);
        }

        fn get_gasless_order_id(self: @ContractState, order: GaslessCrossChainOrder) -> u256 {
            self.basic_swap7683._get_order_id(order.order_data_type, order.order_data)
        }

        fn get_onchain_order_id(self: @ContractState, order: OnchainCrossChainOrder) -> u256 {
            self.basic_swap7683._get_order_id(order.order_data_type, order.order_data)
        }
    }

    /// BASE OVERRIDES ///
    pub impl Base7686VirtualImpl of Base7683Component::Virtual<ContractState> {
        fn _fill_order(
            ref self: Base7683Component::ComponentState<ContractState>,
            order_id: u256,
            origin_data: @Bytes,
            filler_data: @Bytes,
        ) {
            BasicSwap7683Component::InternalImpl::_fill_order(
                ref self, order_id, origin_data, filler_data,
            );
        }

        fn _resolve_onchain_order(
            self: @Base7683Component::ComponentState<ContractState>, order: @OnchainCrossChainOrder,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            BasicSwap7683Component::InternalImpl::_resolve_onchain_order(self, order)
        }

        fn _resolve_gasless_order(
            self: @Base7683Component::ComponentState<ContractState>,
            order: @GaslessCrossChainOrder,
            origin_filler_data: @Bytes,
        ) -> (ResolvedCrossChainOrder, u256, felt252) {
            BasicSwap7683Component::InternalImpl::_resolve_gasless_order(
                self, order, origin_filler_data,
            )
        }

        fn _settle_orders(
            ref self: Base7683Component::ComponentState<ContractState>,
            order_ids: @Array<u256>,
            orders_origin_data: @Array<Bytes>,
            orders_filler_data: @Array<Bytes>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            BasicSwap7683Impl::_settle_orders(
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
            BasicSwap7683Component::InternalImpl::_refund_onchain_orders(
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
            BasicSwap7683Component::InternalImpl::_refund_gasless_orders(
                ref self.basic_swap7683, orders, order_ids, value,
            );
        }

        fn _local_domain(self: @Base7683Component::ComponentState<ContractState>) -> u32 {
            1
        }

        fn _get_gasless_order_id(
            self: @Base7683Component::ComponentState<ContractState>, order: @GaslessCrossChainOrder,
        ) -> u256 {
            compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order.order_data.clone()))
        }

        fn _get_onchain_order_id(
            self: @Base7683Component::ComponentState<ContractState>, order: @OnchainCrossChainOrder,
        ) -> u256 {
            compute_keccak_byte_array(@Into::<Bytes, ByteArray>::into(order.order_data.clone()))
        }
    }

    /// BASIC SWAP OVERRIDES ///
    pub impl BasicSwap7686VirtualImpl of BasicSwap7683Component::Virtual<ContractState> {
        fn _dispatch_settle(
            ref self: BasicSwap7683Component::ComponentState<ContractState>,
            origin_domain: u32,
            order_ids: @Array<u256>,
            orders_filler_data: @Array<Bytes>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            self.dispatched_origin_domain.write(origin_domain);

            self.dispatched_order_ids_len.write(order_ids.len());
            for i in 0..order_ids.len() {
                self.dispatched_order_ids.entry(i).write(*order_ids[i]);
            };

            self.dispatched_orders_filler_data_len.write(orders_filler_data.len());
            for i in 0..orders_filler_data.len() {
                self.dispatched_orders_filler_data.entry(i).write(orders_filler_data[i].clone());
            };
        }

        fn _dispatch_refund(
            ref self: BasicSwap7683Component::ComponentState<ContractState>,
            origin_domain: u32,
            order_ids: @Array<u256>,
            value: u256,
        ) {
            let mut self = self.get_contract_mut();
            self.dispatched_origin_domain.write(origin_domain);

            self.dispatched_order_ids_len.write(order_ids.len());
            for i in 0..order_ids.len() {
                self.dispatched_order_ids.entry(i).write(*order_ids[i]);
            };
        }

        fn _handle_settle_order(
            ref self: BasicSwap7683Component::ComponentState<ContractState>,
            message_origin: u32,
            message_sender: ContractAddress,
            order_id: u256,
            receiver: ContractAddress,
        ) {
            let mut self = self.get_contract_mut();
            BasicSwap7683Component::InternalImpl::_handle_settle_order(
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
            BasicSwap7683Component::InternalImpl::_handle_refund_order(
                ref self.basic_swap7683, message_origin, message_sender, order_id,
            );
        }
    }
}
