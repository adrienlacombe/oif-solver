use core::hash::{HashStateExTrait, HashStateTrait};
use core::poseidon::PoseidonTrait;
use oif_starknet::erc7683::interface::{FillInstruction, Output, ResolvedCrossChainOrder};
use openzeppelin_utils::cryptography::snip12::{SNIP12HashSpanImpl, StructHash};
use permit2::snip12_utils::permits::_U256_TYPE_HASH;
use alexandria_bytes::{Bytes, BytesTrait};


/// @title Base7683 (Cairo)
/// @notice Replicates the ERC7683 standard for cross-chain order resolution, filling, settlement,
/// and refunding in Cairo.
/// @author BootNode (translation by Nethermind)
/// @dev Contains logic for managing orders without requiring specifics of the order data type.
/// Notice that settling and refunding are not described in the ERC7683 but it is included here to
/// provide a common interface for solvers to use.
#[starknet::component]
pub mod Base7683Component {
    use alexandria_bytes::{Bytes, BytesStore};
    use oif_starknet::erc7683::interface::{
        FilledOrder, GaslessCrossChainOrder, IDestinationSettler, IERC7683Extra, IOriginSettler,
        OnchainCrossChainOrder, Open, ResolvedCrossChainOrder,
    };
    use oif_starknet::libraries::order_encoder::OpenOrderEncoder;
    use openzeppelin_token::erc20::interface::{IERC20Dispatcher, IERC20DispatcherTrait};
    use permit2::interfaces::signature_transfer::{
        ISignatureTransferDispatcher, ISignatureTransferDispatcherTrait, PermitBatchTransferFrom,
        SignatureTransferDetails, TokenPermissions,
    };
    use starknet::storage::{
        Map, StoragePathEntry, StoragePointerReadAccess, StoragePointerWriteAccess,
    };
    use starknet::{ContractAddress, get_block_timestamp, get_caller_address, get_contract_address};
    use super::{ResolvedCrossChainOrderStructHash, WITNESS_TYPE_STRING};

    /// CONSTANTS ///
    pub const UNKNOWN: felt252 = 0;
    pub const OPENED: felt252 = 'OPENED';
    pub const FILLED: felt252 = 'FILLED';

    /// ERRORS ///
    pub mod Errors {
        pub const ORDER_OPEN_EXPIRED: felt252 = 'Order open expired';
        pub const INVALID_ORDER_STATUS: felt252 = 'Invalid order status';
        pub const INVALID_GASLESS_ORDER_SETTLER: felt252 = 'Invalid gasless order settler';
        pub const INVALID_NONCE: felt252 = 'Invalid nonce';
        pub const INVALID_GASLESS_ORDER_ORIGIN: felt252 = 'Invalid gasless order origin';
        pub const ORDER_FILL_NOT_EXPIRED: felt252 = 'Order fill not expired';
        pub const INVALID_NATIVE_AMOUNT: felt252 = 'Invalid native amount';
    }

    /// STORAGE ///
    #[storage]
    pub struct Storage {
        pub permit2_address: ContractAddress,
        pub used_nonces: Map<(ContractAddress, felt252), bool>,
        pub open_orders: Map<u256, Bytes>,
        pub filled_orders: Map<u256, FilledOrder>,
        pub order_status: Map<u256, felt252>,
    }

    /// EVENTS ///
    #[event]
    #[derive(Drop, starknet::Event)]
    pub enum Event {
        Filled: Filled,
        Settle: Settle,
        Refund: Refund,
        NonceInvalidation: NonceInvalidation,
        Open: Open,
    }


    /// Emitted when an order is filled.
    /// param order_id: The ID of the filled order.
    /// param origin_data: The origin-specific data for the order.
    /// param filler_data: The filler-specific data for the order.
    #[derive(Drop, starknet::Event)]
    pub struct Filled {
        pub order_id: u256,
        pub origin_data: Bytes,
        pub filler_data: Bytes,
    }

    /// Emitted when a batch of orders is settled.
    /// @param order_ids: The IDs of the orders being settled.
    /// @param orders_filler_data The filler data for the settled orders.
    #[derive(Drop, starknet::Event)]
    pub struct Settle {
        pub order_ids: Array<u256>,
        pub orders_filler_data: Array<Bytes>,
    }

    /// Emitted when a batch of orders is refunded.
    /// @param order_ids: The IDs of the refunded orders.
    #[derive(Drop, starknet::Event, PartialEq)]
    pub struct Refund {
        pub order_ids: Array<u256>,
    }

    /// Emitted when a nonce is invalidated for an address.
    /// @param owner The address whose nonce was invalidated.
    /// @param nonce The invalidated nonce.
    #[derive(Drop, starknet::Event)]
    struct NonceInvalidation {
        #[key]
        owner: ContractAddress,
        nonce: felt252,
    }


    /// PUBLIC ///

    #[embeddable_as(OriginSettlerImpl)]
    pub impl OriginSettler<
        TContractState, +HasComponent<TContractState>, +Virtual<TContractState>,
    > of IOriginSettler<ComponentState<TContractState>> {
        fn open_for(
            ref self: ComponentState<TContractState>,
            order: GaslessCrossChainOrder,
            signature: Array<felt252>,
            origin_filler_data: Bytes,
        ) {
            assert(get_block_timestamp().into() < order.open_deadline, Errors::ORDER_OPEN_EXPIRED);
            assert(
                order.origin_settler == get_contract_address(),
                Errors::INVALID_GASLESS_ORDER_SETTLER,
            );
            assert(
                order.origin_chain_id == self._local_domain(), Errors::INVALID_GASLESS_ORDER_ORIGIN,
            );

            let (mut resolved_order, order_id, nonce) = self
                ._resolve_gasless_order(@order, @origin_filler_data);

            self
                .open_orders
                .entry(order_id)
                .write((order.order_data_type, order.order_data).encode());
            self.order_status.entry(order_id).write(OPENED);
            self._use_nonce(order.user, nonce);

            self
                ._permit_transfer_from(
                    @resolved_order, signature, order.nonce, get_contract_address(),
                );

            self.emit(Open { order_id, resolved_order });
        }

        fn open(ref self: ComponentState<TContractState>, order: OnchainCrossChainOrder) {
            let (mut resolved_order, order_id, nonce) = self._resolve_onchain_order(@order);

            self
                .open_orders
                .entry(order_id)
                .write((order.order_data_type, order.order_data).encode());
            self.order_status.entry(order_id).write(OPENED);
            self._use_nonce(get_caller_address(), nonce);

            for min_received in resolved_order.min_received.span() {
                IERC20Dispatcher { contract_address: *min_received.token }
                    .transfer_from(
                        get_caller_address(), get_contract_address(), *min_received.amount,
                    );
            };

            self.emit(Open { order_id, resolved_order });
        }


        fn resolve_for(
            self: @ComponentState<TContractState>,
            order: GaslessCrossChainOrder,
            origin_filler_data: Bytes,
        ) -> ResolvedCrossChainOrder {
            let (resolved_order, _, _) = self._resolve_gasless_order(@order, @origin_filler_data);

            resolved_order
        }

        fn resolve(
            self: @ComponentState<TContractState>, order: OnchainCrossChainOrder,
        ) -> ResolvedCrossChainOrder {
            let (resolved_order, _, _) = self._resolve_onchain_order(@order);

            resolved_order
        }
    }


    #[embeddable_as(DestinationSettlerImpl)]
    pub impl DestinationSettler<
        TContractState, +HasComponent<TContractState>, +Virtual<TContractState>,
    > of IDestinationSettler<ComponentState<TContractState>> {
        fn fill(
            ref self: ComponentState<TContractState>,
            order_id: u256,
            origin_data: Bytes,
            filler_data: Bytes,
        ) {
            assert(
                self.order_status.entry(order_id).read() == UNKNOWN, Errors::INVALID_ORDER_STATUS,
            );

            self._fill_order(order_id, @origin_data, @filler_data);

            self.order_status.entry(order_id).write(FILLED);
            self
                .filled_orders
                .entry(order_id)
                .write(
                    FilledOrder {
                        filler_data: filler_data.clone(), origin_data: origin_data.clone(),
                    },
                );

            self.emit(Filled { order_id, origin_data, filler_data });
        }
    }


    #[embeddable_as(ERC7683ExtraImpl)]
    pub impl ERC7683Extra<
        TContractState, +HasComponent<TContractState>, +Virtual<TContractState>,
    > of IERC7683Extra<ComponentState<TContractState>> {
        /// READS ///

        fn UNKNOWN(self: @ComponentState<TContractState>) -> felt252 {
            UNKNOWN
        }

        fn OPENED(self: @ComponentState<TContractState>) -> felt252 {
            OPENED
        }

        fn FILLED(self: @ComponentState<TContractState>) -> felt252 {
            FILLED
        }

        fn witness_hash(
            self: @ComponentState<TContractState>, resolved_order: ResolvedCrossChainOrder,
        ) -> felt252 {
            resolved_order.hash_struct()
        }

        fn used_nonces(
            self: @ComponentState<TContractState>, user: ContractAddress, nonce: felt252,
        ) -> bool {
            self.used_nonces.entry((user, nonce)).read()
        }

        fn open_orders(self: @ComponentState<TContractState>, order_id: u256) -> Bytes {
            self.open_orders.entry(order_id).read()
        }

        fn filled_orders(self: @ComponentState<TContractState>, order_id: u256) -> FilledOrder {
            self.filled_orders.entry(order_id).read()
        }

        fn order_status(self: @ComponentState<TContractState>, order_id: u256) -> felt252 {
            self.order_status.entry(order_id).read()
        }

        /// WRITES ///

        fn settle(
            ref self: ComponentState<TContractState>, mut order_ids: Array<u256>, value: u256,
        ) {
            let mut orders_origin_data: Array<Bytes> = array![];
            let mut orders_filler_data: Array<Bytes> = array![];

            for order_id in order_ids.span() {
                assert(
                    self.order_status.entry(*order_id).read() == FILLED,
                    Errors::INVALID_ORDER_STATUS,
                );

                let filled_order = self.filled_orders.entry(*order_id).read();

                orders_origin_data.append(filled_order.origin_data);
                orders_filler_data.append(filled_order.filler_data);
            };

            self._settle_orders(@order_ids, @orders_origin_data, @orders_filler_data, value);

            self.emit(Settle { order_ids, orders_filler_data });
        }

        fn refund_gasless_cross_chain_order(
            ref self: ComponentState<TContractState>,
            orders: Array<GaslessCrossChainOrder>,
            value: u256,
        ) {
            let mut order_ids: Array<u256> = array![];
            for order in orders.span() {
                let order_id = self._get_gasless_order_id(order);
                order_ids.append(order_id);

                assert(
                    self.order_status.entry(order_id).read() == UNKNOWN,
                    Errors::INVALID_ORDER_STATUS,
                );
                assert(
                    get_block_timestamp().into() >= *order.fill_deadline,
                    Errors::ORDER_FILL_NOT_EXPIRED,
                );
            };

            self._refund_gasless_orders(@orders, @order_ids, value);

            self.emit(Refund { order_ids });
        }

        fn refund_onchain_cross_chain_order(
            ref self: ComponentState<TContractState>,
            orders: Array<OnchainCrossChainOrder>,
            value: u256,
        ) {
            let mut order_ids: Array<u256> = array![];

            for order in orders.span() {
                let order_id = self._get_onchain_order_id(order);
                order_ids.append(order_id);

                assert(
                    self.order_status.entry(order_id).read() == UNKNOWN,
                    Errors::INVALID_ORDER_STATUS,
                );
                assert(
                    get_block_timestamp().into() >= *order.fill_deadline,
                    Errors::ORDER_FILL_NOT_EXPIRED,
                );
            };

            self._refund_onchain_orders(@orders, @order_ids, value);

            self.emit(Refund { order_ids });
        }

        fn invalidate_nonces(ref self: ComponentState<TContractState>, nonce: felt252) {
            let owner = get_caller_address();

            self._use_nonce(owner, nonce);
            self.emit(NonceInvalidation { owner, nonce });
        }

        fn is_valid_nonce(
            self: @ComponentState<TContractState>, from: ContractAddress, nonce: felt252,
        ) -> bool {
            !self.used_nonces.entry((from, nonce)).read()
        }
    }

    /// VIRTUAL ///
    pub trait Virtual<TContractState> {
        /// Resolves a GaslessCrossChainOrder into a ResolvedCrossChainOrder.
        /// @dev To be implemented by the inheriting contract. Contains logic specific to the order
        /// type and data.
        ///
        /// Paramters:
        /// - `order`: The GaslessCrossChainOrder to resolve.
        /// - `order_filler_data`: Any filler-defined data required by the settler
        ///
        /// Returns a tuple:
        /// - A ResolvedCrossChainOrder with hydrated data.
        /// - The unique identifier for the order.
        /// - The nonce associated with the order.
        fn _resolve_gasless_order(
            self: @ComponentState<TContractState>,
            order: @GaslessCrossChainOrder,
            origin_filler_data: @Bytes,
        ) -> (ResolvedCrossChainOrder, u256, felt252);

        /// Resolves an OnchainCrossChainOrder into a ResolvedCrossChainOrder.
        /// @dev To be implemented by the inheriting contract. Contains logic specific to the order
        /// type and data.
        ///
        /// Parameters:
        /// - `order`: The OnchainCrossChainOrder to resolve.
        ///
        /// Returns a tuple:
        /// - A ResolvedCrossChainOrder with hydrated data.
        /// - The unique identifier for the order.
        /// - The nonce associated with the order.
        fn _resolve_onchain_order(
            self: @ComponentState<TContractState>, order: @OnchainCrossChainOrder,
        ) -> (ResolvedCrossChainOrder, u256, felt252);

        /// Fills an order with specific origin and filler data.
        /// @dev To be implemented by the inheriting contract. Defines how to process the origin and
        /// filler data.
        ///
        /// Paramters:
        /// - `order_id`: The unique identifier for the order to fill.
        /// - `origin_data`: Data emitted on the origin chain to parameterize the fill.
        /// - `filler_data`: Data provided by the filler, including preferences and additional
        /// information.
        fn _fill_order(
            ref self: ComponentState<TContractState>,
            order_id: u256,
            origin_data: @Bytes,
            filler_data: @Bytes,
        );

        /// Settles a batch of orders using their origin and filler data.
        /// @dev To be implemented by the inheriting contract. Contains the specific logic for
        /// settlement.
        ///
        /// Parameters:
        /// - `order_ids` An array of order IDs to settle.
        /// - `orders_origin_data`: The origin data for the orders being settled.
        /// - `orders_filler_data`: The filler data for the orders being settled.
        fn _settle_orders(
            ref self: ComponentState<TContractState>,
            order_ids: @Array<u256>,
            orders_origin_data: @Array<Bytes>,
            orders_filler_data: @Array<Bytes>,
            value: u256,
        );

        /// Refunds a batch of GaslessCrossChainOrders.
        /// @dev To be implemented by the inheriting contract. Contains logic specific to refunds.
        ///
        /// Paramters:
        /// - `orders`: An array of GaslessCrossChainOrders to refund.
        /// - `order_ids`: An array of IDs for the orders to refund.
        fn _refund_gasless_orders(
            ref self: ComponentState<TContractState>,
            orders: @Array<GaslessCrossChainOrder>,
            order_ids: @Array<u256>,
            value: u256,
        );

        /// Refunds a batch of OnchainCrossChainOrders.
        /// @dev To be implemented by the inheriting contract. Contains logic specific to refunds.
        ///
        /// Paramters:
        /// - `orders`: An array of OnchainCrossChainOrders to refund.
        /// - `order_ids`: An array of IDs for the orders to refund.
        fn _refund_onchain_orders(
            ref self: ComponentState<TContractState>,
            orders: @Array<OnchainCrossChainOrder>,
            order_ids: @Array<u256>,
            value: u256,
        );


        /// Retrieves the local domain identifier.
        /// @dev To be implemented by the inheriting contract. Specifies the logic to determine the
        /// local domain.
        ///
        /// Returns:  The local domain ID.
        fn _local_domain(self: @ComponentState<TContractState>) -> u32;

        /// Computes the unique identifier for a GaslessCrossChainOrder.
        /// @dev To be implemented by the inheriting contract. Specifies the logic to compute the
        /// order ID.
        ///
        /// Parameter:
        /// - `order`: The GaslessCrossChainOrder to compute the ID for.
        ///
        /// Returns: The unique identifier for the order.
        fn _get_gasless_order_id(
            self: @ComponentState<TContractState>, order: @GaslessCrossChainOrder,
        ) -> u256;

        /// Computes the unique identifier for a OnchainCrossChainOrder.
        /// @dev To be implemented by the inheriting contract. Specifies the logic to compute the
        /// order ID.
        ///
        /// Parameter:
        /// - `order`: The OnchainCrossChainOrder to compute the ID for.
        ///
        /// Returns: The unique identifier for the order.
        fn _get_onchain_order_id(
            self: @ComponentState<TContractState>, order: @OnchainCrossChainOrder,
        ) -> u256;
    }


    /// INTERNAL ///
    #[generate_trait]
    pub impl InternalImpl<
        TContractState, impl Virtual: Virtual<TContractState>,
    > of InternalTrait<TContractState> {
        fn _initialize(ref self: ComponentState<TContractState>, permit2_address: ContractAddress) {
            self.permit2_address.write(permit2_address);
        }

        /// Marks a nonce as used by setting its bit in the appropriate bitmap.
        /// @dev Ensures that a nonce cannot be reused by flipping the corresponding bit in the
        /// bitmap. Reverts if the nonce is already used.
        ///
        /// Paramters:
        /// - `from`: The address for which the nonce is being used.
        /// - `nonce`: The nonce to mark as used.
        fn _use_nonce(
            ref self: ComponentState<TContractState>, user: ContractAddress, nonce: felt252,
        ) {
            assert(!self.used_nonces.entry((user, nonce)).read(), Errors::INVALID_NONCE);
            self.used_nonces.entry((user, nonce)).write(true);
        }

        /// Executes a batch token transfer using the Permit2 `permitWitnessTransferFrom` method.
        /// @dev Transfers tokens specified in a resolved cross-chain order to the receiver.
        ///
        /// Paramters:
        /// - `resolved_order`: The resolved order specifying tokens and amounts to transfer.
        /// - `signature`: The user's signature for the permit.
        /// - `nonce`: The unique nonce associated with the order.
        /// - `receiver`: The address that will receive the tokens.
        fn _permit_transfer_from(
            ref self: ComponentState<TContractState>,
            resolved_order: @ResolvedCrossChainOrder,
            signature: Array<felt252>,
            nonce: felt252,
            receiver: ContractAddress,
        ) {
            let mut permitted: Array<TokenPermissions> = array![];
            let mut transfer_details: Array<SignatureTransferDetails> = array![];

            for min_received in resolved_order.min_received.span() {
                permitted
                    .append(
                        TokenPermissions {
                            token: *min_received.token, amount: *min_received.amount,
                        },
                    );
                transfer_details
                    .append(
                        SignatureTransferDetails {
                            to: receiver, requested_amount: *min_received.amount,
                        },
                    );
            };

            let permit = PermitBatchTransferFrom {
                permitted: permitted.span(),
                nonce,
                deadline: (*resolved_order.open_deadline).into(),
            };

            ISignatureTransferDispatcher { contract_address: self.permit2_address.read() }
                .permit_witness_batch_transfer_from(
                    permit,
                    transfer_details.span(),
                    *resolved_order.user,
                    resolved_order.hash_struct(),
                    WITNESS_TYPE_STRING(),
                    signature,
                );
        }
    }
}

// @dev, fix bytes/strings/byte array updates
pub const RESOLVED_CROSS_CHAIN_ORDER_TYPE_HASH: felt252 = selector!(
    "\"Resolved Cross Chain Order\"(\"User\":\"ContractAddress\",\"Origin Chain ID\":\"u128\",\"Open Deadline\":\"timestamp\",\"Fill Deadline\":\"timestamp\",\"Order ID\":\"u256\",\"Max Spent\":\"Output*\",\"Min Received\":\"Output*\",\"Fill Instruction\":\"Fill Instruction*\")\"Bytes\"(\"Size\":\"u128\",\"Data\":\"u128*\")\"Fill Instruction\"(\"Destination Chain ID\":\"u128\",\"Destination Settler\":\"ContractAddress\",\"Origin Data\":\"Bytes\")\"Output\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\",\"Recipient\":\"ContractAddress\",\"Chain ID\":\"u128\")\"u256\"(\"low\":\"u128\",\"high\":\"u128\")",
);

pub const OUTPUT_TYPE_HASH: felt252 = selector!(
    "\"Output\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\",\"Recipient\":\"ContractAddress\",\"Chain ID\":\"u128\")\"u256\"(\"low\":\"u128\",\"high\":\"u128\")",
);

pub const FILL_INSTRUCTION_TYPE_HASH: felt252 = selector!(
    "\"Fill Instruction\"(\"Destination Chain ID\":\"u128\",\"Destination Settler\":\"ContractAddress\",\"Origin Data\":\"Bytes\")\"Bytes\"(\"Size\":\"u128\",\"Data\":\"u128*\")",
);

pub fn WITNESS_TYPE_STRING() -> ByteArray {
    "\"Witness\":\"Resolved Cross Chain Order\")\"Bytes\"(\"Size\":\"u128\",\"Data\":\"u128*\")\"Fill Instruction\"(\"Destination Chain ID\":\"u128\",\"Destination Settler\":\"ContractAddress\",\"Origin Data\":\"Bytes\")\"Resolved Cross Chain Order\"(\"User\":\"ContractAddress\",\"Origin Chain ID\":\"u128\",\"Open Deadline\":\"timestamp\",\"Fill Deadline\":\"timestamp\",\"Order ID\":\"u256\",\"Max Spent\":\"Output*\",\"Min Received\":\"Output*\",\"Fill Instructions\":\"Fill Instruction*\")\"Output\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\",\"Recipient\":\"ContractAddress\",\"Chain ID\":\"u128\")\"Token Permissions\"(\"Token\":\"ContractAddress\",\"Amount\":\"u256\")\"u256\"(\"low\":\"u128\",\"high\":\"u128\")"
}

pub impl U256StructHash of StructHash<u256> {
    fn hash_struct(self: @u256) -> felt252 {
        PoseidonTrait::new().update_with(_U256_TYPE_HASH).update_with(*self).finalize()
    }
}

pub impl ResolvedCrossChainOrderStructHash of StructHash<ResolvedCrossChainOrder> {
    fn hash_struct(self: @ResolvedCrossChainOrder) -> felt252 {
        let mut hashed_max_spents: Array<felt252> = array![];
        for max_spent in self.max_spent.span() {
            hashed_max_spents.append(max_spent.hash_struct());
        };

        let mut hashed_min_receiveds: Array<felt252> = array![];
        for min_received in self.min_received.span() {
            hashed_max_spents.append(min_received.hash_struct());
        };

        let mut hashed_fill_instructions: Array<felt252> = array![];
        for fill_instruction in self.fill_instructions.span() {
            hashed_fill_instructions.append(fill_instruction.hash_struct());
        };

        PoseidonTrait::new()
            .update_with(RESOLVED_CROSS_CHAIN_ORDER_TYPE_HASH)
            .update_with(*self.user)
            .update_with(*self.origin_chain_id)
            .update_with(*self.open_deadline)
            .update_with(*self.fill_deadline)
            .update_with(self.order_id.hash_struct())
            .update_with(hashed_max_spents.span())
            .update_with(hashed_min_receiveds.span())
            .update_with(hashed_fill_instructions.span())
            .finalize()
    }
}

pub impl FillInstructionStructHash of StructHash<FillInstruction> {
    fn hash_struct(self: @FillInstruction) -> felt252 {
        PoseidonTrait::new()
            .update_with(FILL_INSTRUCTION_TYPE_HASH)
            .update_with(*self.destination_chain_id)
            .update_with(*self.destination_settler)
            .update_with(self.origin_data.hash_struct())
            .finalize()
    }
}

pub impl OutputStructHash of StructHash<Output> {
    fn hash_struct(self: @Output) -> felt252 {
        PoseidonTrait::new()
            .update_with(OUTPUT_TYPE_HASH)
            .update_with(*self.token)
            .update_with(self.amount.hash_struct())
            .update_with(*self.recipient)
            .update_with(*self.chain_id)
            .finalize()
    }
}

pub impl SpanFelt252StructHash of StructHash<Span<felt252>> {
    fn hash_struct(self: @Span<felt252>) -> felt252 {
        let mut state = PoseidonTrait::new();
        for el in (*self) {
            state = state.update_with(*el);
        };
        state.finalize()
    }
}

pub impl ArrayFelt252StructHash of StructHash<Array<felt252>> {
    fn hash_struct(self: @Array<felt252>) -> felt252 {
        let mut state = PoseidonTrait::new();
        for el in self.span() {
            state = state.update_with(*el);
        };
        state.finalize()
    }
}

// pub impl ByteArrayStructHash of StructHash<ByteArray> {
//     fn hash_struct(self: @ByteArray) -> felt252 {
//         let mut state = PoseidonTrait::new();
//         let mut output = array![];
//         Serde::serialize(self, ref output);
//         for e in output.span() {
//             state = state.update_with(*e);
//         };
//         state.finalize()
//     }
// }

pub impl BytesStructHash of StructHash<Bytes> {
    fn hash_struct(self: @Bytes) -> felt252 {
        let mut state = PoseidonTrait::new();
        let clone: Bytes = Clone::<Bytes>::clone(self);

        let data = clone.data();

        state = state.update_with(self.size());
        for u128 in data.span() {
            state = state.update_with(*u128);
            u128;
        };

        state.finalize()
    }
}
