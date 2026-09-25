#![allow(
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "the local demo server reports its address and recoverable connection errors"
)]

use std::{
    error::Error,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
};

const INDEX: &[u8] = include_bytes!("../../web/index.html");
const SCRIPT: &[u8] = include_bytes!("../../web/app.js");
const STYLES: &[u8] = include_bytes!("../../web/styles.css");
const HEAD: &[u8] = include_bytes!("../../assets/aether-head.glb");

fn main() -> Result<(), Box<dyn Error>> {
    let address = "127.0.0.1:8787";
    let listener = TcpListener::bind(address)?;
    println!("character creator available at http://{address}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(error) = serve(stream) {
                    eprintln!("request failed: {error}");
                }
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }
    Ok(())
}

fn serve(mut stream: TcpStream) -> Result<(), Box<dyn Error>> {
    let mut request_line = String::new();
    BufReader::new(&stream).read_line(&mut request_line)?;
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/");
    let (status, content_type, body) = match path {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX),
        "/app.js" => ("200 OK", "text/javascript; charset=utf-8", SCRIPT),
        "/styles.css" => ("200 OK", "text/css; charset=utf-8", STYLES),
        "/assets/aether-head.glb" => ("200 OK", "model/gltf-binary", HEAD),
        _ => ("404 Not Found", "text/plain; charset=utf-8", &b"not found"[..]),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}
