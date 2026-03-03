pub mod logic;
pub mod service;

pub mod pb {
    tonic::include_proto!("ledger.v1");
}
