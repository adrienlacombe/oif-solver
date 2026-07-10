pub mod Hyperlane7683Message {
    use alexandria_bytes::{Bytes, BytesTrait};
    use alexandria_encoding::sol_abi::encode::SolAbiEncodeTrait as evm_encoder;
    use alexandria_encoding::sol_abi::decode::SolAbiDecodeTrait as evm_decoder;
    use core::byte_array::ByteArrayImpl;

    /// Returns formatted Router7683 message
    /// @dev This function should only be used in memory message construction.
    /// @dev This function mimics the EVM's abi.encode() function for the Router7683 message.
    ///
    /// Parameters:
    /// - `settle`: Flag to indicate if the message is a settlement or refund
    /// - `order_ids`: The orderIds to settle or refund
    /// - `orders_filler_data`: Each element should contain the bytes32 encoded address of the
    /// settlement receiver.
    ///
    /// Returns: Formatted message body
    pub fn encode(settle: bool, order_ids: Span<u256>, orders_filler_data: Span<Bytes>) -> Bytes {
        // Head size is constant, 3 elements of 32 bytes each (is offset for order_ids)
        let head_size = 96;
        // Number of order ids
        let order_ids_len = order_ids.len();
        // Size of order id array; 32 bytes per order id, plus one for the length
        let order_ids_size = (1 + order_ids_len) * 32;
        // Number of orders_filler_data
        let orders_filler_data_len = orders_filler_data.len();
        // Offset to orders_filler_data is after the head and order_ids array
        let orders_filler_data_offset = head_size + order_ids_size;

        // Encode settle, order_ids & orders_filler_data offsets
        let mut encoded: Bytes = BytesTrait::new_empty()
            .encode(settle)
            .encode(head_size)
            .encode(orders_filler_data_offset);

        // Encode order_ids length and elements
        encoded = encoded.encode(order_ids_len);
        for i in 0..order_ids_len {
            encoded = encoded.encode(*order_ids[i]);
        };

        // Encode orders_filler_data length
        encoded = encoded.encode(orders_filler_data_len);

        // Encode orders_filler_data relative offsets
        let base = orders_filler_data_len * 0x20;
        for i in 0..orders_filler_data_len {
            // Each element's offset is 64 bytes after the previous
            encoded = encoded.encode(base + (i * 0x40));
        };

        // Encode each order_filler_data size and data
        for i in 0..orders_filler_data_len {
            let _order_filler_data_size = orders_filler_data[i].size();
            //assert!(
            //    _order_filler_data_size == 32,
            //    "order filler data size must be 32 bytes, got {_order_filler_data_size}",
            //);
            encoded = encoded.encode(32);

            // Translate order_filler_data into ByteArray to concatenate
            let encoded_ba: ByteArray = ByteArrayImpl::concat(
                @encoded.into(), @orders_filler_data[i].clone().into(),
            );
            encoded = encoded_ba.into();
        };

        encoded
    }

    /// Parses and returns the calls from the provided message
    /// @dev Mimics the EVM's abi.decode() function for the Router7683 message.
    ///
    /// Parameters:
    /// - `message`: The interchain message
    ///
    /// Returns The array of calls
    pub fn decode(message: Bytes) -> (bool, Span<u256>, Span<Bytes>) {
        let mut order_ids: Array<u256> = array![];
        let mut orders_filler_data: Array<Bytes> = array![];
        let mut offset = 0;

        // Decode settle
        let settle = message.decode(ref offset);

        // Ignore offset to order_ids and orders_filler_data
        let _: u256 = message.decode(ref offset);
        let _: u256 = message.decode(ref offset);

        // Decode order ids
        let _order_ids_len: usize = message.decode(ref offset);
        for _ in 0.._order_ids_len {
            order_ids.append(message.decode(ref offset));
        };

        // Ignore orders_filler_data relative offsets
        let _orders_filler_data_len = message.decode(ref offset);
        for _ in 0.._orders_filler_data_len {
            let _: u256 = message.decode(ref offset);
        };

        // Decode orders filler data
        for _ in 0_usize.._orders_filler_data_len {
            // Each order filler data should be size 32
            //assert_eq!(message.decode(ref offset), 32);

            // Ignore filler data size
            let _: u256 = message.decode(ref offset);

            orders_filler_data.append(message.decode(ref offset));
        };

        (settle, order_ids.span(), orders_filler_data.span())
    }

    pub fn encode_settle(order_ids: Span<u256>, orders_filler_data: Span<Bytes>) -> Bytes {
        encode(true, order_ids, orders_filler_data)
    }

    pub fn encode_refund(order_ids: Span<u256>) -> Bytes {
        encode(false, order_ids, array![].span())
    }
}
