use async_tungstenite::tokio::connect_async;
use async_tungstenite::tungstenite::Message;
use futures::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cell::RefCell;
use std::time::SystemTime;
use std::{collections::HashMap, env};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const ROOM_REGISTER: &str = "room.register";
const USER_MODIFY: &str = "user.modify";
const PRESENCE_UPDATE: &str = "presence.update";
const ROOM_SPEAK: &str = "room.speak";

const SPEAK_EVT: &str = "speak";
const PMMED_EVT: &str = "pmmed";

// Valid statuses
const AVAILABLE: &str = "available";
//unavailable = "unavailable"
//away        = "away"

// Valid laptops
//const ANDROID_LAPTOP: &str = "android";
// chromeLaptop  = "chrome"
// iphoneLaptop  = "iphone"
// linuxLaptop   = "linux"
const MAC_LAPTOP: &str = "mac";
//pcLaptop      = "pc"

// Valid clients
const WEB_CLIENT: &str = "web";

// // Just a generic Result type to ease error handling for us. Errors in multithreaded
// // async contexts needs some extra restrictions
// type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct UnackMsg {
    msg_id: i64,
    payload: HashMap<String, serde_json::Value>,
    callback: Option<fn(&Bot, &str)>,
}

#[derive(Default)]
struct Bot {
    auth: String,
    user_id: String,
    room_id: String,
    client_id: String,
    client: String,
    msg_id: RefCell<i64>,
    // unack_msgs must not be borrowed while calling any callbacks
    unack_msgs: RefCell<Vec<UnackMsg>>,
    callbacks: HashMap<String, Vec<fn(&Bot, &str)>>,
    speak_callbacks: Vec<fn(&Bot, SpeakEvt)>,
    pmmed_callbacks: Vec<fn(&Bot, PmmedEvt)>,
    log_ws: bool,
    tx: Option<UnboundedSender<Message>>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpeakEvt {
    command: String,
    userid: String,
    name: String,
    text: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PmmedEvt {
    pub text: String,
    pub userid: String,
    pub senderid: String,
    pub command: String,
    pub time: f64,
}

macro_rules! h {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::HashMap::new();
         $( map.insert($key.to_string(), serde_json::Value::String($val.to_string())); )*
         map
    }}
}

// Compare "cmd" with "cmp", if match, json parse "data" using "typ" and execute provided callbacks
macro_rules! execute_callbacks {
    ( $self: expr, $cmd: expr, $data: expr, $( ($cmp: expr, $callbacks: expr, $typ: ty)),*) => {{
        $(
            if $cmd == $cmp {
                if let Ok(evt) = serde_json::from_str::<$typ>($data).map_err(|err| log::error!("{}", err)) {
                    $callbacks.iter().for_each(|clb| (clb)($self, evt.clone()));
                }
            }
        )*
    }};
}

lazy_static! {
    static ref HEARTBEAT_RGX: Regex = Regex::new(r"^~m~[0-9]+~m~(~h~[0-9]+)$").unwrap();
    static ref LEN_RGX: Regex = Regex::new(r"^~m~([0-9]+)~m~").unwrap();
}

fn get_message_len(msg: &str) -> Option<usize> {
    let captures = LEN_RGX.captures(msg)?;
    let msg_len_str = captures.get(1)?;
    msg_len_str.as_str().parse().ok()
}

// Extract the json part of a websocket message
fn extract_message_json(msg: &str) -> Option<String> {
    let msg_len = get_message_len(msg)?;
    let start_idx = msg.find("{")?;
    Some(msg.chars().skip(start_idx).take(msg_len).collect())
}

fn is_heartbeat(msg: &str) -> bool {
    HEARTBEAT_RGX.is_match(msg)
}

fn get_heartbeat_id(msg: &str) -> Option<&str> {
    let captures = HEARTBEAT_RGX.captures(msg)?;
    let heartbeat_id = captures.get(1)?;
    Some(heartbeat_id.as_str())
}

async fn start_ws(tx: UnboundedSender<String>, mut rx: UnboundedReceiver<Message>) {
    let ws_url = "wss://chat1.turntable.fm:8080/socket.io/websocket";
    let (ws_stream, _) = connect_async(ws_url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();
    let m1 = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            let data = msg.unwrap().into_text().unwrap();
            tx.send(data).expect("failed to send data to tx");
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
    pub fn new(auth: &str, user_id: &str, room_id: &str) -> Bot {
        let unix_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string();

        Bot {
            auth: auth.to_string(),
            user_id: user_id.to_string(),
            room_id: room_id.to_string(),
            client_id: unix_ms,
            client: WEB_CLIENT.to_string(),
            ..Default::default()
        }
    }

    pub fn on(&mut self, event_name: &str, clb: fn(&Bot, &str)) {
        self.add_callback(event_name, clb);
    }

    pub fn on_speak(&mut self, clb: fn(&Bot, SpeakEvt)) {
        self.speak_callbacks.push(clb);
    }

    pub fn on_pmmed(&mut self, clb: fn(&Bot, PmmedEvt)) {
        self.pmmed_callbacks.push(clb);
    }

    pub fn speak(&self, msg: &str) {
        let payload = h!["api" => ROOM_SPEAK, "text" => msg, "room_id" => self.room_id];
        self.send(payload, None);
    }

    pub fn log_ws(&mut self, log_ws: bool) {
        self.log_ws = log_ws;
    }

    async fn start(&mut self) {
        let (tx, mut rx) = unbounded_channel();
        let (tx1, rx1) = unbounded_channel();
        self.tx = Some(tx1);
        tokio::spawn(start_ws(tx, rx1));
        while let Some(msg) = rx.recv().await {
            self.process_msg(msg.as_str()).await;
        }
    }

    fn emit(&self, cmd: &str, data: &str) {
        // Execute event specific callbacks
        execute_callbacks!(
            self,
            cmd,
            data,
            (SPEAK_EVT, self.speak_callbacks, SpeakEvt),
            (PMMED_EVT, self.pmmed_callbacks, PmmedEvt)
        );

        // Execute string registered key, callbacks
        if let Some(callbacks) = self.callbacks.get(cmd) {
            for clb in callbacks {
                (clb)(self, data);
            }
        }
    }

    async fn process_heartbeat(&self, msg: &str) {
        if let Some(heartbeat_id) = get_heartbeat_id(msg) {
            let msg = format!("~m~{}~m~{}", heartbeat_id.len(), heartbeat_id);
            if self.log_ws {
                println!("< {}", msg);
            }
            let tx = self.tx.as_ref().unwrap();
            tx.send(Message::text(msg)).unwrap();
        }
    }

    async fn process_msg(&mut self, msg: &str) {
        if self.log_ws {
            println!("> {}", msg);
        }
        // Heartbeat
        if is_heartbeat(msg) {
            self.process_heartbeat(msg).await;
            return;
        }

        if msg == "~m~10~m~no_session" {
            self.emit("ready", "");
            self.update_presence().await;
            self.user_modify().await;
            if self.room_id != "" {
                let room_id = self.room_id.clone();
                self.room_register(room_id.as_str()).await;
            }
            return;
        }

        if let Some(raw_json) = extract_message_json(msg) {
            self.execute_callback(raw_json.as_str());
            self.process_command(raw_json.as_str());
        }
    }

    fn execute_callback(&self, raw_json: &str) {
        let unack_msg = {
            let mut unack_msgs = self.unack_msgs.borrow_mut();
            if unack_msgs.is_empty() {
                return;
            }
            let v: serde_json::Value = serde_json::from_str(raw_json).unwrap();
            let msg_id: i64 = match v["msgid"].to_string().parse() {
                Ok(num) => num,
                Err(_) => return,
            };
            let i = match (0..unack_msgs.len()).find(|&i| unack_msgs[i].msg_id == msg_id) {
                Some(i) => i,
                None => return,
            };
            unack_msgs.remove(i)
            // Release unack_msgs before calling the callbacks
        };
        if let Some(api) = unack_msg.payload.get("api") {
            if api == ROOM_REGISTER {
                self.emit("roomChanged", raw_json);
            }
        }
        if let Some(clb) = unack_msg.callback {
            (clb)(self, raw_json);
        }
    }

    fn process_command(&mut self, raw_json: &str) {
        let v: serde_json::Value = serde_json::from_str(raw_json).unwrap();
        if let Some(cmd) = v["command"].as_str() {
            self.emit(cmd, raw_json);
        }
    }

    fn add_callback(&mut self, event_name: &str, clb: fn(&Bot, &str)) {
        self.callbacks
            .entry(event_name.to_string())
            .or_insert_with(|| Vec::new())
            .push(clb);
    }

    // TODO: room_register, user_modify, update_presence don't need to be async
    async fn room_register(&mut self, room_id: &str) {
        let payload = h!["api" => ROOM_REGISTER, "roomid" => room_id];
        self.send(payload, None);
    }

    async fn user_modify(&mut self) {
        let payload = h!["api" => USER_MODIFY, "laptop" => MAC_LAPTOP];
        self.send(payload, None);
    }

    async fn update_presence(&mut self) {
        let payload = h!["api" => PRESENCE_UPDATE, "status" => AVAILABLE];
        self.send(payload, None);
    }

    fn send(&self, payload: HashMap<String, serde_json::Value>, clb: Option<fn(&Bot, &str)>) {
        let mut json_val = json!({
            "msgid": self.msg_id,
            "clientid": self.client_id,
            "userid": self.user_id,
            "userauth": self.auth,
            "client": self.client,
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
        let tx = self.tx.as_ref().unwrap();
        tx.send(Message::text(msg)).unwrap();
        let mut msg_id = self.msg_id.borrow_mut();
        self.unack_msgs.borrow_mut().push(UnackMsg {
            msg_id: *msg_id,
            payload: original_payload,
            callback: clb,
        });
        *msg_id += 1;
    }
}

async fn run() {
    let auth = env::var("AUTH").expect("AUTH not defined");
    let user_id = env::var("USER_ID").expect("USER_ID not defined");
    let room_id = env::var("ROOM_ID").expect("ROOM_ID not defined");
    let mut bot = Bot::new(auth.as_str(), user_id.as_str(), room_id.as_str());
    bot.log_ws(true);
    bot.on_speak(|bot, evt: SpeakEvt| {
        println!("chat event: {} ({}) => {}", evt.name, evt.userid, evt.text);
        if evt.text == "/ping" {
            bot.speak("pong");
        }
    });
    bot.on_pmmed(|_, evt: PmmedEvt| {
        println!("pm event: {} => {}", evt.senderid, evt.text);
    });
    bot.on("ready", |_, raw_json: &str| {
        println!("bot is ready: {}", raw_json);
    });
    bot.on("registered", |_, raw_json: &str| {
        println!("bot registered: {}", raw_json);
    });
    bot.on("roomChanged", |_, raw_json: &str| {
        println!("bot roomChanged: {}", raw_json);
    });
    bot.start().await;
}

#[tokio::main]
async fn main() {
    env_logger::Builder::new()
        .target(env_logger::Target::Stdout)
        .init();

    run().await;
}
