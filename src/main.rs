use async_tungstenite::tungstenite::Message;
use futures::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use regex::Regex;
use serde_json::json;
use std::time::SystemTime;
use std::{collections::HashMap, error};

// // Just a generic Result type to ease error handling for us. Errors in multithreaded
// // async contexts needs some extra restrictions
// type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct UnackMsg {
    msg_id: i64,
    payload: HashMap<String, serde_json::Value>,
    callback: fn(&str),
}

struct Bot {
    auth: String,
    user_id: String,
    room_id: String,
    client_id: String,
    msg_id: i64,
    unack_msgs: Vec<UnackMsg>,
    callbacks: HashMap<String, Vec<fn(&str)>>,
    log_ws: bool,
}

// struct SpeakEvt {
//     command: String,
//     user_id: String,
//     name: String,
//     text: String,
// }

macro_rules! h {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::HashMap::new();
         $( map.insert($key.to_string(), serde_json::Value::String($val.to_string())); )*
         map
    }}
}

fn get_message_len(msg: &str) -> Option<usize> {
    if !LEN_RGX.is_match(msg) {
        return None;
    }
    let captures = LEN_RGX.captures(msg)?;
    let msg_len_str = captures.get(1)?;
    msg_len_str.as_str().parse().ok()
}

// Extract the json part of a websocket message
fn extract_message_json(msg: &str) -> Option<String> {
    if let Some(msg_len) = get_message_len(msg) {
        if let Some(start_idx) = msg.find("{") {
            let raw_json: String = msg.chars().skip(start_idx).take(msg_len).collect();
            return Some(raw_json);
        }
    }
    None
}

lazy_static! {
    static ref HEARTBEAT_RGX: Regex = Regex::new(r"^~m~[0-9]+~m~(~h~[0-9]+)$").unwrap();
    static ref LEN_RGX: Regex = Regex::new(r"^~m~([0-9]+)~m~").unwrap();
}

fn is_heartbeat(msg: &str) -> bool {
    HEARTBEAT_RGX.is_match(msg)
}

fn get_heartbeat_id(msg: &str) -> Option<&str> {
    if let Some(captures) = HEARTBEAT_RGX.captures(msg) {
        if let Some(heartbeat_id) = captures.get(1) {
            return Some(heartbeat_id.as_str());
        }
    }
    None
}

async fn start_ws(
    tx: tokio::sync::mpsc::Sender<String>,
    mut rx: tokio::sync::mpsc::Receiver<Message>,
) {
    let ws_url = "wss://chat1.turntable.fm:8080/socket.io/websocket";
    let (ws_stream, _) = async_tungstenite::tokio::connect_async(ws_url)
        .await
        .expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();
    let m1 = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            let data = msg.unwrap().into_text().unwrap();
            tx.send(data).await.expect("failed to send data to tx");
        }
    });
    let m2 = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            write.send(msg).await.expect("failed to send data to ws");
        }
    });
    m1.await.unwrap();
    m2.await.unwrap();
}

impl Bot {
    fn new(auth: &str, user_id: &str, room_id: &str) -> Result<Bot, Box<dyn error::Error>> {
        let unix_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string();

        let b = Bot {
            auth: auth.to_string(),
            user_id: user_id.to_string(),
            room_id: room_id.to_string(),
            client_id: unix_ms,
            msg_id: 0,
            unack_msgs: Vec::new(),
            callbacks: HashMap::new(),
            log_ws: false,
        };
        Ok(b)
    }

    fn log_ws(&mut self, log_ws: bool) {
        self.log_ws = log_ws;
    }

    async fn start(&mut self) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let (tx1, rx1) = tokio::sync::mpsc::channel(32);
        tokio::spawn(start_ws(tx, rx1));
        while let Some(msg) = rx.recv().await {
            self.process_msg(&tx1, msg.as_str()).await;
        }
    }

    fn emit(&self, cmd: &str, data: &str) {
        if let Some(callbacks) = self.callbacks.get(cmd) {
            for clb in callbacks {
                (clb)(data);
            }
        }
    }

    async fn process_heartbeat(&self, tx: &tokio::sync::mpsc::Sender<Message>, msg: &str) {
        if let Some(heartbeat_id) = get_heartbeat_id(msg) {
            let msg = format!("~m~{}~m~{}", heartbeat_id.len(), heartbeat_id);
            if self.log_ws {
                println!("< {}", msg);
            }
            tx.send(Message::text(msg)).await.unwrap();
        }
    }

    async fn process_msg(&mut self, tx: &tokio::sync::mpsc::Sender<Message>, msg: &str) {
        if self.log_ws {
            println!("> {}", msg);
        }
        // Heartbeat
        if is_heartbeat(msg) {
            self.process_heartbeat(tx, msg).await;
            return;
        }

        if msg == "~m~10~m~no_session" {
            self.emit("ready", "");
            self.update_presence(tx).await;
            self.user_modify(tx).await;
            if self.room_id != "" {
                let room_id = self.room_id.clone();
                self.room_register(tx, room_id.as_str()).await;
            }
            return;
        }

        if let Some(raw_json) = extract_message_json(msg) {
            self.execute_callback(raw_json.as_str());
            self.process_command(raw_json.as_str());
        }
    }

    fn execute_callback(&mut self, raw_json: &str) {
        for (idx, unack_msg) in (&self.unack_msgs).iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(raw_json).unwrap();
            let msg_id: i64 = match v["msgid"].to_string().parse() {
                Ok(num) => num,
                Err(_) => continue,
            };
            if unack_msg.msg_id == msg_id {
                if let Some(api) = unack_msg.payload.get("api") {
                    if api == "room.register" {
                        self.emit("roomChanged", raw_json);
                    }
                }
                (unack_msg.callback)(raw_json);
                self.unack_msgs.remove(idx);
                break;
            }
        }
    }

    fn process_command(&mut self, raw_json: &str) {
        let v: serde_json::Value = serde_json::from_str(raw_json).unwrap();
        if let Some(cmd) = v["command"].as_str() {
            self.emit(cmd, raw_json);
        }
    }

    fn add_callback(&mut self, event_name: &str, clb: fn(&str)) {
        self.callbacks
            .entry(event_name.to_string())
            .or_insert_with(|| Vec::new())
            .push(clb);
    }

    pub fn on(&mut self, event_name: &str, clb: fn(&str)) {
        self.add_callback(event_name, clb);
    }

    pub fn on_chat(&mut self, clb: fn(&str)) {
        self.add_callback("speak", clb);
    }

    async fn room_register(&mut self, tx: &tokio::sync::mpsc::Sender<Message>, room_id: &str) {
        let payload = h!["api" => "room.register", "roomid" => room_id];
        let clb = |_: &str| {};
        self.send(tx, payload, clb).await;
    }

    async fn user_modify(&mut self, tx: &tokio::sync::mpsc::Sender<Message>) {
        let payload = h!["api" => "user.modify", "laptop" => "mac"];
        let clb = |_: &str| {};
        self.send(tx, payload, clb).await;
    }

    async fn update_presence(&mut self, tx: &tokio::sync::mpsc::Sender<Message>) {
        let payload = h!["api" => "presence.update", "status" => "available"];
        let clb = |_: &str| {};
        self.send(tx, payload, clb).await;
    }

    async fn send(
        &mut self,
        tx: &tokio::sync::mpsc::Sender<Message>,
        payload: HashMap<String, serde_json::Value>,
        clb: fn(&str),
    ) {
        let mut json_val = json!({
            "msgid": self.msg_id,
            "clientid": self.client_id,
            "userid": self.user_id,
            "userauth": self.auth,
            "client": "web",
        });
        let original_payload = payload.clone();
        for (k, v) in payload {
            json_val[k] = v;
        }
        let raw_json = json_val.to_string();

        let msg = format!("~m~{}~m~{}", raw_json.len(), raw_json);
        if self.log_ws {
            println!("< {}", msg);
        }
        tx.send(Message::text(msg)).await.unwrap();
        self.unack_msgs.push(UnackMsg {
            msg_id: self.msg_id,
            payload: original_payload,
            callback: clb,
        });
        self.msg_id += 1;
    }
}

async fn run() {
    let auth = std::env::var("AUTH").unwrap();
    let user_id = std::env::var("USER_ID").unwrap();
    let room_id = std::env::var("ROOM_ID").unwrap();
    let mut bot = Bot::new(auth.as_str(), user_id.as_str(), room_id.as_str()).unwrap();
    bot.log_ws(false);
    bot.on_chat(|raw_json: &str| {
        println!("chat event: {}", raw_json);
    });
    bot.on("ready", |raw_json: &str| {
        println!("bot is ready: {}", raw_json);
    });
    bot.on("registered", |raw_json: &str| {
        println!("bot registered: {}", raw_json);
    });
    bot.on("roomChanged", |raw_json: &str| {
        println!("bot roomChanged: {}", raw_json);
    });
    bot.start().await;
}

#[tokio::main]
async fn main() {
    run().await;
}
