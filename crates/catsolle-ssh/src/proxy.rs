use crate::config::{ProxyConfig, ProxyType};
use anyhow::Result;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub async fn connect_via_proxy(proxy: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;
    match proxy.proxy_type {
        ProxyType::Socks5 => socks5_handshake(&mut stream, proxy, host, port).await?,
        ProxyType::HttpConnect => http_connect_handshake(&mut stream, proxy, host, port).await?,
    }
    Ok(stream)
}

async fn socks5_handshake(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<()> {
    let mut methods = vec![0x00u8];
    if proxy.username.is_some() && proxy.password.is_some() {
        methods.push(0x02);
    }
    stream.write_all(&[0x05, methods.len() as u8]).await?;
    stream.write_all(&methods).await?;

    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp).await?;
    if resp[0] != 0x05 {
        anyhow::bail!("invalid socks5 version");
    }
    match resp[1] {
        0x00 => {}
        0x02 => {
            let username = proxy.username.clone().unwrap_or_default();
            let password = proxy
                .password
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            if username.len() > 255 || password.len() > 255 {
                anyhow::bail!("socks5 auth too long");
            }
            let mut auth = Vec::with_capacity(3 + username.len() + password.len());
            auth.push(0x01);
            auth.push(username.len() as u8);
            auth.extend_from_slice(username.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await?;
            let mut auth_resp = [0u8; 2];
            stream.read_exact(&mut auth_resp).await?;
            if auth_resp[1] != 0x00 {
                anyhow::bail!("socks5 auth failed");
            }
        }
        _ => anyhow::bail!("socks5 auth method unsupported"),
    }

    let mut req = Vec::new();
    req.push(0x05);
    req.push(0x01);
    req.push(0x00);

    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        req.push(0x01);
        req.extend_from_slice(&ip.octets());
    } else if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        req.push(0x04);
        req.extend_from_slice(&ip.octets());
    } else {
        req.push(0x03);
        req.push(host.len() as u8);
        req.extend_from_slice(host.as_bytes());
    }

    req.push((port >> 8) as u8);
    req.push((port & 0xff) as u8);

    stream.write_all(&req).await?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    if header[1] != 0x00 {
        anyhow::bail!("socks5 connect failed: code {}", header[1]);
    }
    let atyp = header[3];
    match atyp {
        0x01 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut addr = vec![0u8; len[0] as usize];
            stream.read_exact(&mut addr).await?;
        }
        0x04 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
        }
        _ => anyhow::bail!("socks5 invalid atyp"),
    }
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    Ok(())
}

async fn http_connect_handshake(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<()> {
    let target = format!("{}:{}", host, port);
    let mut req = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n", target, target);
    if let (Some(user), Some(pass)) = (&proxy.username, &proxy.password) {
        let token =
            base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass.as_str()));
        req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", token));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            break;
        }
    }
    let resp = String::from_utf8_lossy(&buf);
    let mut lines = resp.lines();
    let status = lines.next().unwrap_or("");
    if !status.contains("200") {
        anyhow::bail!("http connect failed: {}", status);
    }
    Ok(())
}
