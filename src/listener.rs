use *;
use synac::Listener;
use synac::common::Packet;
use rusqlite::Connection as SqlConnection;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use frontend;

pub fn listen(
    db: &Arc<Mutex<SqlConnection>>,
    screen: &Arc<frontend::Screen>,
    tx_sent: &mpsc::SyncSender<()>,
    session: &Arc<Mutex<Option<Session>>>,
    rx_stop: &mpsc::Receiver<()>
) {
    macro_rules! println {
        () => { screen.log(String::new()); };
        ($arg:expr) => { screen.log(String::from($arg)); };
        ($($arg:expr),*) => { screen.log(format!($($arg),*)); };
    }

    let mut typing_last = Instant::now();
    let typing_check = Duration::from_secs(1);

    let mut listener = Listener::new();

    loop {
        thread::sleep(Duration::from_millis(1));

        match rx_stop.try_recv() {
            Ok(_) |
            Err(mpsc::TryRecvError::Disconnected) => break,
            _ => {}
        }

        if let Some(ref mut session) = *session.lock().unwrap() {
            if typing_last.elapsed() >= typing_check {
                typing_last = Instant::now();
                let duration = Duration::from_secs(common::TYPING_TIMEOUT as u64); // TODO use const fn
                session.typing.retain(|_, time| time.elapsed() < duration);

                let people: Vec<_> = session.typing.keys()
                    .filter_map(|&(author, channel)| {
                        if Some(channel) != session.channel {
                            return None;
                        }

                        session.state.users.get(&author).map(|user| &*user.name)
                    })
                    .collect();

                screen.typing_set(get_typing_string(&*people));
            }
            let packet = listener.try_read(session.inner.inner_stream());
            if let Err(err) = packet {
                println!("Error receiving packet: {}", err);
                continue;
            }
            if let Some(packet) = packet.unwrap() {
                screen.delete(LogEntryId::Sending);
                session.state.update(&packet);

                match packet {
                    Packet::LoginSuccess(event) => {
                        db.lock().unwrap().execute(
                            "UPDATE servers SET token = ? WHERE ip = ?",
                            &[&event.token, &session.addr.to_string()]
                        ).unwrap();
                    },
                    Packet::MessageDeleteReceive(event) => {
                        screen.delete(LogEntryId::Message(event.id));
                        screen.repaint();
                    },
                    Packet::MessageReceive(msg) => {
                        let msg = msg.inner;
                        session.typing.remove(&(msg.author, msg.channel));

                        if let Some(user) = session.state.users.get(&msg.author) {
                            if session.channel == Some(msg.channel) {
                                let mut string = String::from_utf8_lossy(&msg.text).into_owned();
                                frontend::sanitize(&mut string);

                                screen.log_with_id(
                                    format!(
                                        "{} (ID #{}): {}",
                                        user.name,
                                        msg.id,
                                        string
                                    ),
                                    LogEntryId::Message(msg.id)
                                );
                            }
                            if msg.author == session.id {
                                session.last = Some((msg.id, msg.text));
                            }
                        }
                    },
                    Packet::RateLimited(time) => {
                        println!("Slow down! You may try again in {} seconds.", time);
                    },
                    Packet::TypingReceive(event) => {
                        if event.author != session.id {
                            session.typing.insert((event.author, event.channel), Instant::now());
                        }
                    },
                    Packet::Err(common::ERR_ALREADY_EXISTS) => {
                        println!("Already exists");
                    },
                    Packet::Err(common::ERR_LIMIT_REACHED) => {
                        println!("Too short or too long. No idea which");
                    },
                    Packet::Err(common::ERR_LOGIN_INVALID) => {
                        println!("Invalid credentials");
                    },
                    Packet::Err(common::ERR_MISSING_PERMISSION) => {
                        println!("Missing permission");
                    },
                    Packet::Err(common::ERR_SELF_PM) => {
                        println!("Can't send a PM to yourself");
                    },
                    Packet::Err(common::ERR_UNKNOWN_CHANNEL) => {
                        println!("This channel was deleted");
                    },
                    Packet::Err(err) => {
                        println!("Unimplemented error: {:?}", err);
                    },
                    _ => {}
                }
                screen.update(session);
                let _ = tx_sent.try_send(());
            }
        }
    }
}
