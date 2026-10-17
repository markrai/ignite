//! Disposable argv-compatible Dropbear stub for Review 114 supervisor tests.
//! Not shipped in the Kindle binary. Not a substitute for Review 113 device evidence.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-L" | "-R" | "-D" | "-y" | "-A" | "-g" | "-f" | "-t" | "-N"
        )
    }) {
        eprintln!("unsafe argument");
        process::exit(2);
    }
    if !args.iter().any(|a| a == "-B") {
        eprintln!("need -B");
        process::exit(2);
    }
    if let Ok(log) = env::var("IGNITE_FAKE_ARGV_LOG") {
        let _ = fs::write(log, args.join("\0"));
    }
    let home = env::var("HOME").unwrap_or_default();
    let home_path = PathBuf::from(&home);
    if home_path.join("hang").exists() {
        loop {
            thread::sleep(Duration::from_secs(30));
        }
    }
    if home_path.join("exit_before").exists() {
        process::exit(3);
    }
    let kh = home_path.join(".ssh").join("known_hosts");
    let text = fs::read_to_string(&kh).unwrap_or_default();
    if text.trim().is_empty() {
        eprintln!("Host key unknown.\nNot accepted automatically");
        process::exit(1);
    }
    if text.contains("WRONG_PIN") {
        eprintln!("ssh-ed25519 host key mismatch");
        process::exit(1);
    }
    let id = args
        .windows(2)
        .find(|w| w[0] == "-i")
        .map(|w| PathBuf::from(&w[1]));
    if let Some(p) = id {
        match fs::read(&p) {
            Ok(b) if !b.is_empty() => {}
            _ => {
                eprintln!("client authentication refused");
                process::exit(6);
            }
        }
    }
    if env::var("SSH_ASKPASS").is_ok() || env::var("DROPBEAR_PASSWORD").is_ok() {
        eprintln!("password env leaked");
        process::exit(7);
    }
    let forward = match fs::read_to_string(home_path.join("forward_tcp")) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            eprintln!("no forward_tcp fixture map");
            process::exit(8);
        }
    };
    if home_path.join("hang_after_read").exists() {
        let _ = read_http_message(&mut std::io::stdin());
        loop {
            thread::sleep(Duration::from_secs(30));
        }
    }
    if home_path.join("exit_during_write").exists() {
        let mut b = [0u8; 1];
        let _ = std::io::stdin().read(&mut b);
        process::exit(4);
    }
    let mut sock = match TcpStream::connect(forward.trim()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("forward connect failed: {e}");
            process::exit(9);
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(5)));
    let req = match read_http_message(&mut std::io::stdin()) {
        Ok(b) => b,
        Err(_) => process::exit(10),
    };
    if home_path.join("exit_during_response").exists() {
        let _ = sock.write_all(&req);
        process::exit(5);
    }
    if sock.write_all(&req).is_err() {
        process::exit(11);
    }
    let mut resp = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if resp.len() > 8 * 1024 * 1024 {
            break;
        }
    }
    eprintln!("ssh-diag-not-http");
    let _ = std::io::stdout().write_all(&resp);
    let _ = std::io::stdout().flush();
    if home_path.join("respond_then_hang").exists() {
        loop {
            thread::sleep(Duration::from_secs(30));
        }
    }
}

fn read_http_message<R: Read>(r: &mut R) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        let n = r.read(&mut tmp).map_err(|_| ())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = &buf[..idx];
            let extra = buf[idx + 4..].to_vec();
            let mut need = 0usize;
            for line in String::from_utf8_lossy(header).split("\r\n") {
                if let Some(rest) = line
                    .split_once(':')
                    .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.trim())
                {
                    need = rest.parse().unwrap_or(0);
                }
            }
            let mut body = extra;
            while body.len() < need {
                let take = (need - body.len()).min(tmp.len());
                let n = r.read(&mut tmp[..take]).map_err(|_| ())?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            let mut out = buf[..idx + 4].to_vec();
            out.extend_from_slice(&body[..body.len().min(need)]);
            if need == 0 {
                return Ok(buf);
            }
            return Ok(out);
        }
        if buf.len() > 64 * 1024 {
            return Err(());
        }
    }
    Ok(buf)
}
