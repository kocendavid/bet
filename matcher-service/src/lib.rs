pub mod book;
pub mod http;
pub mod ledger;
pub mod persistence;
pub mod service;
pub mod types;

pub mod pb {
    tonic::include_proto!("ledger.v1");
}
