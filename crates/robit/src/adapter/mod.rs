use anyhow::Result;

use crate::types::{InboundMessage, OutboundMessage};

pub mod robrix;
pub mod stdin;

pub trait Adapter {
    fn name(&self) -> &'static str;
    fn recv(&mut self) -> Result<Option<InboundMessage>>;
    fn send(&mut self, msg: OutboundMessage) -> Result<()>;
}
