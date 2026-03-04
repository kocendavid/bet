pub mod admin;
pub mod book;
pub mod http;
pub mod kafka;
pub mod ledger;
pub mod persistence;
pub mod risk;
pub mod service;
pub mod sharding;
pub mod streaming;
pub mod types;

pub mod pb {
    tonic::include_proto!("ledger.v1");
}

pub mod stream_pb {
    tonic::include_proto!("stream.v1");
}
