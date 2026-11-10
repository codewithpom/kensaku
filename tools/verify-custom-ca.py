"""Local TLS regression: requires Python, Cargo and OpenSSL; no system trust changes."""
import http.server
import os
from pathlib import Path
import ssl
import subprocess
import tempfile
import threading


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


with tempfile.TemporaryDirectory(prefix="kensaku-ca-") as tmp:
    root = Path(tmp)
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
        "-keyout", str(root / "key.pem"), "-out", str(root / "ca.pem"),
        "-days", "1", "-subj", "/CN=localhost", "-addext",
        "subjectAltName=DNS:localhost", stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL)
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", str(root / "server.key"), "-out", str(root / "server.csr"), "-subj", "/CN=localhost", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    (root / "extensions").write_text("subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", str(root / "server.csr"), "-CA", str(root / "ca.pem"), "-CAkey", str(root / "key.pem"), "-CAcreateserial", "-out", str(root / "server.pem"), "-days", "1", "-extfile", str(root / "extensions"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    certdir = root / "certs"
    certdir.mkdir()
    (certdir / "ca.pem").write_bytes((root / "ca.pem").read_bytes())
    cert_hash = subprocess.check_output(["openssl", "x509", "-hash", "-noout", "-in", str(root / "ca.pem")], text=True).strip()
    (certdir / (cert_hash + ".0")).write_bytes((root / "ca.pem").read_bytes())
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), http.server.SimpleHTTPRequestHandler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(root / "server.pem", root / "server.key")
    server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (root / "src").mkdir()
    (root / "Cargo.toml").write_text('[package]\nname="kensaku-ca-probe"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nureq="=2.12.1"\n[features]\nnative=["ureq/native-certs"]\n')
    (root / "src/main.rs").write_text('fn main() { match ureq::get(&std::env::args().nth(1).unwrap()).call() { Ok(_) => (), Err(e) => { eprintln!("{e}"); std::process::exit(1); } } }')
    env = dict(os.environ)
    for key in ("SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"):
        env.pop(key, None)
        env.pop(key.lower(), None)
    url = f"https://localhost:{server.server_port}/"
    def probe(native, overrides, target, success, label):
        command = ["cargo", "run", "--quiet", "--manifest-path", str(root / "Cargo.toml")]
        if native:
            command += ["--features", "native"]
        result = subprocess.run(command + ["--", target], env=env | overrides, capture_output=True, text=True)
        assert (result.returncode == 0) == success, (label, result.stderr)
        if not success:
            assert "invalid peer certificate" in result.stderr, result.stderr
        print(f"PASS: {label}", flush=True)
    try:
        probe(False, {"SSL_CERT_FILE": str(root / "ca.pem")}, url, False, "baseline ignores custom CA")
        probe(True, {}, url, False, "untrusted CA rejected")
        probe(True, {"SSL_CERT_FILE": str(root / "ca.pem")}, url, True, "custom PEM trusted")
        probe(True, {"SSL_CERT_DIR": str(certdir)}, url, True, "custom directory trusted")
        probe(True, {"SSL_CERT_FILE": str(root / "ca.pem")}, url.replace("localhost", "127.0.0.1"), False, "hostname mismatch rejected")
    finally:
        server.shutdown()
