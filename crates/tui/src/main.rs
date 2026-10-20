use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use exchange_protocol::ServerMessage;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui::app::{App, Effect, Key, Msg};
use tui::ui;

const USAGE: &str = "uso: tui --account <conta> [--server <endereço:porta>]";
const DEFAULT_SERVER: &str = "127.0.0.1:7000";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    account: u64,
    server: SocketAddr,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Args, &'static str> {
    let mut account = None;
    let mut server = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        let value = value.to_str().ok_or(USAGE)?;
        let was_empty = if flag == "--account" {
            account.replace(value.parse().map_err(|_| USAGE)?).is_none()
        } else if flag == "--server" {
            server.replace(value.parse().map_err(|_| USAGE)?).is_none()
        } else {
            return Err(USAGE);
        };
        if !was_empty {
            return Err(USAGE);
        }
    }
    let server = match server {
        Some(address) => address,
        None => DEFAULT_SERVER.parse().map_err(|_| USAGE)?,
    };
    Ok(Args {
        account: account.ok_or(USAGE)?,
        server,
    })
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    };
    // Conecta antes de tomar o terminal: se o servidor não estiver no ar, o erro aparece
    // no shell, legível, em vez de numa tela que some.
    let stream = match TcpStream::connect(args.server) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("não conectou em {}: {error}", args.server);
            return ExitCode::FAILURE;
        }
    };
    let reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let (sender, inbox) = mpsc::channel();
    spawn_server_reader(reader, sender.clone());
    spawn_keyboard(sender);

    let app = App::new(args.server.to_string(), args.account);
    let mut terminal = ratatui::init();
    let outcome = run(&mut terminal, app, &inbox, stream);
    ratatui::restore();
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Uma linha do servidor por mensagem. Linha que este cliente não entende (de um servidor
/// mais novo) é ignorada; o fim da conexão vira uma mensagem para a tela.
fn spawn_server_reader(stream: TcpStream, inbox: Sender<Msg>) {
    thread::spawn(move || {
        let mut lines = BufReader::new(stream).lines();
        let reason = loop {
            match lines.next() {
                None => break "o servidor fechou a conexão".to_string(),
                Some(Err(error)) => break error.to_string(),
                Some(Ok(line)) => {
                    let Ok(message) = ServerMessage::from_line(&line) else {
                        continue;
                    };
                    if inbox.send(Msg::Server(message)).is_err() {
                        return;
                    }
                }
            }
        };
        // Se a tela já fechou, não há para quem contar.
        let _ = inbox.send(Msg::Disconnected(reason));
    });
}

fn spawn_keyboard(inbox: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let msg = match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    let Some(key) = translate(key) else {
                        continue;
                    };
                    Msg::Key(key)
                }
                Ok(Event::Resize(..)) => Msg::Resize,
                Ok(_) => continue,
                Err(_) => return,
            };
            if inbox.send(msg).is_err() {
                return;
            }
        }
    });
}

fn translate(key: KeyEvent) -> Option<Key> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return (key.code == KeyCode::Char('c')).then_some(Key::Interrupt);
    }
    Some(match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        _ => return None,
    })
}

/// O laço: desenha, espera uma mensagem, junta as que chegaram juntas, atualiza o estado e
/// executa os efeitos. Uma rajada do feed vira um redesenho só.
fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    inbox: &Receiver<Msg>,
    mut server: TcpStream,
) -> io::Result<()> {
    let mut effects = app.start();
    loop {
        for effect in std::mem::take(&mut effects) {
            match effect {
                Effect::Quit => return Ok(()),
                Effect::Send(message) => {
                    if let Err(error) = server.write_all(message.to_line().as_bytes()) {
                        effects.extend(app.update(Msg::Disconnected(error.to_string())));
                    }
                }
            }
        }
        terminal.draw(|frame| ui::draw(frame, &app))?;
        let Ok(first) = inbox.recv() else {
            return Ok(());
        };
        effects.extend(app.update(first));
        effects.extend(inbox.try_iter().flat_map(|msg| app.update(msg)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, &'static str> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn account_is_required_and_server_has_a_default() {
        assert_eq!(
            parse(&["--account", "1"]),
            Ok(Args {
                account: 1,
                server: DEFAULT_SERVER.parse().unwrap()
            })
        );
        assert_eq!(parse(&[]), Err(USAGE));
        assert_eq!(parse(&["--server", "127.0.0.1:9000"]), Err(USAGE));
    }

    #[test]
    fn server_can_be_chosen() {
        assert_eq!(
            parse(&["--server", "127.0.0.1:9000", "--account", "2"]),
            Ok(Args {
                account: 2,
                server: "127.0.0.1:9000".parse().unwrap()
            })
        );
    }

    #[test]
    fn bad_or_repeated_flags_are_refused() {
        assert_eq!(parse(&["--account", "um"]), Err(USAGE));
        assert_eq!(parse(&["--account", "-1"]), Err(USAGE));
        assert_eq!(parse(&["--account", "1", "--account", "2"]), Err(USAGE));
        assert_eq!(parse(&["--account"]), Err(USAGE));
        assert_eq!(parse(&["--conta", "1"]), Err(USAGE));
    }

    #[test]
    fn control_c_interrupts_and_other_control_keys_are_dropped() {
        let control = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);

        assert_eq!(translate(control('c')), Some(Key::Interrupt));
        assert_eq!(translate(control('h')), None);
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            Some(Key::Char('j'))
        );
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Some(Key::Char('?'))
        );
    }
}
