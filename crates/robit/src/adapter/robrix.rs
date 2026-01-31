use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::adapter::Adapter;
use crate::types::{InboundMessage, OutboundMessage};

pub struct RobrixAdapter {
    inbound: Receiver<InboundMessage>,
    outbound: Sender<OutboundMessage>,
}

pub struct RobrixHandle {
    inbound: Sender<InboundMessage>,
    outbound: Receiver<OutboundMessage>,
}

impl RobrixAdapter {
    pub fn new() -> (Self, RobrixHandle) {
        let (in_tx, in_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        (
            Self {
                inbound: in_rx,
                outbound: out_tx,
            },
            RobrixHandle {
                inbound: in_tx,
                outbound: out_rx,
            },
        )
    }
}

impl Adapter for RobrixAdapter {
    fn name(&self) -> &'static str {
        "robrix"
    }

    fn recv(&mut self) -> Result<Option<InboundMessage>> {
        match self.inbound.recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(_) => Ok(None),
        }
    }

    fn send(&mut self, msg: OutboundMessage) -> Result<()> {
        self.outbound
            .send(msg)
            .map_err(|_| anyhow!("robrix outbound channel closed"))
    }
}

impl RobrixHandle {
    pub fn send(&self, msg: InboundMessage) -> Result<()> {
        self.inbound
            .send(msg)
            .map_err(|_| anyhow!("robrix inbound channel closed"))
    }

    pub fn send_json(&self, msg: Value) -> Result<()> {
        let inbound: InboundMessage = serde_json::from_value(msg)
            .map_err(|err| anyhow!("invalid inbound json: {err}"))?;
        self.send(inbound)
    }

    pub fn recv(&self) -> Option<OutboundMessage> {
        self.outbound.recv().ok()
    }

    pub fn try_recv(&self) -> Option<OutboundMessage> {
        match self.outbound.try_recv() {
            Ok(msg) => Some(msg),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => None,
        }
    }

    pub fn recv_json(&self) -> Option<Value> {
        self.recv().and_then(|msg| serde_json::to_value(msg).ok())
    }

    pub fn try_recv_json(&self) -> Option<Value> {
        self.try_recv()
            .and_then(|msg| serde_json::to_value(msg).ok())
    }
}
