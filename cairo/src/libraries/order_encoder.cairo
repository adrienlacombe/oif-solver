use alexandria_bytes::{Bytes, BytesTrait};
use alexandria_encoding::sol_abi::decode::SolAbiDecodeTrait as evm_decoder;
use alexandria_encoding::sol_abi::encode::SolAbiEncodeTrait as evm_encoder;
use core::byte_array::ByteArrayImpl;
use core::integer::u128_byte_reverse;
use core::num::traits::Zero;
use starknet::ContractAddress;


pub const EVM_ORDER_DATA_TYPE_HASH: u256 =
    0x08d75650babf4de09c9273d48ef647876057ed91d4323f8a2e3ebc2cd8a63b5e;

#[derive(Serde, Default, Drop)]
pub struct OrderData {
    pub sender: ContractAddress,
    pub recipient: ContractAddress,
    pub input_token: ContractAddress,
    pub output_token: ContractAddress,
    pub amount_in: u256,
    pub amount_out: u256,
    pub sender_nonce: felt252,
    pub origin_domain: u32,
    pub destination_domain: u32,
    pub destination_settler: ContractAddress,
    pub fill_deadline: u64,
    pub data: Bytes,
}

pub fn u256_reverse_endian(input: u256) -> u256 {
    let low = u128_byte_reverse(input.high);
    let high = u128_byte_reverse(input.low);
    u256 { low, high }
}

pub mod OrderEncoder {
    use alexandria_bytes::{Bytes, BytesTrait};
    use alexandria_encoding::sol_abi::decode::SolAbiDecodeTrait as evm_decoder;
    use alexandria_encoding::sol_abi::encode::SolAbiEncodeTrait as evm_encoder;
    use core::byte_array::ByteArrayImpl;
    use core::keccak::compute_keccak_byte_array;
    use starknet::ContractAddress;
    use super::{OrderData, u256_reverse_endian};

    //    pub const ORDER_DATA_TYPE_HASH: felt252 = selector!(
    //        "\"Order
    //        Data\"(\"Sender\":\"ContractAddress\",\"Recipient\":\"ContractAddress\",\"Input
    //        Token\":\"ContractAddress\",\"Output Token\":\"ContractAddress\",\"Amount
    //        In\":\"u256\",\"Amount Out\":\"u256\",\"Sender Nonce\":\"felt\",\"Origin
    //        Domain\":\"u128\",\"Destination Domain\":\"u128\",\"Destination
    //        Settler\":\"ContractAddress\",\"Fill
    //        Deadline\":\"timestamp\",\"Data\":\"Bytes\")\"Bytes\"(\"Size\":
    //        \"u128\",\"Data\":\"u128*\")\"u256\"(\"low\": \"u128\",\"high\":\"u128*\")",
    //    );

    // NOTE: Hard coded to match Solidity's hash so that order IDs are compatible
    pub const ORDER_DATA_TYPE_HASH: u256 =
        0x08d75650babf4de09c9273d48ef647876057ed91d4323f8a2e3ebc2cd8a63b5e;

    /// Typehash for OrderData struct
    pub fn order_data_type_hash() -> u256 {
        ORDER_DATA_TYPE_HASH
    }

    /// Compute the ID of an OrderData struct (matching EVM's keccak256 hash)
    /// Note: `compute_keccak_byte_array` returns the hash in little-endian format. Solidity's
    /// keccak returns it in big-endian.
    pub fn id(order: @OrderData) -> u256 {
        u256_reverse_endian(compute_keccak_byte_array(@encode(order).into()))
    }


    /// Encode an OrderData struct into Bytes, matching EVM's abi.encode()
    pub fn encode(order: @OrderData) -> Bytes {
        let OrderData {
            sender,
            recipient,
            input_token,
            output_token,
            amount_in,
            amount_out,
            sender_nonce,
            origin_domain,
            destination_domain,
            destination_settler,
            fill_deadline,
            data,
        } = order;

        // Encode the OrderData struct according to EVM's abi.encode() format
        let encoded: Bytes = BytesTrait::new_empty()
            .encode(32) // length of the data, required to match EVM encoding
            .encode(*sender)
            .encode(*recipient)
            .encode(*input_token)
            .encode(*output_token)
            .encode(*amount_in)
            .encode(*amount_out)
            .encode(*sender_nonce)
            .encode(*origin_domain)
            .encode(*destination_domain)
            .encode(*destination_settler)
            .encode(*fill_deadline)
            .encode(0x180) // offset to tail (12 * 32 = 384 bytes)
            .encode(data.size()); // Tail starts with data length in bytes

        ByteArrayImpl::concat(@encoded.into(), @data.clone().into()).into()
    }

    /// Decode OrderData struct from Bytes
    pub fn decode(order_data: @Bytes) -> OrderData {
        let mut offset = 0;

        let _dynamic_data_offset: u256 = order_data.decode(ref offset);

        let sender: ContractAddress = order_data.decode(ref offset);
        let recipient: ContractAddress = order_data.decode(ref offset);
        let input_token: ContractAddress = order_data.decode(ref offset);
        let output_token: ContractAddress = order_data.decode(ref offset);
        let amount_in: u256 = order_data.decode(ref offset);
        let amount_out: u256 = order_data.decode(ref offset);
        let sender_nonce: felt252 = order_data.decode(ref offset);
        let origin_domain: u32 = order_data.decode(ref offset);
        let destination_domain: u32 = order_data.decode(ref offset);
        let destination_settler: ContractAddress = order_data.decode(ref offset);
        let fill_deadline: u64 = order_data.decode(ref offset);

        // Read the dynamic bytes metadata (offset and length), then the data
        let _offset_from_head: u256 = order_data.decode(ref offset);
        let data_size: usize = order_data.decode(ref offset);
        let data: Bytes = if data_size > 0 {
            let (_, _data) = order_data.read_bytes(offset, data_size);
            _data
        } else {
            BytesTrait::new_empty()
        };

        OrderData {
            sender,
            recipient,
            input_token,
            output_token,
            amount_in,
            amount_out,
            sender_nonce,
            origin_domain,
            destination_domain,
            destination_settler,
            fill_deadline,
            data,
        }
        //        let (offset, sender) = order_data.read_address(0);
    //        let (offset, recipient) = order_data.read_address(offset);
    //        let (offset, input_token) = order_data.read_address(offset);
    //        let (offset, output_token) = order_data.read_address(offset);
    //        let (offset, amount_in) = order_data.read_u256(offset);
    //        let (offset, amount_out) = order_data.read_u256(offset);
    //        let (offset, sender_nonce) = order_data.read_felt252(offset);
    //        let (offset, origin_domain) = order_data.read_u32(offset);
    //        let (offset, destination_domain) = order_data.read_u32(offset);
    //        let (offset, destination_settler) = order_data.read_address(offset);
    //        let (offset, fill_deadline) = order_data.read_u64(offset);
    //
    //        let order_data_size = order_data.size();
    //        let data = if (order_data_size - offset > 0) {
    //            let (_, _data) = order_data.read_bytes(offset, order_data_size - offset);
    //            _data
    //        } else {
    //            BytesTrait::new_empty()
    //        };

    }
}

pub trait OpenOrderEncoder<T> {
    fn encode(self: T) -> Bytes;
    fn decode(self: Bytes) -> T;
}

pub impl OpenOrderEncoderImplAt of OpenOrderEncoder<(u256, @Bytes)> {
    /// Encodes an order_data_type and @order_data into Bytes, mimicking EVM's abi.encode()
    fn encode(self: (u256, @Bytes)) -> Bytes {
        let (order_data_type, data) = self;

        let encoded = BytesTrait::new_empty()
            .encode(order_data_type)
            .encode(64) // offset from head (2 * 32 = 64 bytes), required to match EVM encoding
            .encode(data.size()); // length of the data

        ByteArrayImpl::concat(@encoded.into(), @data.clone().into()).into()
    }

    /// Decodes an order_data_type and @order_data from Bytes, mimicking EVM's abi.decode()
    fn decode(self: Bytes) -> (u256, @Bytes) {
        let mut offset = 0;
        let self = @self;
        let order_data_type: u256 = evm_decoder::decode(self, ref offset);
        let _dynamic_data_offset: u256 = evm_decoder::decode(self, ref offset);
        let data_size: usize = evm_decoder::decode(self, ref offset);
        let data: Bytes = if data_size > 0 {
            let (_, _data) = self.read_bytes(offset, data_size);
            _data
        } else {
            BytesTrait::new_empty()
        };

        (order_data_type, @data)
        //let (offset, order_data_type) = self.read_felt252(0);
    //let (offset, data_size) = self.read_usize(offset);
    //let (_, data) = self.read_bytes(offset, data_size);
    //(order_data_type, @data)
    }
}

pub impl OpenOrderEncoderImpl of OpenOrderEncoder<(u256, Bytes)> {
    /// Encodes an order_data_type and order_data into Bytes, mimicking EVM's abi.encode()
    fn encode(self: (u256, Bytes)) -> Bytes {
        let (order_data_type, data) = self;

        let encoded = BytesTrait::new_empty()
            .encode(order_data_type)
            .encode(64) // offset from head (2 * 32 = 64 bytes), required to match EVM encoding
            .encode(data.size()); // length of the data

        ByteArrayImpl::concat(@encoded.into(), @data.clone().into()).into()
    }

    /// Decodes an order_data_type and order_data from Bytes, mimicking EVM's abi.decode()
    fn decode(self: Bytes) -> (u256, Bytes) {
        let mut offset = 0;
        let self = @self;
        let order_data_type: u256 = evm_decoder::decode(self, ref offset);
        let _dynamic_data_offset: u256 = evm_decoder::decode(self, ref offset);
        let data_size: usize = evm_decoder::decode(self, ref offset);
        let data: Bytes = if data_size > 0 {
            let (_, _data) = self.read_bytes(offset, data_size);
            _data
        } else {
            BytesTrait::new_empty()
        };

        (order_data_type, data)
    }
}

/// Sets the default value of `ContractAddress` to zero.
pub impl ContractAddressDefault of Default<ContractAddress> {
    fn default() -> ContractAddress {
        Zero::zero()
    }
}

/// Sets the default value of `Bytes` to zero.
pub impl BytesDefault of Default<Bytes> {
    fn default() -> Bytes {
        BytesTrait::new_empty()
    }
}

