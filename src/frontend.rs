extern crate termion;

use *;
use rustyline::completion::{extract_word, Completer as RustyCompleter};
use rustyline::error::ReadlineError;
use rustyline;
use self::termion::screen::AlternateScreen;
use self::termion::{cursor, color};
use std::cmp;
use std::io::{self, Write};
use std::sync::{Mutex, RwLock};

pub fn sanitize(text: &mut String) {
    // text.retain(|c| {
    //     !c.is_control() || c == '\n' || c == '\t'
    // });
    // YES, that's right. Until that's stabilized, I'd rather clone the entire string
    // than using difficult workarounds.
    // TODO: Use `retain` to avoid allocation.
    *text = text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
}

struct Completer {
    session: Arc<Mutex<Option<Session>>>
}

impl RustyCompleter for Completer {
    fn complete(&self, line: &str, pos: usize) -> Result<(usize, Vec<String>), ReadlineError> {
        let break_chars = &[' ', '"', '\''].into_iter().cloned().collect();
        let (start, word) = extract_word(line, pos, break_chars);

        let mut output = Vec::new();

        if let Some(ref session) = *self.session.lock().unwrap() {
            output.extend(session.state.users.values()
                .map(|user| user.name.clone())
                .filter(|name| name.starts_with(word)));
            output.extend(session.state.channels.values()
                .map(|channel| {
                    let mut name = String::with_capacity(channel.name.len() + 1);
                    name.push('#');
                    name.push_str(&channel.name);
                    name
                })
                .filter(|name| name.starts_with(word)));
        }

        Ok((start, output))
    }
}

pub struct Screen {
    editor: Mutex<rustyline::Editor<Completer>>,
    log:    RwLock<Vec<(String, LogEntryId)>>,
    status: Mutex<Vec<String>>,
    stdin:  Mutex<io::Stdin>,
    stdout: Mutex<AlternateScreen<io::Stdout>>,
    typing: RwLock<String>
}
impl Screen {
    pub fn new(session: &Arc<Mutex<Option<Session>>>) -> Screen {
        let stdin = io::stdin();
        stdin.lock();
        let stdout = io::stdout();
        stdout.lock();

        let mut editor = rustyline::Editor::<Completer>::new();
        editor.set_completer(Some(Completer {
            session: Arc::clone(session),
        }));

        Screen {
            editor: Mutex::new(editor),
            log:    RwLock::new(Vec::new()),
            status: Mutex::new(Vec::new()),
            stdin:  Mutex::new(stdin),
            stdout: Mutex::new(AlternateScreen::from(stdout)),
            typing: RwLock::new(String::new())
        }
    }

    pub fn clear(&self) {
        self.log.write().unwrap().clear();
        self.repaint();
    }

    pub fn log(&self, text: String) {
        self.log_with_id(text, LogEntryId::None);
    }
    pub fn log_with_id(&self, mut text: String, id: LogEntryId) {
        if let LogEntryId::Message(_) = id {
            if let Some(text2) = self.edit(text, id) {
                text = text2;
            } else {
                self.repaint();
                return;
            }
        }
        self.log.write().unwrap().push((text, id));
        self.repaint();
    }
    pub fn edit(&self, text: String, id: LogEntryId) -> Option<String> {
        let mut log = self.log.write().unwrap();
        for &mut (ref mut entry_text, entry_id) in &mut *log {
            if entry_id == id {
                *entry_text = text;
                return None;
            }
        }
        Some(text)
    }
    pub fn delete(&self, id: LogEntryId) {
        let mut log = self.log.write().unwrap();
        let pos = log.iter().position(|&(_, entry_id)| entry_id == id);
        if let Some(pos) = pos {
            log.remove(pos);
        }
        // TODO: remove_item once stable
    }

    pub fn update(&self, session: &Session) {
        let mut status = self.status.lock().unwrap();
        status.clear();

        status.push(String::new());
        status.push(String::from("Channels:"));
        for channel in session.state.channels.values() {
            let mut string = String::with_capacity(2 + channel.name.len());
            if channel.private {
                string.push_str("* ");
                if let Some(recipient) = session.state.get_recipient_unchecked(channel.id) {
                    string.push_str(&recipient.name);
                } else {
                    string.push_str("unknown");
                }
            } else {
                string.push_str(" #");
                string.push_str(&channel.name);
            }
            status.push(string);
        }
        status[2..].sort_unstable();
    }
    pub fn repaint(&self) {
        self.repaint_(&**self.log.read().unwrap());
    }
    fn repaint_(&self, mut log: &[(String, LogEntryId)]) {
        let mut stdout = self.stdout.lock().unwrap();
        let (width, height) = termion::terminal_size().unwrap_or((50, 50));
        let cw =  width.saturating_sub(11 + 2);
        let ch = height.saturating_sub(3);

        let status = self.status.lock().unwrap();

        let mut index = 0;
        let mut status = |stdout: &mut AlternateScreen<io::Stdout>, left| {
            if let Some(text) = status.get(index) {
                let mut text = text as &str;
                let ellipsis = if text.len() > 11 {
                    text = &text[..10];
                    "…"
                } else { "" };
                index += 1;
                writeln!(stdout, "{}{}{}", cursor::Right(cw + 2 - left), text, ellipsis).unwrap();
            } else {
                writeln!(stdout).unwrap();
            }
        };

        loop {
            let mut lines = 0;
            for msg in log {
                lines += 1;

                let indent_amount = msg.0.find(": ").map(|i| i+2).unwrap_or_default();

                let mut i = 0;
                for c in msg.0.chars() {
                    if (i != 0 && i == cw as usize) || c == '\n' {
                        i = indent_amount;
                        lines += 1;
                    } else {
                        i += 1;
                    }
                }
            }

            if lines <= ch {
                break;
            }
            log = &log[1..];
        }

        write!(stdout, "{}{}", termion::clear::All, cursor::Goto(1, 1)).unwrap();

        for &(ref text, id) in log {
            let indent_amount = text.find(": ").map(|i| i+2).unwrap_or_default();

            let mut first  = true;
            let mut indent = String::new();
            let mut skip   = 0;
            let mut text   = &**text;

            if text.is_empty() {
                status(&mut stdout, 0);
            }
            while !text.is_empty() {
                if !first && indent.is_empty() {
                    indent = " ".repeat(indent_amount);
                }
                let indent_amount = if first { 0 } else { indent_amount };
                first = false;

                text = &text[skip..];
                skip = 0;

                let newline = text.find('\n').unwrap_or(std::usize::MAX);
                let width = cmp::min(newline, cmp::min((cw as usize).saturating_sub(indent_amount), text.len()));
                if let LogEntryId::Sending = id {
                    write!(
                        stdout,
                        "{}{}{}{}",
                        indent,
                        color::Fg(color::LightBlack),
                        &text[..width],
                        color::Fg(color::Reset)
                    ).unwrap();
                } else {
                    write!(stdout, "{}{}", indent, &text[..width]).unwrap();
                }
                status(&mut stdout, (indent_amount + width) as u16);
                text = &text[width..];
                if width == newline {
                    skip += 1;
                }
            }
        }

        write!(stdout, "{}{}", cursor::Goto(1, height), self.typing.read().unwrap()).unwrap();
        write!(stdout, "{}> ", cursor::Goto(1, height-1)).unwrap();
        stdout.flush().unwrap();
    }

    pub fn readline(&self) -> Result<String, ()> {
        let mut error = None;
        let ret = {
            let mut editor = self.editor.lock().unwrap();
            match editor.readline("> ") {
                Ok(some) => { editor.add_history_entry(&some); Ok(some) },
                Err(ReadlineError::Interrupted) |
                Err(ReadlineError::Eof) => Err(()),
                Err(err) => {
                    error = Some(err);
                    Err(())
                }
            }
        };
        if let Some(err) = error {
            self.log(format!("Failed to read line: {}", err));
        }
        ret
    }
    pub fn readpass(&self) -> Result<String, ()> {
        use self::termion::input::TermRead;
        let mut error = None;
        let ret = {
            match self.stdin.lock().unwrap().read_passwd(&mut *self.stdout.lock().unwrap()) {
                Ok(Some(some)) => Ok(some),
                Ok(None) => Err(()),
                Err(err) => {
                    error = Some(err);
                    Err(())
                }
            }
        };
        if let Some(err) = error {
            self.log(format!("Failed to read password: {}", err));
        }
        ret
    }

    pub fn typing_set(&self, typing: String) {
        if *self.typing.read().unwrap() == typing {
            return;
        }
        *self.typing.write().unwrap() = typing;
        self.repaint();
    }
}
