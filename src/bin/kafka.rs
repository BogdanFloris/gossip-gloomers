use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::Context;
use async_trait::async_trait;
use gossip_glomers::{event_loop, Body, Event, Init, Message, Node, KV};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const MSG_SIZE: i64 = 5;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum Payload {
    Send {
        key: String,
        msg: i64,
    },
    SendOk {
        offset: i64,
    },
    Poll {
        offsets: HashMap<String, i64>,
    },
    PollOk {
        msgs: HashMap<String, Vec<Vec<i64>>>,
    },
    CommitOffsets {
        offsets: HashMap<String, i64>,
    },
    CommitOffsetsOk,
    ListCommittedOffsets {
        keys: Vec<String>,
    },
    ListCommittedOffsetsOk {
        offsets: HashMap<String, i64>,
    },
    Read {
        key: String,
    },
    ReadOk {
        value: i64,
    },
    Error {
        code: usize,
        text: String,
    },
    Write {
        key: String,
        value: i64,
    },
    WriteOk {},
    Cas {
        key: String,
        from: i64,
        to: i64,
        #[serde(default, rename = "create_if_not_exists")]
        put: bool,
    },
    CasOk {},
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum InjectedPayload {}

struct KafkaNode {
    id: AtomicUsize,
    node: String,
    stdout: Mutex<tokio::io::Stdout>,
    storage_lin: String,
    storage_seq: String,
    rpc: Mutex<HashMap<usize, tokio::sync::oneshot::Sender<Message<Payload>>>>,
}

impl KafkaNode {
    async fn rpc(&self, to: &String, payload: Payload) -> anyhow::Result<Message<Payload>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = Message {
            src: self.node.clone(),
            dest: to.clone(),
            body: Body {
                id: self.id.fetch_add(1, Ordering::SeqCst).into(),
                in_reply_to: None,
                payload,
            },
        };
        self.rpc.lock().await.insert(msg.body.id.unwrap(), tx);
        msg.send(&self.stdout).await.context("send rpc message")?;
        rx.await.context("receive rpc response")
    }
}

#[async_trait]
impl KV<i64> for KafkaNode {
    async fn read(&self, storage: &String, key: String) -> anyhow::Result<i64> {
        let payload = Payload::Read { key };
        let result = self
            .rpc(storage, payload)
            .await
            .context("read from storage")?;
        match result.body.payload {
            Payload::ReadOk { value } => Ok(value),
            _ => anyhow::bail!("unexpected payload"),
        }
    }

    async fn write(&self, storage: &String, key: String, value: i64) -> anyhow::Result<()> {
        let payload = Payload::Write { key, value };
        let _result = self.rpc(storage, payload).await.context("write to storage");
        Ok(())
    }

    async fn cas(
        &self,
        storage: &String,
        key: String,
        from: i64,
        to: i64,
        put: bool,
    ) -> anyhow::Result<()> {
        let payload = Payload::Cas { key, from, to, put };
        let result = self.rpc(storage, payload).await.context("cas to storage")?;
        match result.body.payload {
            Payload::CasOk {} => Ok(()),
            _ => anyhow::bail!("unexpected payload"),
        }
    }
}

#[async_trait]
impl Node<Payload, InjectedPayload> for KafkaNode {
    fn from_init(
        init: Init,
        _tx: tokio::sync::mpsc::Sender<Event<Payload, InjectedPayload>>,
        stdout: Mutex<tokio::io::Stdout>,
    ) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        let id = AtomicUsize::new(1);
        let storage_lin = "lin-kv".to_string();
        let storage_seq = "seq-kv".to_string();

        Ok(Self {
            id,
            node: init.node_id,
            stdout,
            storage_lin,
            storage_seq,
            rpc: Mutex::new(HashMap::new()),
        })
    }

    /// # Handle incoming messages
    ///
    /// We will store the messages and offsets in the following format in the KV store:
    /// - {key}:{offset} -> {msg}
    /// - latest:{key} -> {offset}
    /// - committed:{key} -> {offset}
    async fn handle(
        &self,
        event: gossip_glomers::Event<Payload, InjectedPayload>,
    ) -> anyhow::Result<()> {
        match event {
            gossip_glomers::Event::EOF => {}
            gossip_glomers::Event::Message(message) => {
                // Handle RPC responses
                if message.body.in_reply_to.is_some() {
                    let id = message.body.in_reply_to.unwrap();
                    let tx = self.rpc.lock().await.remove(&id).unwrap();
                    if let Err(_) = tx.send(message) {
                        anyhow::bail!("rpc response channel closed");
                    }
                    return Ok(());
                }

                let mut reply = message.into_reply(Some(&self.id));
                match reply.body.payload {
                    Payload::Send { key, msg } => {
                        // Find the offset
                        let latest_key = format!("latest:{}", key);
                        let mut start = match self
                            .read(&self.storage_lin, latest_key.clone())
                            .await
                            .context("read latest offset")
                        {
                            Ok(offset) => offset,
                            Err(_) => 0,
                        };

                        loop {
                            let curr = start.clone();
                            let (prev, now) = (curr.clone() - 1, curr);
                            let res = self
                                .cas(&self.storage_lin, latest_key.clone(), prev, now, true)
                                .await
                                .context("cas to find offset");
                            match res {
                                Ok(_) => break,
                                Err(_) => start += 1,
                            }
                        }

                        let msg_key = format!("{}:{}", key, start);
                        let _ = self
                            .write(&self.storage_seq, msg_key.clone(), msg)
                            .await
                            .context("write message");

                        let _ = self
                            .write(&self.storage_seq, latest_key, start)
                            .await
                            .context("write latest offset");

                        reply.body.payload = Payload::SendOk { offset: start };
                        reply
                            .send(&self.stdout)
                            .await
                            .context("send send ok response")?;
                    }
                    Payload::Poll { offsets } => {
                        let mut msgs = HashMap::new();
                        for (key, offset) in offsets {
                            let mut msg = Vec::new();
                            for id in offset..(offset + MSG_SIZE) {
                                let msg_key = format!("{}:{}", key, id);
                                let res = self
                                    .read(&self.storage_seq, msg_key)
                                    .await
                                    .context("read message");
                                match res {
                                    Ok(value) => msg.push(vec![id, value]),
                                    Err(_) => continue,
                                };
                            }
                            msgs.insert(key, msg);
                        }
                        reply.body.payload = Payload::PollOk { msgs };
                        reply
                            .send(&self.stdout)
                            .await
                            .context("send poll ok response")?;
                    }
                    Payload::CommitOffsets { offsets } => {
                        for (key, offset) in offsets {
                            let committed_key = format!("committed:{}", key);
                            let _ = self
                                .write(&self.storage_seq, committed_key, offset)
                                .await
                                .context("write committed offset");
                        }
                        reply.body.payload = Payload::CommitOffsetsOk;
                        reply
                            .send(&self.stdout)
                            .await
                            .context("send commit offsets ok response")?;
                    }
                    Payload::ListCommittedOffsets { keys } => {
                        let mut offsets = HashMap::new();
                        for key in keys {
                            let committed_key = format!("committed:{}", key);
                            let res = self
                                .read(&self.storage_seq, committed_key)
                                .await
                                .context("read committed offset");
                            let offset = match res {
                                Ok(offset) => offset,
                                Err(_) => 0,
                            };
                            offsets.insert(key, offset);
                        }
                        reply.body.payload = Payload::ListCommittedOffsetsOk { offsets };
                        reply
                            .send(&self.stdout)
                            .await
                            .context("send list commit offsets ok response")?;
                    }
                    Payload::Error { code, text } => {
                        eprintln!("Error {}: {}", code, text);
                    }
                    Payload::ListCommittedOffsetsOk { .. }
                    | Payload::CommitOffsetsOk
                    | Payload::PollOk { .. }
                    | Payload::SendOk { .. }
                    | Payload::Read { .. }
                    | Payload::ReadOk { .. }
                    | Payload::Write { .. }
                    | Payload::WriteOk {}
                    | Payload::Cas { .. }
                    | Payload::CasOk {} => {}
                }
            }
            gossip_glomers::Event::Injected(_) => {}
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    event_loop::<KafkaNode, _, _>().await
}
