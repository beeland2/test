use std::{
    collections::HashMap,
    env,
    io::Error as IoError,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use std::collections::HashSet;
use std::hash::Hash;

use futures_channel::mpsc::{unbounded, UnboundedSender};
use futures_util::{future, pin_mut, stream::TryStreamExt, StreamExt};

use tokio::net::{TcpListener, TcpStream};
use tungstenite::protocol::Message;
use uuid::Uuid;
use failure::{Error, format_err};
use serde::{Serialize, Deserialize};
use log::{debug, error, log_enabled, info, Level};


type Result<T> = std::result::Result<T, Error>;

type Tx = UnboundedSender<Message>;


#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignallerMessage {
    Offer {
        // sdp: RTCSessionDescription,
        uuid: String,
        to: String,
    },
    Answer {
        // sdp: RTCSessionDescription,
        uuid: String,
        to: String,
    },
    Ice {
        // ice: RTCIceCandidateInit,
        uuid: String,
        to: String,
    },
    Join {
        uuid: String,
        room: String,
    },
    Start {
        uuid: String,
    },
    Leave {
        uuid: String,
    },
}


struct State {
    sessions: HashMap<Uuid, Session>,
    peers: HashMap<Uuid, Peer>,
}

struct Session {
    sharer: Uuid,
    viewers: HashSet<Uuid>,
}

impl Session {
    fn new(sharer: Uuid) -> Self {
        Session {
            sharer,
            viewers: Default::default()
        }
    }
}

struct Peer {
    session: Uuid,
    sender: Tx,
    peer_type: PeerType,
}

enum PeerType {
    Sharer {},
    Viewer {}
}

type StateType = Arc<Mutex<State>>;

impl State {
    fn new() -> StateType {
        Arc::new(Mutex::new(
            State {
                sessions: Default::default(),
                peers: Default::default()
            }
        ))
    }

    fn add_sharer(&mut self, id: Uuid, sender: Tx) -> Result<()> {
        if self.sessions.contains_key(&id) {
            return Err(format_err!("Session already exists"));
        }
        self.sessions.insert(id, Session::new(id));
        self.peers.insert(id, Peer {
            session: id,
            sender,
            peer_type: PeerType::Sharer {}
        });
        Ok(())
    }

    fn add_viewer(&mut self, id: Uuid, session: Uuid, sender: Tx) -> Result<()> {
        if !self.sessions.contains_key(&session) {
            return Err(format_err!("Session does not exist"));
        }
        self.sessions.get_mut(&session).unwrap().viewers.insert(id);
        self.peers.insert(id, Peer {
            session,
            sender,
            peer_type: PeerType::Viewer {}
        });
        Ok(())
    }

    fn end_session(&mut self, id: Uuid) -> Result<()> {
        /////////////////
        let session_id = self.peers.get(&id).unwrap().session;
        if !self.sessions.contains_key(&session_id) {
            return Err(format_err!("Session does not exist"));
        }
        let session = self.sessions.remove(&session_id).unwrap();
        for viewer in session.viewers {
            self.peers.remove(&viewer);
        }
        self.peers.remove(&session.sharer);
        Ok(())
    }
}

fn handle_message(state: &mut State, tx: &Tx, raw_payload: &str) -> Result<()> {
    let msg: SignallerMessage = serde_json::from_str(raw_payload)?;
    let forward_message = |state: &State, to: String| -> Result<()> {
        let peer = state.peers.get(&Uuid::parse_str(&to)?).ok_or(format_err!("Peer does not exist"))?;
        peer.sender.unbounded_send(raw_payload.into())?;
        Ok(())
    };

    match msg {
        SignallerMessage::Join { uuid, room } => {
            state.add_viewer(
                Uuid::parse_str(&uuid)?,
                Uuid::parse_str(&room)?,
                tx.clone()
            )?;
            forward_message(&state, room)?;
        },
        SignallerMessage::Start { uuid } => {
            state.add_sharer(
                Uuid::parse_str(&uuid)?,
                tx.clone()
            )?;
        },
        SignallerMessage::Leave { uuid } => {
            state.end_session(
                Uuid::parse_str(&uuid)?
            )?;
        },
        SignallerMessage::Offer { uuid, to } |
        SignallerMessage::Answer { uuid, to } |
        SignallerMessage::Ice { uuid, to } => {
            forward_message(&state, to)?;
        },
    };
    Ok(())
}

async fn handle_connection(state: StateType, raw_stream: TcpStream, addr: SocketAddr) {
    info!("Incoming TCP connection from: {}", addr);

    let ws_stream = tokio_tungstenite::accept_async(raw_stream)
        .await
        .expect("Error during the websocket handshake occurred");
    info!("WebSocket connection established: {}", addr);

    // Insert the write part of this peer to the peer map.
    let (tx, rx) = unbounded();

    let (outgoing, incoming) = ws_stream.split();

    let broadcast_incoming = incoming.try_for_each(|msg| {
        if !msg.is_text() {
            return future::ok(());
        }

        info!("Received a message from {}: {}", addr, msg.to_text().unwrap());

        if let Ok(s) = msg.to_text() {
            let mut locked_state = state.lock().unwrap();
            if let Err(e) = handle_message(&mut locked_state, &tx,s) {
                info!("Error handling message: {}", e);
            }
        }
        future::ok(())
    });

    let receive_from_others = rx.map(Ok).forward(outgoing);

    pin_mut!(broadcast_incoming, receive_from_others);
    future::select(broadcast_incoming, receive_from_others).await;

    info!("{} disconnected", &addr);
    //locked_state.remove(&addr); todo: handle termination logic
}

#[tokio::main]
async fn main() -> Result<()> {

    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "debug"),
    );
    let addr = env::args().nth(1).unwrap_or_else(|| "0.0.0.0:8080".to_string());

    let state = State::new();

    // Create the event loop and TCP listener we'll accept connections on.
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
    info!("Listening on: {}", addr);

    // Let's spawn the handling of each connection in a separate task.
    while let Ok((stream, addr)) = listener.accept().await {
        tokio::spawn(handle_connection(state.clone(), stream, addr));
    }

    Ok(())
}