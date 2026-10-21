use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Instant;

use exchange_protocol::ServerMessage;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui::app::{App, Effect, Key, Msg, RETRY, TICK};
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

    let (sender, inbox) = mpsc::channel();
    let (writer, writers) = mpsc::channel();
    spawn_connection(args.server, stream, sender.clone(), writer);
    spawn_keyboard(sender);

    let app = App::new(args.server.to_string(), args.account);
    let mut terminal = ratatui::init();
    let outcome = run(&mut terminal, app, &inbox, &writers);
    ratatui::restore();
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// A conexão inteira numa thread: lê as linhas, e quando a conexão cai, tenta outra a cada
/// [`RETRY`] até conseguir. Cada conexão nova manda primeiro a ponta de escrita para o laço
/// (`writers`) e depois o [`Msg::Connected`], então o laço já tem por onde escrever quando
/// o `update` pede o login.
fn spawn_connection(
    server: SocketAddr,
    first: TcpStream,
    inbox: Sender<Msg>,
    writers: Sender<TcpStream>,
) {
    thread::spawn(move || {
        let mut attempt: io::Result<TcpStream> = Ok(first);
        loop {
            let reason = match attempt.and_then(|reader| Ok((reader.try_clone()?, reader))) {
                Ok((writer, reader)) => {
                    if writers.send(writer).is_err() || inbox.send(Msg::Connected).is_err() {
                        return;
                    }
                    let Some(reason) = read_lines(reader, &inbox) else {
                        return;
                    };
                    reason
                }
                Err(error) => error.to_string(),
            };
            if inbox.send(Msg::Disconnected(reason)).is_err() {
                return;
            }
            thread::sleep(RETRY);
            attempt = TcpStream::connect_timeout(&server, RETRY);
        }
    });
}

/// Uma linha do servidor por mensagem, até a conexão acabar; devolve o motivo, ou `None` se
/// a tela já fechou e não há para quem contar. Linha que este cliente não entende (de um
/// servidor mais novo) é ignorada.
fn read_lines(stream: TcpStream, inbox: &Sender<Msg>) -> Option<String> {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next() {
            None => return Some("o servidor fechou a conexão".to_string()),
            Some(Err(error)) => return Some(error.to_string()),
            Some(Ok(line)) => {
                let Ok(message) = ServerMessage::from_line(&line) else {
                    continue;
                };
                // A hora de chegada, no fuso da máquina; sem fuso conhecido, UTC.
                let at = jiff::Zoned::now().strftime("%H:%M:%S").to_string();
                inbox.send(Msg::Server { message, at }).ok()?;
            }
        }
    }
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

/// O laço: desenha, espera uma mensagem (ou o próximo tick), junta as que chegaram juntas,
/// atualiza o estado e executa os efeitos. Uma rajada do feed vira um redesenho só, e o
/// tick sai no horário mesmo com o feed sem parar.
fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    inbox: &Receiver<Msg>,
    writers: &Receiver<TcpStream>,
) -> io::Result<()> {
    let mut server: Option<TcpStream> = None;
    let mut effects = Vec::new();
    let mut next_tick = Instant::now() + TICK;
    loop {
        if let Some(latest) = writers.try_iter().last() {
            server = Some(latest);
        }
        for effect in std::mem::take(&mut effects) {
            match effect {
                Effect::Quit => return Ok(()),
                // Uma escrita que falha quer dizer conexão morta: a thread de leitura percebe
                // e avisa, e até lá o que for mandado se perde, como se perderia no fio.
                Effect::Send(message) => {
                    if let Some(stream) = &mut server
                        && stream.write_all(message.to_line().as_bytes()).is_err()
                    {
                        server = None;
                    }
                }
            }
        }
        terminal.draw(|frame| ui::draw(frame, &app))?;
        match inbox.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(first) => {
                effects.extend(app.update(first));
                effects.extend(inbox.try_iter().flat_map(|msg| app.update(msg)));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if Instant::now() >= next_tick {
            effects.extend(app.update(Msg::Tick));
            next_tick = Instant::now() + TICK;
        }
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
