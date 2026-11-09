//! SSH session over russh, running on a tokio runtime.
//!
//! The UI thread talks to it through two channels: [`Cmd`] in (keystrokes, resizes)
//! and [`Event`] out (server output, notices, questions for the user).

use std::sync::Arc;

use russh::client::{self, AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::{self, HashAlg, PublicKeyOrCertificate};
use russh::{cipher, kex, mac, ChannelMsg, Disconnect, MethodKind, Preferred};
use tokio::sync::{mpsc, oneshot};

use crate::args::Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
}

pub enum Cmd {
    Input(Vec<u8>),
    Resize(Size),
}

pub enum Event {
    Output(Vec<u8>),
    /// A message from termhop itself, shown dimmed in the terminal.
    Notice(String),
    /// Ask the user for a line of input. `None` means they cancelled.
    Prompt { text: String, echo: bool, reply: oneshot::Sender<Option<String>> },
    Closed(String),
}

type Events = mpsc::UnboundedSender<Event>;

/// `term` is the terminal type announced to the server (`xterm` by default, as PuTTY does).
pub fn spawn(
    rt: &tokio::runtime::Handle,
    target: Target,
    term: String,
    events: Events,
    cmds: mpsc::UnboundedReceiver<Cmd>,
) {
    rt.spawn(async move {
        let reason = session(target, &term, &events, cmds).await.unwrap_or_else(|e| e);
        let _ = events.send(Event::Closed(reason));
    });
}

async fn session(
    target: Target,
    term: &str,
    events: &Events,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) -> Result<String, String> {
    let Target { host, port, user, password } = target;
    notice(events, format!("{host}:{port} 에 연결하는 중..."));

    let config = Arc::new(client::Config {
        preferred: preferred(),
        // Group-exchange sizes as PuTTY asks for them (min 1024, max 8192); russh's
        // default minimum of 3072 fails on servers whose moduli stop at 2048 or 1024.
        gex: client::GexParams::new(1024, 4096, 8192).map_err(|e| e.to_string())?,
        ..Default::default()
    });
    let handler = Client { host: host.clone(), port, events: events.clone() };
    let mut handle = client::connect(config, (host.as_str(), port), handler)
        .await
        .map_err(|e| format!("연결하지 못했습니다: {e}"))?;

    let user = match user {
        Some(u) => u,
        None => ask(events, "login as: ", true).await.filter(|u| !u.is_empty()).ok_or("로그인을 취소했습니다.")?,
    };
    authenticate(&mut handle, &user, &host, password, events).await?;

    // The pty is sized from the window, so wait for the first layout.
    // Keystrokes typed in the meantime are replayed once the shell is up.
    let mut early_input = Vec::new();
    let size = loop {
        match cmds.recv().await {
            Some(Cmd::Resize(size)) => break size,
            Some(Cmd::Input(bytes)) => early_input.extend(bytes),
            None => return Ok(String::new()),
        }
    };

    let channel = handle.channel_open_session().await.map_err(|e| format!("세션을 열지 못했습니다: {e}"))?;
    channel
        .request_pty(false, term, size.cols.into(), size.rows.into(), size.width_px, size.height_px, &[])
        .await
        .map_err(|e| format!("터미널을 요청하지 못했습니다: {e}"))?;
    channel.request_shell(true).await.map_err(|e| format!("셸을 시작하지 못했습니다: {e}"))?;

    // Writes wait for the server's receive window, which only opens while incoming
    // data keeps being read. So writing happens in its own task, never in the read loop.
    let (mut reader, writer) = channel.split();
    let (to_writer, mut outgoing) = mpsc::unbounded_channel::<Cmd>();
    tokio::spawn(async move {
        while let Some(cmd) = outgoing.recv().await {
            match cmd {
                Cmd::Input(bytes) => {
                    if writer.data(&bytes[..]).await.is_err() {
                        break;
                    }
                }
                Cmd::Resize(s) => {
                    let _ = writer.window_change(s.cols.into(), s.rows.into(), s.width_px, s.height_px).await;
                }
            }
        }
    });
    if !early_input.is_empty() {
        let _ = to_writer.send(Cmd::Input(early_input));
    }

    let mut exit_status = None;
    loop {
        tokio::select! {
            msg = reader.wait() => match msg {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if events.send(Event::Output(data.to_vec())).is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status: code }) => exit_status = Some(code),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
            cmd = cmds.recv() => match cmd {
                Some(cmd) => {
                    let _ = to_writer.send(cmd);
                }
                // The window was closed.
                None => {
                    let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
                    return Ok(String::new());
                }
            },
        }
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
    Ok(match exit_status {
        Some(code) => format!("세션이 끝났습니다 (종료 코드 {code})."),
        None => "세션이 끝났습니다.".into(),
    })
}

/// Password first (what `-pw` means in PuTTY), then whatever the server says it
/// still accepts: keyboard-interactive or password, asking in the terminal.
async fn authenticate(
    handle: &mut client::Handle<Client>,
    user: &str,
    host: &str,
    password: Option<String>,
    events: &Events,
) -> Result<(), String> {
    let failed = |e: russh::Error| format!("로그인 중 오류: {e}");
    // Empty means "not told yet": try keyboard-interactive, then password.
    let mut allowed: Vec<MethodKind> = Vec::new();

    if let Some(pw) = &password {
        match handle.authenticate_password(user, pw).await.map_err(failed)? {
            AuthResult::Success => return Ok(()),
            AuthResult::Failure { remaining_methods, .. } => {
                allowed = remaining_methods.to_vec();
                // Keyboard-interactive will still try the same password; only say so when it won't.
                if !allowed.contains(&MethodKind::KeyboardInteractive) {
                    notice(events, "전달받은 비밀번호(-pw)로 로그인하지 못했습니다.");
                }
            }
        }
    }

    // The given password answers the first hidden keyboard-interactive prompt.
    let mut unused_password = password;
    for attempt in 0..3 {
        if attempt > 0 {
            notice(events, "로그인하지 못했습니다. 다시 입력해 주세요.");
        }
        let mut asked = false;
        if allowed.is_empty() || allowed.contains(&MethodKind::KeyboardInteractive) {
            let mut response = handle.authenticate_keyboard_interactive_start(user, None).await.map_err(failed)?;
            loop {
                match response {
                    KeyboardInteractiveAuthResponse::Success => return Ok(()),
                    KeyboardInteractiveAuthResponse::Failure { remaining_methods, .. } => {
                        allowed = remaining_methods.to_vec();
                        break;
                    }
                    KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                        for text in [name, instructions] {
                            if !text.is_empty() {
                                notice(events, text);
                            }
                        }
                        let mut answers = Vec::with_capacity(prompts.len());
                        for prompt in prompts {
                            let answer = match (prompt.echo, unused_password.take()) {
                                (false, Some(pw)) => pw,
                                (_, pw) => {
                                    unused_password = pw;
                                    asked = true;
                                    ask(events, prompt.prompt, prompt.echo).await.ok_or("로그인을 취소했습니다.")?
                                }
                            };
                            answers.push(answer);
                        }
                        response = handle.authenticate_keyboard_interactive_respond(answers).await.map_err(failed)?;
                    }
                }
            }
        }
        // Don't ask twice in one round when the server took a keyboard-interactive answer.
        if asked && allowed.contains(&MethodKind::KeyboardInteractive) {
            continue;
        }
        if allowed.contains(&MethodKind::Password) {
            let pw = ask(events, format!("{user}@{host}'s password: "), false).await.ok_or("로그인을 취소했습니다.")?;
            match handle.authenticate_password(user, pw).await.map_err(failed)? {
                AuthResult::Success => return Ok(()),
                AuthResult::Failure { remaining_methods, .. } => allowed = remaining_methods.to_vec(),
            }
        } else if !allowed.contains(&MethodKind::KeyboardInteractive) {
            let names: Vec<&str> = allowed.iter().map(<&str>::from).collect();
            return Err(format!("서버가 허용하는 로그인 방식({})은 아직 지원하지 않습니다.", names.join(", ")));
        }
    }
    Err("로그인하지 못했습니다.".into())
}

/// russh's safe defaults, with the older algorithms PuTTY still speaks appended
/// at the lowest priority so servers that offer nothing newer can still connect.
fn preferred() -> Preferred {
    let defaults = Preferred::default();

    let mut kex_list = defaults.kex.to_vec();
    let at = kex_list.iter().position(|k| *k == kex::EXTENSION_SUPPORT_AS_CLIENT).unwrap_or(kex_list.len());
    kex_list.splice(at..at, [kex::DH_GEX_SHA1, kex::DH_G14_SHA1, kex::DH_G1_SHA1]);

    let mut ciphers = defaults.cipher.to_vec();
    ciphers.extend([cipher::AES_256_CBC, cipher::AES_192_CBC, cipher::AES_128_CBC, cipher::TRIPLE_DES_CBC]);

    let mut macs = defaults.mac.to_vec();
    macs.extend([mac::HMAC_SHA1_ETM, mac::HMAC_SHA1]);

    // ssh-dss host keys, for servers that have no other kind.
    let mut host_keys = defaults.key.to_vec();
    host_keys.push(keys::Algorithm::Dsa);

    Preferred { kex: kex_list.into(), key: host_keys.into(), cipher: ciphers.into(), mac: macs.into(), ..defaults }
}

struct Client {
    host: String,
    port: u16,
    events: Events,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            notice(&self.events, "서버가 인증서로 신원을 밝혔지만, 인증서 확인은 아직 지원하지 않습니다.");
            return Ok(false);
        };
        let Some(path) = crate::config::user_config_dir().map(|d| d.join("known_hosts")) else {
            notice(&self.events, "서버 키를 저장할 폴더를 찾지 못했습니다.");
            return Ok(false);
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256);
        match keys::check_known_hosts_path(&self.host, self.port, key, &path) {
            Ok(true) => Ok(true),
            Err(keys::Error::KeyChanged { line }) => {
                notice(
                    &self.events,
                    format!(
                        "경고: 서버 키가 저장된 것과 다릅니다. 접속을 중단합니다.\n  지금 받은 키: {} {fingerprint}\n  저장된 키: {} ({line}번째 줄)",
                        key.algorithm(),
                        path.display()
                    ),
                );
                Ok(false)
            }
            other => {
                if let Err(e) = &other {
                    notice(&self.events, format!("서버 키 기록({})을 읽지 못했습니다: {e}", path.display()));
                }
                let question = format!(
                    "처음 접속하는 서버입니다. 서버 키:\r\n  {} {fingerprint}\r\n이 키를 저장하고 접속할까요? (y/n) ",
                    key.algorithm()
                );
                let yes = ask(&self.events, question, true).await.is_some_and(|a| a.trim().eq_ignore_ascii_case("y"));
                // Don't append to a file we couldn't read.
                if yes && other.is_ok() {
                    if let Err(e) = keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, &path) {
                        notice(&self.events, format!("서버 키를 저장하지 못했습니다 ({e}). 이번 접속만 계속합니다."));
                    }
                }
                Ok(yes)
            }
        }
    }
}

fn notice(events: &Events, text: impl Into<String>) {
    let _ = events.send(Event::Notice(text.into()));
}

async fn ask(events: &Events, text: impl Into<String>, echo: bool) -> Option<String> {
    let (reply, answer) = oneshot::channel();
    events.send(Event::Prompt { text: text.into(), echo, reply }).ok()?;
    answer.await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_algorithms_come_last() {
        let p = preferred();
        let names: Vec<&str> = p.kex.iter().map(|k| k.as_ref()).collect();
        let g1 = names.iter().position(|n| *n == "diffie-hellman-group1-sha1").unwrap();
        let modern = names.iter().position(|n| *n == "curve25519-sha256").unwrap();
        let ext = names.iter().position(|n| *n == "ext-info-c").unwrap();
        assert!(modern < g1 && g1 < ext);
        assert_eq!(p.cipher.last().map(|c| c.as_ref()), Some("3des-cbc"));
        assert_eq!(p.mac.last().map(|m| m.as_ref()), Some("hmac-sha1"));
        assert_eq!(p.key.last(), Some(&keys::Algorithm::Dsa));
    }
}
