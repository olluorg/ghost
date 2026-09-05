//! Публичная ссылка без белого IP.
//!
//! Помощнику нужно как-то достучаться до машины, у которой нет внешнего
//! адреса. Поддерживаются два провайдера, потому что один-единственный
//! оказался ненадёжен: на этой сети cloudflared не смог зарегистрировать
//! туннель (ошибка 1033, QUIC отваливался по таймауту), а SSH-туннель поднялся
//! с первой попытки. Оба не требуют аккаунта.
//!
//! Побочно решается и шифрование: наружу трафик идёт по HTTPS, а до нашего
//! сервера — по localhost.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Debug)]
pub enum Tunnel {
    Off,
    Starting,
    Ready(String),
    Failed(String),
}

/// Обычное место установки cloudflared из winget — туда он кладётся без
/// прописи в PATH текущего сеанса.
const CLOUDFLARED: &str = r"C:\Program Files (x86)\cloudflared\cloudflared.exe";
const SSH: &str = r"C:\Windows\System32\OpenSSH\ssh.exe";

pub struct Handle {
    child: Arc<Mutex<Option<Child>>>,
    state: Arc<Mutex<Tunnel>>,
}

impl Handle {
    pub fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(Tunnel::Off)),
        }
    }

    pub fn state(&self) -> Tunnel {
        self.state.lock().unwrap().clone()
    }

    /// Поднимает туннель в фоне: адрес выдаётся через несколько секунд, и
    /// ждать этого в потоке интерфейса нельзя.
    pub fn start(&self, kind: &str, exe: String, port: u16) {
        if kind == "off" {
            return;
        }
        if matches!(*self.state.lock().unwrap(), Tunnel::Starting | Tunnel::Ready(_)) {
            return;
        }
        *self.state.lock().unwrap() = Tunnel::Starting;

        let state = Arc::clone(&self.state);
        let slot = Arc::clone(&self.child);
        let kind = kind.to_string();

        thread::spawn(move || {
            let mut command = build(&kind, &exe, port);
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            hide_console(&mut command);

            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(e) => {
                    *state.lock().unwrap() = Tunnel::Failed(hint(&kind, &e.to_string()));
                    return;
                }
            };

            // Адрес печатают то в stdout, то в stderr, поэтому читаем оба.
            let out = child.stdout.take();
            let err = child.stderr.take();
            *slot.lock().unwrap() = Some(child);

            let mut readers = Vec::new();
            for stream in [
                out.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                err.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ]
            .into_iter()
            .flatten()
            {
                let state = Arc::clone(&state);
                let kind = kind.clone();
                readers.push(thread::spawn(move || {
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        if let Some(url) = extract_url(&kind, &line) {
                            eprintln!("[ghost] туннель поднят: {url}");
                            *state.lock().unwrap() = Tunnel::Ready(url);
                        }
                    }
                }));
            }
            for reader in readers {
                let _ = reader.join();
            }

            // Оба потока вывода закончились — значит процесс завершился.
            let mut guard = state.lock().unwrap();
            if !matches!(*guard, Tunnel::Off) {
                *guard = Tunnel::Failed("туннель закрылся".into());
            }
        });
    }

    pub fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *self.state.lock().unwrap() = Tunnel::Off;
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build(kind: &str, exe: &str, port: u16) -> Command {
    match kind {
        "cloudflared" => {
            let mut c = Command::new(resolve(exe, CLOUDFLARED, "cloudflared"));
            c.args([
                "tunnel",
                "--url",
                &format!("http://127.0.0.1:{port}"),
                "--no-autoupdate",
            ]);
            c
        }
        // localhost.run: аккаунт не нужен, транспорт — обычный SSH, который
        // проходит там, где QUIC у cloudflared отваливается.
        _ => {
            let mut c = Command::new(resolve(exe, SSH, "ssh"));
            c.args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=NUL",
                // Иначе провайдер или NAT молча закроют простаивающее соединение.
                "-o",
                "ServerAliveInterval=30",
                "-R",
                &format!("80:127.0.0.1:{port}"),
                "nokey@localhost.run",
            ]);
            c
        }
    }
}

fn resolve(configured: &str, fallback: &str, in_path: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    if std::path::Path::new(fallback).exists() {
        return fallback.to_string();
    }
    in_path.to_string()
}

fn hint(kind: &str, error: &str) -> String {
    match kind {
        "cloudflared" => {
            format!("cloudflared не запустился ({error}); поставьте: winget install Cloudflare.cloudflared")
        }
        _ => format!("ssh не запустился ({error}); нужен клиент OpenSSH из состава Windows"),
    }
}

fn extract_url(kind: &str, line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|' || c == '"')
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches('/');

    let ours = match kind {
        "cloudflared" => url.contains("trycloudflare.com"),
        // Панель управления сессией на том же домене — она нам не нужна.
        _ => (url.contains(".lhr.life") || url.contains(".localhost.run"))
            && !url.contains("//admin."),
    };
    ours.then(|| url.to_string())
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: иначе поверх всего выскочит консоль туннеля.
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn адрес_cloudflare_находится_в_рамке() {
        let line = "2026-09-03 INF |  https://mild-cat-fox.trycloudflare.com   |";
        assert_eq!(
            extract_url("cloudflared", line).as_deref(),
            Some("https://mild-cat-fox.trycloudflare.com")
        );
    }

    #[test]
    fn чужие_адреса_не_принимаются() {
        // В выводе полно ссылок на документацию и условия использования.
        let doc = "INF see https://developers.cloudflare.com/cloudflare-one/";
        assert!(extract_url("cloudflared", doc).is_none());
        assert!(extract_url("ssh", doc).is_none());
    }

    #[test]
    fn адрес_ssh_туннеля_находится() {
        let line = "https://f7bbb0c57b1241.lhr.life tunneled with tls termination";
        assert_eq!(
            extract_url("ssh", line).as_deref(),
            Some("https://f7bbb0c57b1241.lhr.life")
        );
    }

    #[test]
    fn панель_управления_не_путается_с_туннелем() {
        // localhost.run печатает и ссылку на свою админку — она нам не нужна.
        let line = "**You need to accept the new terms: https://admin.localhost.run **";
        assert!(extract_url("ssh", line).is_none());
    }

    #[test]
    fn завершающий_слеш_срезается() {
        let line = "https://abc.lhr.life/";
        assert_eq!(extract_url("ssh", line).as_deref(), Some("https://abc.lhr.life"));
    }
}
