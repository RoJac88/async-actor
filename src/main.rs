//! Adapted from axum's example websocket server.
//!
//! Run the server with
//! ```not_rust
//! cargo run -p example-websockets --bin example-websockets
//! ```
//!
//! Run a browser client with
//! ```not_rust
//! firefox http://localhost:3000
//! ```
//!
//! Alternatively you can run the rust client (showing two
//! concurrent websocket connections being established) with
//! ```not_rust
//! cargo run -p example-websockets --bin example-client
//! ```

mod actor;
use actor::{ActorHandle, Message as ActorMessage};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        FromRef, State, TypedHeader,
    },
    response::IntoResponse,
    routing::get,
    Router,
};

use std::borrow::Cow;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

//allows to extract the IP of connecting user
use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::CloseFrame;

//allows to split the websocket stream into separate TX and RX branches
use futures::{sink::SinkExt, stream::StreamExt};

use tokio::sync::{mpsc, oneshot};

#[derive(FromRef, Clone)]
struct AppState {
    handle: ActorHandle,
}

#[tokio::main]
async fn main() {
    // All endpoints can comunicate with our actor via the ActorHandle
    let shared_state = AppState {
        handle: ActorHandle::new(),
    };

    let cors = CorsLayer::new()
        // allow requests from any origin
        .allow_origin(Any);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "example_websockets=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // build our application with some routes
    let app = Router::new()
        .layer(cors)
        .route("/ws", get(ws_handler))
        .with_state(shared_state)
        // logging so we can see whats going on
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // run it with hyper
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    println!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

/// The handler for the HTTP request (this gets called when the HTTP GET lands at the start
/// of websocket negotiation). After this completes, the actual switching from HTTP to
/// websocket protocol will occur.
/// This is the last point where we can extract TCP/IP metadata such as IP address of the client
/// as well as things from HTTP headers such as user-actor of the browser etc.
async fn ws_handler(
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(handle): State<ActorHandle>,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    println!("`{}` at {} connected.", user_agent, addr.to_string());
    let tx: mpsc::Sender<ActorMessage> = handle.tx.clone();
    // finalize the upgrade process by returning upgrade callback.
    // we can customize the callback by sending additional info such as address.
    ws.on_upgrade(move |socket| handle_socket(socket, addr, tx))
}

/// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(socket: WebSocket, who: SocketAddr, tx: mpsc::Sender<ActorMessage>) {
    // We will use the tx end of the channel to send messages to the actor.
    // To receive notifications, we will send a subscribe message and get
    // back a read only channel
    let (ch, response) = oneshot::channel::<mpsc::Receiver<ActorMessage>>();
    {
        let tx = tx.clone();
        let subscribe = ActorMessage::Subscribe {
            name: who.to_string(),
            respond_to: Some(ch),
        };
        tx.send(subscribe)
            .await
            .expect("ws handler subscribe error");
    }
    // The oneshot channel in the subscription message gets back
    // another channel that recieves messages from the actor
    let mut subscription = response.await.unwrap();

    // By splitting socket we can send and receive at the same time.
    let (mut sender, mut receiver) = socket.split();

    let mut send_task = tokio::spawn(async move {
        // Forward messages from the actor to the client
        while let Some(msg) = subscription.recv().await {
            match msg {
                ActorMessage::Subscribe {
                    name,
                    respond_to: _,
                } => {
                    let forward_msg = format!("{} joined", name);
                    if sender.send(Message::Text(forward_msg)).await.is_err() {
                        break;
                    }
                }
                ActorMessage::Unsubscribe { name } => {
                    let forward_msg = format!("{} left", name);
                    if sender.send(Message::Text(forward_msg)).await.is_err() {
                        break;
                    }
                }
                ActorMessage::Broadcast {
                    from,
                    body,
                    respond_to: _,
                } => {
                    let forward_msg = match from {
                        Some(name) => format!("{} says: {}", name, body),
                        None => format!("SERVER: {}", body),
                    };
                    if sender.send(Message::Text(forward_msg)).await.is_err() {
                        break;
                    }
                }
            }
        }
        println!("Sending close to {}...", who);
        if let Err(e) = sender
            .send(Message::Close(Some(CloseFrame {
                code: axum::extract::ws::close_code::NORMAL,
                reason: Cow::from("Goodbye"),
            })))
            .await
        {
            println!("Could not send Close due to {}, probably it is ok?", e);
        }
    });

    let _tx = tx.clone();
    let mut recv_task = tokio::spawn(async move {
        // Receive messages from client and forward to actor when needed
        let mut cnt = 0;
        while let Some(Ok(msg)) = receiver.next().await {
            cnt += 1;
            // print message and break if instructed to do so
            if process_message(msg, who, _tx.clone()).await.is_break() {
                break;
            }
        }
        cnt
    });

    // If any one of the tasks exit, abort the other.
    tokio::select! {
        rv_a = (&mut send_task) => {
            match rv_a {
                Ok(()) => println!("connection to {} closed", who),
                Err(e) => println!("Error sending messages to {}: {}", who, e)
            }
            recv_task.abort();
        },
        rv_b = (&mut recv_task) => {
            match rv_b {
                Ok(b) => println!("Received {} messages", b),
                Err(b) => println!("Error receiving messages {:?}", b)
            }
            send_task.abort();
        }
    }

    // returning from the handler closes the websocket connection
    {
        // Unsubscribe on exit
        let unsub = ActorMessage::Unsubscribe {
            name: who.to_string(),
        };
        match tx.send(unsub).await {
            Ok(()) => {}
            Err(_) => {}
        }
    }
    println!("Websocket context {} destroyed", who);
}

async fn process_message(
    msg: Message,
    who: SocketAddr,
    actor_ch: mpsc::Sender<ActorMessage>,
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!(">>> {} sent str: {:?}", who, t);
            match serde_json::from_str::<ActorMessage>(&t) {
                Ok(parsed_msg) => {
                    match parsed_msg {
                        ActorMessage::Broadcast {
                            from: _,
                            body,
                            respond_to: _,
                        } => {
                            let (tx, rx) = oneshot::channel();
                            let server_msg = ActorMessage::Broadcast {
                                from: Some(who.to_string()),
                                body,
                                respond_to: Some(tx),
                            };
                            if actor_ch.send(server_msg).await.is_err() {
                                return ControlFlow::Break(());
                            }
                            if rx.await.is_err() {
                                return ControlFlow::Break(());
                            }
                        }
                        _ => return ControlFlow::Break(()),
                    }
                    // println!("{:?}", parsed_msg);
                }
                Err(e) => {
                    eprintln!("{}", e);
                    return ControlFlow::Break(());
                }
            }
        }
        Message::Binary(d) => {
            println!(">>> {} sent {} bytes: {:?}", who, d.len(), d);
        }
        Message::Close(c) => {
            if let Some(cf) = c {
                println!(
                    ">>> {} sent close with code {} and reason `{}`",
                    who, cf.code, cf.reason
                );
            } else {
                println!(">>> {} somehow sent close message without CloseFrame", who);
            }
            return ControlFlow::Break(());
        }

        Message::Pong(v) => {
            println!(">>> {} sent pong with {:?}", who, v);
        }
        // You should never need to manually handle Message::Ping, as axum's websocket library
        // will do so for you automagically by replying with Pong and copying the v according to
        // spec. But if you need the contents of the pings you can see them here.
        Message::Ping(v) => {
            println!(">>> {} sent ping with {:?}", who, v);
        }
    }
    ControlFlow::Continue(())
}
