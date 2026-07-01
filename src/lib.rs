pub mod fec;

pub use fec::{
    ConstellationEncoder, DecodeError, EncodeError, DATA_PSHREDS, PARITY_PSHREDS, TOTAL_PSHREDS,
};
