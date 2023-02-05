use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

#[derive(Deserialize, Serialize, Debug)]
#[serde(tag = "type")]
pub enum Message {
    Subscribe {
        name: String,
        #[serde(skip_serializing, skip_deserializing)]
        respond_to: Option<oneshot::Sender<mpsc::Receiver<Message>>>,
    },
    Unsubscribe {
        name: String,
    },
    Broadcast {
        from: Option<String>,
        body: String,
        #[serde(skip_serializing, skip_deserializing)]
        respond_to: Option<oneshot::Sender<bool>>,
    },
}

struct Actor {
    recent: Vec<Message>,
    rx: mpsc::Receiver<Message>,
    subscribers: HashMap<String, mpsc::Sender<Message>>,
}

impl Actor {
    async fn run(&mut self) {
        while let Some(message) = self.rx.recv().await {
            dbg!(&message);
            match message {
                Message::Subscribe { name, respond_to } => {
                    let (sender, receiver) = mpsc::channel::<Message>(8);
                    self.subscribers.insert(name.clone(), sender);
                    if let Some(ch) = respond_to {
                        ch.send(receiver).expect("subscribe error");
                    }
                    let mut notifications = Vec::new();
                    for (k, v) in self.subscribers.iter_mut() {
                        let name = name.clone();
                        if k == &name {
                            continue;
                        }
                        let notif = Message::Subscribe {
                            name,
                            respond_to: None,
                        };
                        let task = v.send(notif);
                        notifications.push(task);
                    }
                    join_all(notifications).await;
                }
                Message::Unsubscribe { name } => {
                    self.subscribers.remove(&name);
                    let mut notifications = Vec::new();
                    for (k, v) in self.subscribers.iter_mut() {
                        let name = name.clone();
                        if k == &name {
                            continue;
                        }
                        let notif = Message::Unsubscribe { name };
                        let task = v.send(notif);
                        notifications.push(task);
                    }
                    join_all(notifications).await;
                }
                Message::Broadcast {
                    from,
                    body,
                    respond_to,
                } => {
                    {
                        let from = from.clone();
                        let body = body.clone();
                        let cache = Message::Broadcast {
                            from,
                            respond_to: None,
                            body,
                        };
                        self.recent.push(cache);
                        if self.recent.len() > 12 {
                            self.recent.pop();
                        }
                    }
                    if let Some(ch) = respond_to {
                        ch.send(true).expect("broadcast error");
                    }
                    let mut notifications = Vec::new();
                    for (k, v) in self.subscribers.iter_mut() {
                        let from = from.clone();
                        let body = body.clone();
                        if let Some(name) = &from {
                            if k == name {
                                continue;
                            }
                        }
                        let notif = Message::Broadcast {
                            from,
                            body,
                            respond_to: None,
                        };
                        let task = v.send(notif);
                        notifications.push(task);
                    }
                    join_all(notifications).await;
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct ActorHandle {
    pub tx: mpsc::Sender<Message>,
}

impl ActorHandle {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Message>(8);
        let mut agent = Actor {
            rx,
            subscribers: HashMap::new(),
            recent: Vec::new(),
        };
        tokio::spawn(async move { agent.run().await });
        Self { tx }
    }
}
