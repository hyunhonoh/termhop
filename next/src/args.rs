//! PuTTY-style command line: `[-ssh] [-l user] [-pw password] [-P port] [user@]host`.
//!
//! Only what a launcher passes when it starts PuTTY for an SSH login is accepted.
//! Anything else is rejected with a message instead of being silently ignored.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// Flags that only tune PuTTY/plink behavior and have no meaning here.
const IGNORED: &[&str] = &[
    "-ssh", "-2", "-4", "-6", "-v", "-batch", "-no-antispoof", "-t", "-agent", "-noagent", "-a",
    "-A", "-x", "-X", "-C",
];

pub fn parse(args: &[String]) -> Result<Target, String> {
    let mut user = None;
    let mut password = None;
    let mut port = None;
    let mut host: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next().cloned().ok_or_else(|| format!("{name} 다음에 값이 필요합니다."))
        };
        match arg.as_str() {
            "-l" => user = Some(value("-l")?),
            "-pw" => password = Some(value("-pw")?),
            "-P" => {
                let v = value("-P")?;
                port = Some(v.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| format!("포트 번호가 올바르지 않습니다: {v}"))?);
            }
            a if IGNORED.contains(&a) => {}
            a if a.starts_with('@') => return Err("저장된 PuTTY 세션(@이름)은 아직 지원하지 않습니다.".into()),
            a if a.starts_with('-') && a.len() > 1 => return Err(format!("지원하지 않는 옵션입니다: {a}")),
            a => {
                if host.is_some() {
                    return Err(format!("서버 주소 뒤의 인자는 지원하지 않습니다: {a}"));
                }
                host = Some(a.to_string());
            }
        }
    }

    let mut host = host.ok_or("서버 주소가 없습니다.")?;
    if let Some((u, h)) = host.rsplit_once('@') {
        // `-l` wins over `user@host`, as in PuTTY.
        if user.is_none() && !u.is_empty() {
            user = Some(u.to_string());
        }
        host = h.to_string();
    }
    if host.is_empty() {
        return Err("서버 주소가 없습니다.".into());
    }
    Ok(Target { host, port: port.unwrap_or(22), user: user.filter(|u| !u.is_empty()), password })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Target, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn launcher_style() {
        let t = p(&["-ssh", "-pw", "secret", "-l", "a.b.c", "-P", "2222", "example.com"]).unwrap();
        assert_eq!(
            t,
            Target {
                host: "example.com".into(),
                port: 2222,
                user: Some("a.b.c".into()),
                password: Some("secret".into())
            }
        );
    }

    #[test]
    fn user_at_host_and_default_port() {
        let t = p(&["me@example.com"]).unwrap();
        assert_eq!((t.user.as_deref(), t.host.as_str(), t.port), (Some("me"), "example.com", 22));
        let t = p(&["-l", "other", "me@example.com"]).unwrap();
        assert_eq!(t.user.as_deref(), Some("other"));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&[]).is_err());
        assert!(p(&["-P", "0", "h"]).is_err());
        assert!(p(&["-P", "x", "h"]).is_err());
        assert!(p(&["-pw"]).is_err());
        assert!(p(&["-load", "s"]).is_err());
        assert!(p(&["@saved"]).is_err());
        assert!(p(&["h", "uptime"]).is_err());
        assert!(p(&["me@"]).is_err());
    }

    #[test]
    fn ignores_plink_tuning_flags() {
        assert_eq!(p(&["-no-antispoof", "-batch", "-v", "h"]).unwrap().host, "h");
    }
}
