//! Real-process semantic CLI lifecycle and configuration coverage.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::tempdir;

struct OllamaFixture {
    base_url: String,
    stop: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    thread: Option<thread::JoinHandle<()>>,
}

impl OllamaFixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("fixture address");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_embedding(&mut stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
        });
        Self {
            base_url: format!("http://{address}"),
            stop,
            address,
            thread: Some(thread),
        }
    }
}

impl Drop for OllamaFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("fixture thread");
        }
    }
}

fn serve_embedding(stream: &mut TcpStream) {
    let body = read_http_body(stream);
    if body.is_empty() {
        return;
    }
    let request: Value = serde_json::from_slice(&body).expect("embedding request");
    assert_eq!(request["truncate"], false);
    let inputs = request["input"].as_array().expect("batched input");
    let embeddings = inputs.iter().map(|_| json!([1.0, 0.0])).collect::<Vec<_>>();
    let response = json!({
        "model": request["model"],
        "embeddings": embeddings,
    })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response.len(),
        response
    )
    .expect("fixture response");
}

fn read_http_body(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).expect("request read");
        if read == 0 {
            return Vec::new();
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
        })
        .expect("content-length");
    while request.len() - header_end < content_length {
        let read = stream.read(&mut buffer).expect("request body");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    request[header_end..header_end + content_length].to_vec()
}

fn mdtree(workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mdtree"))
        .arg("--workspace")
        .arg(workspace)
        .arg("--output")
        .arg("json")
        .args(args)
        .output()
        .expect("mdtree process")
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "JSON output: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn real_cli_builds_searches_resumes_retries_reports_and_clears() {
    let directory = tempdir().expect("directory");
    let workspace = directory.path().join("semantic-cli.mdtree");
    let initialized = mdtree(&workspace, &["init", "Orders"]);
    assert!(initialized.status.success());
    let ollama = OllamaFixture::start();

    let build = Command::new(env!("CARGO_BIN_EXE_mdtree"))
        .arg("--workspace")
        .arg(&workspace)
        .arg("--output")
        .arg("json")
        .arg("--ollama-url")
        .arg(&ollama.base_url)
        .env("MDTREE_OLLAMA_URL", "http://127.0.0.1:9")
        .env("MDTREE_OLLAMA_MODEL", "fixture")
        .args([
            "semantic-index",
            "build",
            "--dimensions",
            "2",
            "--batch-size",
            "4",
        ])
        .output()
        .expect("semantic build");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let build = json_output(&build);
    assert_eq!(build["status"], "complete");
    assert_eq!(build["report"]["status"]["coverage"]["ready"], 1);

    let status = mdtree(&workspace, &["semantic-index", "status"]);
    assert!(status.status.success());
    assert_eq!(json_output(&status)["state"], "ready");

    for action in ["resume", "retry"] {
        let output = mdtree_with_ollama(
            &workspace,
            &ollama,
            &["--ollama-model", "fixture", "semantic-index", action],
        );
        assert!(output.status.success(), "{action}");
        assert_eq!(json_output(&output)["status"], "complete");
    }

    let lexical = mdtree(&workspace, &["search", "Orders"]);
    assert!(lexical.status.success());
    assert!(json_output(&lexical)["items"].is_array());

    for mode in ["semantic", "hybrid"] {
        let output = mdtree_with_ollama(
            &workspace,
            &ollama,
            &[
                "--ollama-model",
                "fixture",
                "search",
                "Orders",
                "--mode",
                mode,
            ],
        );
        assert!(output.status.success(), "{mode}");
        assert_eq!(
            json_output(&output)["matches"]["items"][0]["title"],
            "Orders"
        );
    }

    let stopped_url = ollama.base_url.clone();
    drop(ollama);
    let unavailable = Command::new(env!("CARGO_BIN_EXE_mdtree"))
        .arg("--workspace")
        .arg(&workspace)
        .arg("--output")
        .arg("json")
        .arg("--ollama-url")
        .arg(stopped_url)
        .arg("--ollama-model")
        .arg("fixture")
        .args(["search", "Orders", "--mode", "semantic"])
        .output()
        .expect("unavailable semantic search");
    assert_eq!(unavailable.status.code(), Some(1));
    let unavailable = json_output(&unavailable);
    assert_eq!(unavailable["status"], "error");
    assert_eq!(unavailable["error"]["code"], "provider_unavailable");

    let planned = mdtree(&workspace, &["semantic-index", "clear", "--dry-run"]);
    assert!(planned.status.success());
    assert_eq!(json_output(&planned)["status"], "planned");
    let cleared = mdtree(&workspace, &["semantic-index", "clear", "--yes"]);
    assert!(cleared.status.success());
    assert_eq!(json_output(&cleared)["status"], "cleared");
}

fn mdtree_with_ollama(workspace: &Path, ollama: &OllamaFixture, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mdtree"))
        .arg("--workspace")
        .arg(workspace)
        .arg("--output")
        .arg("json")
        .arg("--ollama-url")
        .arg(&ollama.base_url)
        .args(args)
        .output()
        .expect("mdtree process")
}

#[test]
fn help_documents_modes_lifecycle_and_ollama_configuration() {
    let help = Command::new(env!("CARGO_BIN_EXE_mdtree"))
        .args(["semantic-index", "--help"])
        .output()
        .expect("semantic help");
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).expect("UTF-8 help");
    for expected in ["build", "resume", "retry", "status", "clear"] {
        assert!(help.contains(expected), "{expected}");
    }

    let search = Command::new(env!("CARGO_BIN_EXE_mdtree"))
        .args(["search", "--help"])
        .output()
        .expect("search help");
    let search = String::from_utf8(search.stdout).expect("UTF-8 help");
    assert!(search.contains("--mode"));
    assert!(search.contains("lexical"));
    assert!(search.contains("semantic"));
    assert!(search.contains("hybrid"));
}
