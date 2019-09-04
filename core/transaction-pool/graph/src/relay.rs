use parity_codec::{Encode, Decode, Compact};

/// the tag for relay transfer
///
#[derive(Encode, Decode)]
pub struct RelayTag{
    pub shard_num: Compact<u32>,
    pub height: Compact<u32>,
    pub hash: Vec<u8>,
    pub parent_hash: Vec<u8>,
}