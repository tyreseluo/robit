use std::io::{self, Write};

use anyhow::Result;
use serde_json::Value;

use crate::adapter::Adapter;
use crate::types::{InboundMessage, OutboundMessage};

pub struct StdinAdapter {
    prompt: String,
    counter: u64,
}

impl StdinAdapter {
    pub fn new() -> Self {
        Self {
            prompt: "robit> ".to_string(),
            counter: 1,
        }
    }

    fn next_id(&mut self) -> String {
        let id = self.counter;
        self.counter += 1;
        format!("in-{id}")
    }
}

impl Adapter for StdinAdapter {
    fn name(&self) -> &'static str {
        "stdin"
    }

    fn recv(&mut self) -> Result<Option<InboundMessage>> {
        print!("{}", self.prompt);
        io::stdout().flush()?;
        let mut line = String::new();
        let stdin = io::stdin();
        if stdin.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let text = line.trim().to_string();
        if matches!(text.as_str(), "exit" | "quit") {
            return Ok(None);
        }

        Ok(Some(InboundMessage {
            id: self.next_id(),
            text,
            sender: "stdin".to_string(),
            channel: "stdin".to_string(),
            workspace_id: Some("local".to_string()),
            metadata: Value::Null,
        }))
    }

    fn send(&mut self, msg: OutboundMessage) -> Result<()> {
        println!("{}", msg.text);
        if let Some(data) = msg.metadata.get("data") {
            if !data.is_null() {
                println!("data: {}", data);
            }
        }
        Ok(())
    }
}
