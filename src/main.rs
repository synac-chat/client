extern crate openssl;
extern crate rusqlite;
extern crate rustyline;
extern crate synac;
extern crate xdg;

use synac::State;
use synac::common::{self, Packet};
use rusqlite::Connection as SqlConnection;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use xdg::BaseDirectories;

mod connect;
mod frontend;
mod help;
mod listener;
mod parser;

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum LogEntryId {
    Message(usize),
    None,
    Sending
}

pub struct Connector {
    db:   Arc<Mutex<SqlConnection>>,
    nick: RwLock<String>,
}
pub struct Session {
    inner: synac::Session,
    state: synac::State,

    addr: SocketAddr,
    channel: Option<usize>,
    id: usize,
    last: Option<(usize, Vec<u8>)>,
    typing: HashMap<(usize, usize), Instant>
}
impl Session {
    pub fn new(addr: SocketAddr, id: usize, inner: synac::Session) -> Session {
        Session {
            inner: inner,
            state: State::new(),

            addr: addr,
            channel: None,
            id: id,
            last: None,
            typing: HashMap::new(),
        }
    }
}

fn main() {
    let session: Arc<Mutex<Option<Session>>> = Arc::new(Mutex::new(None));

    let basedirs = match BaseDirectories::with_prefix("synac") {
        Ok(basedirs) => basedirs,
        Err(err) => {
            eprintln!("Failed to get XDG basedirs");
            eprintln!("{}", err);
            return;
        }
    };

    let data_path;
    if let Some(path) = basedirs.find_data_file("data2.sqlite") {
        data_path = path;
    } else {
        let mut rename = None;
        if let Some(path) = basedirs.find_config_file("data.sqlite") {
            rename = Some(path);
        }
        match basedirs.place_data_file("data2.sqlite") {
            Ok(path) => {
                if let Some(path1) = rename {
                    if let Err(err) = fs::rename(&path1, &path) {
                        eprintln!("Failed to move old sqlite config to new one");
                        eprintln!("{}", err);
                    }
                }
                data_path = path;
            },
            Err(err) => {
                eprintln!("Failed to place data file");
                eprintln!("{}", err);
                return;
            }
        }
    }

    let db = match SqlConnection::open(&data_path) {
        Ok(ok) => ok,
        Err(err) => {
            eprintln!("Failed to open database");
            eprintln!("{}", err);
            return;
        }
    };
    db.execute("CREATE TABLE IF NOT EXISTS data (
                    key     TEXT NOT NULL PRIMARY KEY UNIQUE,
                    value   TEXT NOT NULL
                )", &[])
        .expect("Couldn't create SQLite table");
    db.execute("CREATE TABLE IF NOT EXISTS servers (
                    ip      TEXT NOT NULL PRIMARY KEY,
                    key     BLOB NOT NULL,
                    token   TEXT
                )", &[])
        .expect("Couldn't create SQLite table");

    let nick = {
        let mut stmt = db.prepare("SELECT value FROM data WHERE key = 'nick'").unwrap();
        let mut rows = stmt.query(&[]).unwrap();

        if let Some(row) = rows.next() {
            row.unwrap().get::<_, String>(0)
        } else {
            #[cfg(unix)]
            { env::var("USER").unwrap_or_else(|_| String::from("unknown")) }
            #[cfg(windows)]
            { env::var("USERNAME").unwrap_or_else(|_| String::from("unknown")) }
            #[cfg(not(any(unix, windows)))]
            { String::from("unknown") }
        }
    };

    let db = Arc::new(Mutex::new(db));

    let screen = Arc::new(frontend::Screen::new(&session));
    // See https://github.com/rust-lang/rust/issues/35853
    macro_rules! println {
        () => { screen.log(String::new()); };
        ($arg:expr) => { screen.log(String::from($arg)); };
        ($($arg:expr),*) => { screen.log(format!($($arg),*)); };
    }
    macro_rules! readline {
        ($break:block) => {
            match screen.readline() {
                Ok(ok) => ok,
                Err(_) => $break
            }
        }
    }
    macro_rules! readpass {
        ($break:block) => {
            match screen.readpass() {
                Ok(ok) => ok,
                Err(_) => $break
            }
        }
    }


    println!("Welcome, {}", nick);
    println!("To quit, type /quit");
    println!("To change your name, type");
    println!("/nick <name>");
    println!("To connect to a server, type");
    println!("/connect <ip>");
    println!();

    let connector = Arc::new(Connector {
        db: Arc::clone(&db),
        nick: RwLock::new(nick)
    });

    macro_rules! write {
        ($session:expr, $packet:expr, $break:block) => {
            if !connect::write(&connector, &$packet, &*screen, $session) {
                $break
            }
        }
    }

    let (tx_sent, rx_sent) = mpsc::sync_channel(0);
    let (tx_stop, rx_stop) = mpsc::channel();

    let db_clone      = Arc::clone(&db);
    let screen_clone  = Arc::clone(&screen);
    let session_clone = Arc::clone(&session);
    let thread = thread::spawn(move || {
        listener::listen(&db_clone, &screen_clone, &tx_sent, &session_clone, &rx_stop);
    });

    loop {
        screen.repaint();
        let input = readline!({ break; });
        if input.is_empty() {
            continue;
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

            match &*command {
                "ban" | "unban" => {
                    usage!(1, "ban/unban <user>");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let id = match find_user(&session.state.users, &args[0]) {
                        Some(user) => user.id,
                        None => {
                            println!("No such user");
                            continue;
                        }
                    };

                    let packet = Packet::UserUpdate(common::UserUpdate {
                        admin: None,
                        ban: Some(command == "ban"),
                        channel_mode: None,
                        id: id
                    });
                    write!(session, packet, {})
                },
                "connect" => {
                    usage!(1, "connect <ip[:port]>");
                    let mut session = session.lock().unwrap();
                    if session.is_some() {
                        println!("You have to disconnect before doing that.");
                        continue;
                    }

                    let addr = match parse_addr(&args[0]) {
                        Some(some) => some,
                        None => {
                            println!("Could not parse IP");
                            continue;
                        }
                    };
                    *session = connect::connect(addr, &connector, &screen);
                },
                "create" => {
                    usage_min!(2, "create <\"channel\"/\"group\"> <name> [data]");
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
                                default_mode_bot: 0,
                                default_mode_user: common::PERM_READ | common::PERM_WRITE,
                                name: name,
                                recipient: None
                            })
                        },
                        _ => { println!("Unable to create that"); continue; }
                    };
                    write!(session, packet, {})
                },
                "delete" => {
                    usage!(2, "delete <\"channel\"/\"message\"> <id>");

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
                        "channel" => if session.state.channels.contains_key(&id) {
                            Some(Packet::ChannelDelete(common::ChannelDelete {
                                id: id
                            }))
                        } else { None },
                        "message" =>
                            Some(Packet::MessageDelete(common::MessageDelete {
                                id: id
                            })),
                        _ => {
                            println!("Unable to delete that");
                            continue;
                        }
                    };
                    if let Some(packet) = packet {
                        write!(session, packet, {})
                    } else {
                        println!("Nothing with that ID exists");
                    }
                },
                "disconnect" => {
                    usage!(0, "disconnect");
                    let mut session = session.lock().unwrap();
                    {
                        let session = require_session!(session);
                        let _ = session.inner.send(&Packet::Close);
                    }
                    *session = None;
                },
                "forget" => {
                    usage!(1, "forget <ip>");
                    let addr = match parse_addr(&args[0]) {
                        Some(some) => some,
                        None => {
                            println!("Invalid IP");
                            continue;
                        }
                    };
                    db.lock().unwrap().execute("DELETE FROM servers WHERE ip = ?", &[&addr.to_string()]).unwrap();
                },
                "help" => {
                    let borrowed: Vec<_> = args.iter().map(|arg| &**arg).collect();
                    help::help(&*borrowed, &screen);
                },
                "info" => {
                    usage!(1, "info <channel/user>");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let mut name = &*args[0];
                    if name.starts_with('#') {
                        name = &name[1..];
                    }

                    let id = name.parse();

                    println!();
                    for channel in session.state.channels.values() {
                        if name == channel.name
                            || (name.starts_with('#') && name[1..] == channel.name)
                            || Ok(channel.id) == id {
                            println!("Channel #{}", channel.name);
                            println!("ID: #{}", channel.id);
                            println!("Default mode for bots: {}", to_perm_string(channel.default_mode_bot));
                            println!("Default mode for users: {}", to_perm_string(channel.default_mode_user));
                        }
                    }
                    for user in session.state.users.values() {
                        if name == user.name || Ok(user.id) == id {
                            println!("User {}", user.name);
                            if user.ban {
                                println!("Banned.");
                            }
                            println!("Bot: {}", if user.bot { "true" } else { "false" });
                            println!("ID: #{}", user.id);
                            for (&channel, &mode) in &user.modes {
                                if let Some(channel) = session.state.channels.get(&channel) {
                                    println!("Channel #{} mode: {}", channel.name, to_perm_string(mode));
                                }
                            }
                        }
                    }
                },
                "join" => {
                    usage!(1, "join <channel>");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    let mut name = &*args[0];
                    if name.starts_with('#') {
                        name = &name[1..];
                    }

                    let mut packet = None;
                    for channel in session.state.channels.values() {
                        if channel.name == name {
                            session.channel = Some(channel.id);
                            screen.clear();
                            println!("Joined channel #{}", channel.name);
                            packet = Some(Packet::MessageList(common::MessageList {
                                after: None,
                                before: None,
                                channel: channel.id,
                                limit: common::LIMIT_BULK
                            }));
                            break;
                        }
                    }
                    if let Some(packet) = packet {
                        write!(session, packet, {});
                    } else {
                        println!("No channel found with that name");
                    }
                },
                "list" => {
                    usage!(1, "list <\"channels\"/\"groups\"/\"users\">");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);
                    match &*args[0] {
                        "channels" => {
                            // Yeah, cloning this is bad... But what can I do? I need to sort!
                            // Though, I suspect this might be a shallow copy.
                            let mut channels: Vec<_> = session.state.channels.values().collect();
                            channels.sort_by_key(|item| &item.name);

                            let result = channels.iter().fold(String::new(), |mut acc, channel| {
                                if !acc.is_empty() { acc.push_str(", "); }
                                acc.push('#');
                                acc.push_str(&channel.name);
                                acc
                            });
                            println!(result);
                        },
                        "users" => {
                            // Read the above comment, thank you ---------------------------^
                            let mut users: Vec<_> = session.state.users.values().collect();
                            users.sort_by_key(|item| &item.name);

                            for banned in &[false, true] {
                                if *banned {
                                    println!("Banned:");
                                }
                                println!(users.iter().fold(String::new(), |mut acc, user| {
                                    if user.ban == *banned {
                                        if !acc.is_empty() { acc.push_str(", "); }
                                        acc.push_str(&user.name);
                                    }
                                    acc
                                }));
                            }
                        },
                        _ => println!("Unable to list that")
                    }
                },
                "msg" => {
                    usage!(1, "msg <user>");
                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);

                    // lifetime issues
                    let (channel, user) = {
                        let user = match find_user(&session.state.users, &args[0]) {
                            Some(user) => user,
                            None => {
                                println!("No such user");
                                continue;
                            }
                        };

                        (session.state.get_private_channel(&user).map(|channel| channel.id), user.id)
                    };

                    if channel.is_none() {
                        let packet = Packet::ChannelCreate(common::ChannelCreate {
                            default_mode_bot: 0,
                            default_mode_user: 0,
                            name: String::new(),
                            recipient: Some(user)
                        });
                        write!(session, packet, { continue; });

                        println!("Creating channel with <user>...");
                        println!("Type command again to switch to it.");
                        continue;
                    }

                    let channel = channel.unwrap();

                    session.channel = Some(channel);
                    screen.clear();
                    println!("Joined private channel");
                    let packet = Packet::MessageList(common::MessageList {
                        after: None,
                        before: None,
                        channel: channel,
                        limit: common::LIMIT_BULK
                    });
                    write!(session, packet, {});
                },
                "nick" => {
                    usage!(1, "nick <name>");
                    let new = args.remove(0);

                    db.lock().unwrap().execute(
                        "REPLACE INTO data (key, value) VALUES ('nick', ?)",
                        &[&new]
                    ).unwrap();

                    let mut session = session.lock().unwrap();
                    if let Some(ref mut session) = *session {
                        let packet = Packet::LoginUpdate(common::LoginUpdate {
                            name: Some(new.clone()),
                            password_current: None,
                            password_new: None,
                            reset_token: false
                        });
                        write!(session, packet, {});
                    }

                    println!("Your name is now {}", new);
                    *connector.nick.write().unwrap() = new;
                },
                "passwd" => {
                    usage!(0, "passwd");
                    {
                        let mut session = session.lock().unwrap();
                        let session = require_session!(session);

                        println!("Current password: ");
                        let current = readpass!({ continue; });
                        println!("New password: ");
                        let new = readpass!({ continue; });

                        let packet = Packet::LoginUpdate(common::LoginUpdate {
                            name: None,
                            password_current: Some(current),
                            password_new: Some(new),
                            reset_token: true // Doesn't actually matter in this case
                        });
                        write!(session, packet, { continue; })
                    }
                    let _ = rx_sent.recv_timeout(Duration::from_secs(10));
                },
                "quit" => break,
                "setupkeys" => {
                    usage!(1, "setupkeys <user>");

                    let mut session = session.lock().unwrap();
                    let session = require_session!(session);

                    let id = match find_user(&session.state.users, &args[0]) {
                        Some(user) => user.id,
                        None => {
                            println!("No such user");
                            continue;
                        }
                    };

                    println!("In order to secure your connection, we need two things:");
                    use openssl::rsa::Rsa;
                    let rsa = match Rsa::generate(common::RSA_LENGTH) {
                        Ok(ok) => ok,
                        Err(err) => {
                            println!("Error! Failed to generate a key pair.");
                            println!("Details: {}", err);
                            continue;
                        }
                    };
                    let private = rsa.private_key_to_pem().unwrap();
                    let public  =  rsa.public_key_to_pem().unwrap();

                    println!("1. You need to give your recipient this \"public key\":");
                    println!(String::from_utf8_lossy(&public));
                    println!("2. Have your recipient run /setupkeys too.");
                    println!("You will be asked to enter his/her public key here.");
                    println!("When you've entered it, type \"END\".");
                    println!("Enter his/her public key here:");

                    let mut key = String::with_capacity(512); // idk, just guessing
                    while let Ok(line) = screen.readline() {
                        if line == "END" { break; }

                        key.push_str(&line);
                        key.push('\n');
                    }

                    if let Err(err) = Rsa::public_key_from_pem(key.as_bytes()) {
                        println!("Error! Did you enter it correctly?");
                        println!("Details: {}", err);
                        continue;
                    }
                    db.lock().unwrap().execute(
                        "REPLACE INTO pms (private, public, recipient) VALUES (?, ?, ?)",
                        &[&private, &key, &(id as i64)]
                    ).unwrap();
                },
                "update" => {
                    usage!(2, "update <\"channel\"/\"user\"> <id>");

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
                        "channel" => if let Some(channel) = session.state.channels.get(&id) {
                            println!("Editing: #{}", channel.name);
                            println!("(Press enter to keep current value)");
                            println!();

                            println!("Name [{}]: ", channel.name);
                            let name = readline!({ continue; });
                            let mut name = name.trim();
                            if name.is_empty() { name = &channel.name }

                            println!("Default mode for bots [{}]: ", to_perm_string(channel.default_mode_bot));
                            let default_mode_bot = readline!({ continue; });
                            let default_mode_bot = if default_mode_bot.trim().is_empty() {
                                channel.default_mode_bot
                            } else {
                                let mut bitmask = channel.default_mode_bot;
                                if !from_perm_string(&default_mode_bot, &mut bitmask) {
                                    println!("Invalid permission string.");
                                    continue;
                                }
                                bitmask
                            };
                            println!("Default mode for users [{}]: ", to_perm_string(channel.default_mode_user));
                            let default_mode_user = readline!({ continue; });
                            let default_mode_user = if default_mode_user.trim().is_empty() {
                                channel.default_mode_user
                            } else {
                                let mut bitmask = channel.default_mode_user;
                                if !from_perm_string(&default_mode_user, &mut bitmask) {
                                    println!("Invalid permission string.");
                                    continue;
                                }
                                bitmask
                            };
                            println!("{:?}", default_mode_user);

                            Some(Packet::ChannelUpdate(common::ChannelUpdate {
                                inner: common::Channel {
                                    default_mode_bot: default_mode_bot,
                                    default_mode_user: default_mode_user,
                                    id: channel.id,
                                    name: name.to_string(),
                                    private: false
                                }
                            }))
                        } else { None },
                        "user" => if let Some(user) = session.state.users.get(&id) {
                            println!("Admin [{}]: ", user.admin);
                            let admin = readline!({ continue; });
                            let admin = admin.trim();
                            let admin = if admin.is_empty() { user.admin }
                                        else if admin == "true" { true }
                                        else if admin == "false" { false }
                                        else {
                                            println!("Invalid boolean");
                                            continue;
                                        };
                            let mut channel_mode = None;
                            let admin = if admin != user.admin {
                                Some(admin)
                            } else {
                                if let Some(channel) = session.channel {
                                    if let Some(channel) = session.state.channels.get(&channel) {
                                        let mut mode = user.modes.get(&channel.id).cloned().unwrap_or_default();
                                        println!("Mode in #{} [{}]: ", channel.name, to_perm_string(mode));
                                        println!("Type r to remove");
                                        let new_mode = readline!({ continue; });
                                        let new_mode = new_mode.trim();
                                        let mode = if new_mode == "r" {
                                            None
                                        } else if !new_mode.trim().is_empty() {
                                            if !from_perm_string(new_mode, &mut mode) {
                                                println!("Invalid permission string");
                                                continue;
                                            }
                                            Some(mode)
                                        } else { Some(mode) };
                                        channel_mode = Some((channel.id, mode));
                                    }
                                }
                                None
                            };

                            Some(Packet::UserUpdate(common::UserUpdate {
                                admin: admin,
                                ban: None,
                                channel_mode: channel_mode,
                                id: id
                            }))
                        } else { None },
                        _ => {
                            println!("Unable to delete that");
                            continue;
                        }
                    };
                    if let Some(packet) = packet {
                        write!(session, packet, {})
                    } else {
                        println!("Nothing with that ID exists");
                    }
                },
                _ => {
                    println!("Unknown command");
                }
            }
            continue;
        } else if input.starts_with('!') {
            let mut session = session.lock().unwrap();
            let session = require_session!(session);
            let mut args = parser::parse(&input[1..]);
            if args.len() < 2 {
                continue;
            }
            let name = args.remove(0);

            let recipient = match find_user(&session.state.users, &name) {
                Some(user) if user.bot => user.id,
                Some(_) => { println!("That's not a bot!"); continue; },
                None => { println!("No such user"); continue; }
            };

            let packet = Packet::Command(common::Command {
                args: args,
                recipient: recipient
            });

            write!(session, packet, {});

            continue;
        }

        let mut session = session.lock().unwrap();
        let session = require_session!(session);

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
                screen.log_with_id(format!("{}: {}", connector.nick.read().unwrap(), input), LogEntryId::Sending);
                Packet::MessageCreate(common::MessageCreate {
                    channel: channel,
                    text: input.into_bytes()
                })
            };

            write!(session, packet, {})
        } else {
            println!("No channel specified. See /create channel, /list channels and /join");
            continue;
        }
    }

    tx_stop.send(()).unwrap();
    if let Some(ref mut session) = *session.lock().unwrap() {
        let _ = session.inner.send(&Packet::Close);
    }
    thread.join().unwrap();
}

fn find_user<'a>(users: &'a HashMap<usize, common::User>, name: &str) -> Option<&'a common::User> {
    users.values().find(|user| user.name == name)
}
fn to_perm_string(bitmask: u8) -> String {
    let mut result = String::with_capacity(5);

    if bitmask != 0 {
        result.push('+');
    }

    if bitmask & common::PERM_READ == common::PERM_READ {
        result.push('r');
    }
    if bitmask & common::PERM_WRITE == common::PERM_WRITE {
        result.push('w');
    }
    if bitmask & common::PERM_MANAGE_CHANNELS == common::PERM_MANAGE_CHANNELS {
        result.push('c');
    }
    if bitmask & common::PERM_MANAGE_MESSAGES == common::PERM_MANAGE_MESSAGES {
        result.push('m');
    }

    result.shrink_to_fit();
    result
}
fn from_perm_string(input: &str, bitmask: &mut u8) -> bool {
    let mut add = true;

    for c in input.chars() {
        let perm = match c {
            '+' => { add = true; continue; },
            '-' => { add = false; continue; },
            'r' => common::PERM_READ,
            'w' => common::PERM_WRITE,

            'c' => common::PERM_MANAGE_CHANNELS,
            'm' => common::PERM_MANAGE_MESSAGES,
            ' ' => continue,
            _   => return false
        };
        if add {
            *bitmask |= perm;
        } else {
            *bitmask &= !perm;
        }
    }

    true
}

fn parse_addr(input: &str) -> Option<SocketAddr> {
    let mut parts = input.rsplitn(2, ':');
    let addr = match (parts.next()?, parts.next()) {
        (port, Some(ip)) => (ip, port.parse().ok()?),
        (ip,   None)     => (ip, common::DEFAULT_PORT)
    };

    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
}

fn get_typing_string(people: &[&str]) -> String {
    match people.len() {
        n if n > 500 => String::from("(╯°□°）╯︵ ┻━┻"),
        n if n > 100 => String::from("A crap ton of people are typing"),
        n if n > 50 => String::from("Over 50 people are typing"),
        n if n > 10 => String::from("Over 10 people are typing"),
        n if n > 3 => String::from("Several people are typing"),
        3 => format!("{}, {} and {} are typing", people[0], people[1], people[2]),
        2 => format!("{} and {} are typing", people[0], people[1]),
        1 => format!("{} is typing", people[0]),
        _ => String::new()
    }
}
