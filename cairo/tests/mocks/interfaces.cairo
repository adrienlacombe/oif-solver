#[starknet::interface]
pub trait IMintable<TState> {
    fn mint(ref self: TState, to: starknet::ContractAddress, amount: u256);
}

#[starknet::interface]
pub trait IDS<TState> {
    fn DOMAIN_SEPARATOR(self: @TState) -> felt252;
}
