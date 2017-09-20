extern crate openssl;
extern crate rusqlite;
extern crate rustyline;
extern crate termion;
extern crate common;

use common::Packet;
use openssl::ssl::{SslConnectorBuilder, SslMethod, SslStream};
use rusqlite::Connection as SqlConnection;
use rustyline::error::ReadlineError;
use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use termion::color;
use termion::cursor;
use termion::screen::AlternateScreen;

macro_rules! flush {
    () => {
        io::stdout().flush().unwrap();
    }
}
macro_rules! readline {
    ($editor:expr, $prompt:expr, $break:expr) => {
        match $editor.readline($prompt) {
            Ok(ok) => ok,
            Err(ReadlineError::Eof) |
            Err(ReadlineError::Interrupted) => $break,
            Err(err) => {
                println!("Couldn't read line: {}", err);
                $break;
            }
        }
    }
}

mod connect;
mod listener;
mod parser;

pub struct Session {
    attributes: HashMap<usize, common::Attribute>,
    channel: Option<usize>,
    channels: HashMap<usize, common::Channel>,
    id: usize,
    last: Option<(usize, Vec<u8>)>,
    stream: SslStream<TcpStream>,
    users: HashMap<usize, common::User>
}
impl Session {
    pub fn new(id: usize, stream: SslStream<TcpStream>) -> Session {
        Session {
            attributes: HashMap::new(),
            channel: None,
            channels: HashMap::new(),
            id: id,
            last: None,
            stream: stream,
            users: HashMap::new()
        }
    }
}

fn main() {
    let db = match SqlConnection::open("data.sqlite") {
        Ok(ok) => ok,
        Err(err) => {
            println!("Failed to open database");
            println!("{}", err);
            return;
        }
    };

    db.execute("CREATE TABLE IF NOT EXISTS servers (
                    ip      TEXT NOT NULL UNIQUE,
                    key     BLOB NOT NULL,
                    token   TEXT
                )", &[])
        .expect("Couldn't create SQLite table");

    #[cfg(unix)]
    let mut nick = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    #[cfg(windows)]
    let mut nick = env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string());
    #[cfg(not(any(unix, windows)))]
    let mut nick = "unknown".to_string();

    let ssl = SslConnectorBuilder::new(SslMethod::tls())
        .expect("Failed to create SSL connector D:")
        .build();
    let session: Arc<Mutex<Option<Session>>> = Arc::new(Mutex::new(None));
    let session_clone = Arc::clone(&session);

    let _screen = AlternateScreen::from(io::stdout());

    println!("{}Welcome, {}", cursor::Goto(1, 1), nick);
    println!("To quit, type /quit");
    println!("To change your name, type");
    println!("/nick <name>");
    println!("To connect to a server, type");
    println!("/connect <ip>");
    println!();

    let (sent_sender, sent_receiver) = mpsc::sync_channel(0);

    thread::spawn(move || {
        listener::listen(session_clone, sent_sender);
    });

    let mut editor = rustyline::Editor::<()>::new();
    loop {
        to_terminal_bottom();
        let input = readline!(editor, "> ", { break; });
        print!("{}", cursor::Restore);
        if input.is_empty() {
            continue;
        }
        editor.add_history_entry(&input);

        if input.starts_with('/') {
            let mut args = parser::parse(&input[1..]);
            if args.is_empty() {
                continue;
            }
            let command = args.remove(0);

            macro_rules! usage_min {
                ($amount:expr, $usage:expr) => {
                    if args.len() < $amount {
                        println!(concat!("Usage: /", $usage));
                        continue;
                    }
                }
            }
            macro_rules! usage_max {
                ($amount:expr, $usage:expr) => {
                    if args.len() > $amount {
                        println!(concat!("Usage: /", $usage));
                        continue;
                    }
                }
            }
            macro_rules! usage {
                ($amount:expr, $usage:expr) => {
                    if args.len() != $amount {
                        println!(concat!("Usage: /", $usage));
                        continue;
                    }
                }
            }

            macro_rules! require_session {
                ($session:expr) => {
                    match *$session {
                        Some(ref mut s) => s,
                        None => {
                            println!("You're not connected to a server");
                            continue;
                        }
                    }
                }
            }

            match &*command {
                "quit" => break,
                "nick" => {
                    usage!(1, "nick <name>");
                    nick = args.remove(0);
                    println!("Your name is now {}", nick);
                },
                "connect" => {
                    usage!(1, "connect <ip[:port]>");
                    let mut session = session.lock().unwrap();
                    if session.is_some() {
                        println!("Please disconnect first");
                        continue;
                    }
                    *session = connect::connect(&db, &nick, &args[0], &mut editor, &ssl);
                },
                "disconnect" => {
                    usage!(0, "disconnect");
                    let mut session = session.lock().unwrap();
                    {
                        let session = require_session!(session);
                        let _ = common::write(&mut session.stream, &Packet::Close);
                    }
                    *session = None;
                },
                "forget" => {
                    usage!(1, "forget <ip>");
                    let addr = match parse_ip(&args[0]) {
                        Some(some) => some,
                        None => {
                            println!("Not a valid IP");
                            continue;
                        }
                    };
                    db.execute("DELETE FROM servers WHERE ip = ?", &[&addr.to_string()]).unwrap();
                },
                "create" => {
                    usage_min!(2, "create <\"attribute\"/\"channel\"> <name> [data]");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let packet = match &*args[0] {
                        "channel" => {
                            usage_max!(2, "create channel <name>");
                            let mut name = args.remove(1);
                            if name.starts_with('#') {
                                name.drain(..1);
                            }
                            Packet::ChannelCreate(common::ChannelCreate {
                                overrides: Vec::new(),
                                name: name
                            })
                        },
                        "attribute" => {
                            usage_max!(3, "create attribute <name> [data]");
                            let (mut allow, mut deny) = (0, 0);
                            if args.len() == 3 {
                                if !from_perm_string(&*args[2], &mut allow, &mut deny) {
                                    println!("Invalid permission string");
                                    continue;
                                }
                            }
                            let max = session.attributes.values().max_by_key(|item| item.pos);
                            Packet::AttributeCreate(common::AttributeCreate {
                                allow: allow,
                                deny: deny,
                                name: args.remove(1),
                                pos: max.map_or(1, |item| item.pos+1),
                                unassignable: false
                            })
                        },
                        _ => { println!("Can't create that"); continue; }
                    };
                    if let Err(err) = common::write(&mut session.stream, &packet) {
                        println!("Failed to send packet");
                        println!("{}", err);
                    }
                },
                "update" => {
                    usage!(2, "update <\"attribute\"/\"channel\"> <id>");

                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let id = match args[1].parse() {
                        Ok(ok) => ok,
                        Err(_) => {
                            println!("Not a valid number");
                            continue;
                        }
                    };

                    let packet = match &*args[0] {
                        "attribute" => if let Some(attribute) = session.attributes.get(&id) {
                            let (mut allow, mut deny) = (attribute.allow, attribute.deny);

                            println!("Editing: {}", attribute.name);
                            println!("(Press enter to keep current value)");
                            println!();

                            let name = readline!(editor,
                                &format!("Name [{}]: ", attribute.name),
                                { continue; }
                            );
                            let mut name = name.trim();
                            if name.is_empty() { name = &attribute.name };
                            let perms = readline!(
                                editor,
                                &format!("Permission [{}]: ", to_perm_string(allow, deny)),
                                { continue; }
                            );
                            if !from_perm_string(&perms, &mut allow, &mut deny) {
                                println!("Invalid permission string");
                                continue;
                            }
                            println!("{} {}", allow, deny);
                            let pos = readline!(
                                editor,
                                &format!("Position [{}]: ", attribute.pos),
                                { continue; }
                            );
                            let pos = pos.trim();
                            let pos = if pos.is_empty() {
                                attribute.pos
                            } else {
                                match pos.parse() {
                                    Ok(ok) => ok,
                                    Err(_) => {
                                        println!("Not a valid number");
                                        continue;
                                    }
                                }
                            };

                            Some(Packet::AttributeUpdate(common::AttributeUpdate {
                                inner: common::Attribute {
                                    allow: allow,
                                    deny: deny,
                                    id: attribute.id,
                                    name: name.to_string(),
                                    pos: pos,
                                    unassignable: attribute.unassignable
                                }
                            }))
                        } else { None },
                        "channel" => if let Some(channel) = session.channels.get(&id) {
                            println!("Editing: #{}", channel.name);
                            println!("(Press enter to keep current value)");
                            println!();

                            let name = readline!(
                                editor,
                                &format!("Name [{}]: ", channel.name),
                                { continue; }
                            );
                            let mut name = name.trim();
                            if name.is_empty() { name = &channel.name }
                            Some(Packet::ChannelUpdate(common::ChannelUpdate {
                                inner: common::Channel {
                                    id: channel.id,
                                    name: name.to_string(),
                                    overrides: Vec::new()
                                },
                                keep_overrides: true
                            }))
                        } else { None },
                        "user" => if let Some(user) = session.users.get(&id) {
                            let mut attributes = user.attributes.clone();
                            loop {
                                let result = attributes.iter().fold(String::new(), |mut acc, item| {
                                    if !acc.is_empty() { acc.push_str(", "); }
                                    acc.push_str(&item.to_string());
                                    acc
                                });
                                println!("Attributes: [{}]", result);
                                println!("Commands: add, remove, quit");

                                let line = readline!(
                                    editor,
                                    "> ",
                                    { break; }
                                );

                                let parts = parser::parse(&line);
                                if parts.is_empty() { continue; }
                                let add = match &*parts[0] {
                                    "add" => true,
                                    "remove" => false,
                                    "quit" => break,
                                    _ => {
                                        println!("Unknown command");
                                        continue;
                                    }
                                };

                                if parts.len() != 2 {
                                    println!("Usage: add/remove <id>");
                                    break;
                                }
                                let id = match parts[1].parse() {
                                    Ok(ok) => ok,
                                    Err(_) => {
                                        println!("Invalid ID");
                                        continue;
                                    }
                                };
                                if let Some(attribute) = session.attributes.get(&id) {
                                    if attribute.pos == 0 {
                                        println!("Can't assign that attribute");
                                        continue;
                                    }
                                    if add {
                                        println!("Added: {}", attribute.name);
                                        attributes.push(id);
                                    } else {
                                        println!("Removed: {}", attribute.name);
                                        attributes.retain(|item| *item != id);
                                        // TODO remove_item once stable
                                    }
                                } else {
                                    println!("Couldn't find attribute");
                                }
                            }
                            Some(Packet::UserUpdate(common::UserUpdate {
                                attributes: attributes,
                                id: id
                            }))
                        } else { None },
                        _ => {
                            println!("Can't delete that");
                            continue;
                        }
                    };
                    if let Some(packet) = packet {
                        if let Err(err) = common::write(&mut session.stream, &packet) {
                            println!("Oh no could not delete");
                            println!("{}", err);
                        }
                    } else {
                        println!("Nothing with that ID");
                    }
                },
                "delete" => {
                    usage!(2, "delete <\"attribute\"/\"channel\"/\"message\"> <id>");

                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let id = match args[1].parse() {
                        Ok(ok) => ok,
                        Err(_) => {
                            println!("Failed to parse ID");
                            continue;
                        }
                    };

                    let packet = match &*args[0] {
                        "attribute" => if session.attributes.contains_key(&id) {
                            Some(Packet::AttributeDelete(common::AttributeDelete {
                                id: id
                            }))
                        } else { None },
                        "channel" => if session.channels.contains_key(&id) {
                            Some(Packet::ChannelDelete(common::ChannelDelete {
                                id: id
                            }))
                        } else { None },
                        "message" =>
                            Some(Packet::MessageDelete(common::MessageDelete {
                                id: id
                            })),
                        _ => {
                            println!("Can't delete that");
                            continue;
                        }
                    };
                    if let Some(packet) = packet {
                        if let Err(err) = common::write(&mut session.stream, &packet) {
                            println!("Oh no could not delete");
                            println!("{}", err);
                        }
                    } else {
                        println!("Nothing with that ID");
                    }
                },
                "list" => {
                    usage!(1, "list <\"attributes\"/\"channels\"/\"users\">");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    match &*args[0] {
                        "channels" => {
                            let mut result = String::new();
                            // Yeah, cloning this is bad... But what can I do? I need to sort!
                            // Though, I suspect this might be a shallow copy.
                            let mut channels: Vec<_> = session.channels.values().collect();
                            channels.sort_by_key(|item| &item.name);
                            for channel in channels {
                                if !result.is_empty() { result.push_str(", "); }
                                result.push('#');
                                result.push_str(&channel.name);
                            }
                            println!("{}", result);
                        },
                        "attributes" => {
                            let mut result = String::new();
                            // Read the above comment, thank you ---------------------------^
                            let mut attributes: Vec<_> = session.attributes.values().collect();
                            attributes.sort_by_key(|item| &item.pos);
                            for user in attributes {
                                if !result.is_empty() { result.push_str(", "); }
                                result.push_str(&user.name);
                            }
                            println!("{}", result);
                        },
                        "users" => {
                            let mut result = String::new();
                            // something something above comment
                            let mut users: Vec<_> = session.users.values().collect();
                            users.sort_by_key(|item| &item.name);
                            for banned in &[false, true] {
                                if *banned {
                                    result.push_str("\nBanned:\n");
                                }
                                for attribute in &users {
                                    if attribute.ban != *banned { continue; }
                                    if !result.is_empty() { result.push_str(", "); }
                                    result.push_str(&attribute.name);
                                }
                            }
                            println!("{}", result);
                        },
                        _ => println!("Can't list that")
                    }
                },
                "info" => {
                    usage!(1, "info <attribute or channel name>");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let mut name = &*args[0];
                    if name.starts_with('#') {
                        name = &name[1..];
                    }

                    let id = name.parse();

                    println!();
                    for channel in session.channels.values() {
                        if name == channel.name
                            || (name.starts_with('#') && name[1..] == channel.name)
                            || Ok(channel.id) == id {
                            println!("Channel #{}", channel.name);
                            println!("ID: #{}", channel.id);
                            for &(id, (allow, deny)) in &channel.overrides {
                                println!("Permission override: Role #{} = {}", id, to_perm_string(allow, deny));
                            }
                        }
                    }
                    for attribute in session.attributes.values() {
                        if name == attribute.name || Ok(attribute.id) == id {
                            println!("Attribute {}", attribute.name);
                            println!("Permission: {}", to_perm_string(attribute.allow, attribute.deny));
                            println!("ID: #{}", attribute.id);
                            println!("Position: {}", attribute.pos);
                        }
                    }
                    for user in session.users.values() {
                        if name == user.name || Ok(user.id) == id {
                            println!("User {}", user.name);
                            println!(
                                "Attributes: [{}]",
                                user.attributes.iter()
                                    .fold(String::new(), |mut acc, id| {
                                        if !acc.is_empty() {
                                            acc.push_str(", ");
                                        }
                                        acc.push_str(&id.to_string());
                                        acc
                                    })
                            );
                            if user.ban {
                                println!("Banned.");
                            }
                            println!("Bot: {}", if user.bot { "true" } else { "false" });
                            println!("ID: #{}", user.id);
                        }
                    }
                },
                "join" => {
                    usage!(1, "join <channel name>");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let mut name = &*args[0];
                    if name.starts_with('#') {
                        name = &name[1..];
                    }

                    let mut found = false;
                    for channel in session.channels.values() {
                        if channel.name == name {
                            session.channel = Some(channel.id);
                            println!("{}{}Joined channel #{}", termion::clear::All, cursor::Goto(1, 1), channel.name);
                            let packet = Packet::MessageList(common::MessageList {
                                after: None,
                                before: None,
                                channel: channel.id,
                                limit: common::LIMIT_MESSAGE_LIST
                            });
                            if let Err(err) = common::write(&mut session.stream, &packet) {
                                println!("Failed to send message_list packet");
                                println!("{}", err);
                            }
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        println!("No channel found with that name");
                    }
                },
                _ => {
                    println!("Unknown command");
                }
            }
            continue;
        }

        match *session.lock().unwrap() {
            None => {
                println!("You're not connected to a server");
                continue;
            },
            Some(ref mut session) => {
                if let Some(channel) = session.channel {
                    let packet = if input.starts_with("s/") && session.last.is_some() {
                        let mut parts = input[2..].splitn(2, '/');
                        let find = match parts.next() {
                            Some(some) => some,
                            None => continue
                        };
                        let replace = match parts.next() {
                            Some(some) => some,
                            None => continue
                        };
                        let last = session.last.as_ref().unwrap();
                        Packet::MessageUpdate(common::MessageUpdate {
                            id: last.0,
                            text: String::from_utf8_lossy(&last.1).replace(find, replace).into_bytes()
                        })
                    } else {
                        print!("{}{}: {}{}", color::Fg(color::LightBlack), nick, input, color::Fg(color::Reset));
                        flush!();
                        Packet::MessageCreate(common::MessageCreate {
                            channel: channel,
                            text: input.into_bytes()
                        })
                    };

                    if let Err(err) = common::write(&mut session.stream, &packet) {
                        println!("\rFailed to deliver message");
                        println!("{}", err);
                        continue;
                    }
                } else {
                    println!("No channel specified. See /create channel, /list channels and /join");
                    continue;
                }
            }
        }
        // Could only happen if message was sent
        if let Err(mpsc::RecvTimeoutError::Timeout) = sent_receiver.recv_timeout(Duration::from_secs(2)) {
            println!("Failed to verify message was received...");
        }
    }
}

fn to_terminal_bottom() {
    if let Ok((_, height)) = termion::terminal_size() {
        print!("{}{}", cursor::Save, cursor::Goto(0, height-1));
    }
}

fn to_perm_string(allow: u8, deny: u8) -> String {
    let mut result = String::with_capacity(10);

    if allow != 0 {
        result.push('+');
        result.push_str(&to_single_perm_string(allow));
    }
    if deny != 0 {
        result.push('-');
        result.push_str(&to_single_perm_string(deny));
    }

    result.shrink_to_fit();
    result
}
fn to_single_perm_string(bitmask: u8) -> String {
    let mut result = String::with_capacity(4);

    if bitmask & common::PERM_READ == common::PERM_READ {
        result.push('r');
    }
    if bitmask & common::PERM_WRITE == common::PERM_WRITE {
        result.push('w');
    }
    if bitmask & common::PERM_ASSIGN_ATTRIBUTES == common::PERM_ASSIGN_ATTRIBUTES {
        result.push('s');
    }
    if bitmask & common::PERM_MANAGE_ATTRIBUTES == common::PERM_MANAGE_ATTRIBUTES {
        result.push('a');
    }
    if bitmask & common::PERM_MANAGE_CHANNELS == common::PERM_MANAGE_CHANNELS {
        result.push('c');
    }
    if bitmask & common::PERM_MANAGE_MESSAGES == common::PERM_MANAGE_MESSAGES {
        result.push('m');
    }

    result
}
fn from_perm_string(input: &str, allow: &mut u8, deny: &mut u8) -> bool {
    let mut mode = '+';

    for c in input.chars() {
        if c == '+' || c == '-' || c == '=' {
            mode = c;
            continue;
        }
        let perm = match c {
            'r' => common::PERM_READ,
            'w' => common::PERM_WRITE,
            's' => common::PERM_ASSIGN_ATTRIBUTES,
            'a' => common::PERM_MANAGE_ATTRIBUTES,
            'c' => common::PERM_MANAGE_CHANNELS,
            'm' => common::PERM_MANAGE_MESSAGES,
            ' ' => continue,
            _   => return false
        };
        match mode {
            '+' => { *allow |= perm; *deny &= !perm; },
            '-' => { *allow &= !perm; *deny |= perm },
            '=' => { *allow &= !perm; *deny &= !perm }
            _   => unreachable!()
        }
    }

    true
}

fn parse_ip(input: &str) -> Option<SocketAddr> {
    let mut parts = input.split(':');
    let ip = match parts.next() {
        Some(some) => some,
        None => return None
    };
    let port = match parts.next() {
        Some(some) => match some.parse() {
            Ok(ok) => ok,
            Err(_) => return None
        },
        None => common::DEFAULT_PORT
    };
    if parts.next().is_some() {
        return None;
    }

    use std::net::ToSocketAddrs;
    match (ip, port).to_socket_addrs() {
        Ok(ok) => ok,
        Err(_) => return None
    }.next()
}
