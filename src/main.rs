use async_tungstenite::tungstenite::Message;
use futures::{SinkExt, StreamExt};
use regex::Regex;
use serde_json::json;
use std::time::SystemTime;
use std::{collections::HashMap, error};

struct Bot {
    auth: String,
    user_id: String,
    room_id: String,
    client_id: String,
    msg_id: i64,
    //ws: websocket::client::sync::Client<Box<dyn NetworkStream + Send>>,
}

macro_rules! h {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::HashMap::new();
         $( map.insert($key.to_string(), serde_json::Value::String($val.to_string())); )*
         map
    }}
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
            tx.send(data).await;
        }
    });
    let m2 = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            write.send(msg).await;
        }
    });
    m1.await;
    m2.await;
}

impl Bot {
    fn new(auth: &str, user_id: &str, room_id: &str) -> Result<Bot, Box<dyn error::Error>> {
        let b = Bot {
            auth: auth.to_string(),
            user_id: user_id.to_string(),
            room_id: room_id.to_string(),
            client_id: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                .to_string(),
            msg_id: 0,
            //ws: client,
        };
        Ok(b)
    }

    async fn start(&mut self) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let (tx1, rx1) = tokio::sync::mpsc::channel(32);
        tokio::spawn(start_ws(tx, rx1));
        while let Some(msg) = rx.recv().await {
            self.process_msg(&tx1, msg.as_str()).await;
        }
    }

    async fn process_msg(&mut self, tx: &tokio::sync::mpsc::Sender<Message>, data: &str) {
        println!("> {}", data);
        // Heartbeat
        let re = Regex::new(r"^~m~[0-9]+~m~(~h~[0-9]+)$").unwrap();
        if re.is_match(data) {
            if let Some(captures) = re.captures(data) {
                if let Some(heartbeat_id) = captures.get(1) {
                    let msg = format!(
                        "~m~{}~m~{}",
                        heartbeat_id.as_str().len(),
                        heartbeat_id.as_str()
                    );
                    println!("< {}", msg);
                    tx.send(Message::text(msg)).await;
                }
            }
        }

        if data == "~m~10~m~no_session" {
            self.update_presence(tx).await;
            self.user_modify(tx).await;
            if self.room_id != "" {
                let room_id = self.room_id.clone();
                self.room_register(tx, room_id.as_str()).await;
            }
        }
    }

    async fn room_register(&mut self, tx: &tokio::sync::mpsc::Sender<Message>, room_id: &str) {
        let payload = h!["api" => "room.register", "roomid" => room_id];
        let clb = |_: Vec<u8>| {};
        self.send(tx, payload, clb).await;
    }

    async fn user_modify(&mut self, tx: &tokio::sync::mpsc::Sender<Message>) {
        let payload = h!["api" => "user.modify", "laptop" => "mac"];
        let clb = |_: Vec<u8>| {};
        self.send(tx, payload, clb).await;
    }

    async fn update_presence(&mut self, tx: &tokio::sync::mpsc::Sender<Message>) {
        let payload = h!["api" => "presence.update", "status" => "available"];
        let clb = |_: Vec<u8>| {};
        self.send(tx, payload, clb).await;
    }

    async fn send(
        &mut self,
        tx: &tokio::sync::mpsc::Sender<Message>,
        payload: HashMap<String, serde_json::Value>,
        _: fn(Vec<u8>),
    ) {
        let mut json_val = json!({
            "msgid": self.msg_id,
            "clientid": self.client_id,
            "userid": self.user_id,
            "userauth": self.auth,
            "client": "web",
        });
        for (k, v) in payload {
            json_val[k] = v;
        }
        let raw_json = json_val.to_string();

        let msg = format!("~m~{}~m~{}", raw_json.len(), raw_json);
        println!("< {}", msg);
        tx.send(Message::text(msg)).await;
        self.msg_id += 1;
    }
}

async fn run() {
    println!();
    let mut bot = Bot::new(
        "AOhSRFcOVYYAgpRVPKwXGlBM",
        "604156e1c2dbd9001be73ffc",
        "6041625e3f4bfc001c3a4ab3",
    )
    .unwrap();
    bot.start().await;
}

#[tokio::main]
async fn main() {
    run().await;
}
