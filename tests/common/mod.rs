//! Utilidades de las pruebas end-to-end: cliente MCP por stdio contra el exe, DBF de prueba y
//! verificaciones que se acumulan (se informan todas juntas al final).
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::{Value, json};

const EXE: &str = env!("CARGO_BIN_EXE_poweroperation-mcp");
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// Carpeta temporal propia de cada prueba (se borra al empezar).
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pomcp-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Tabla dBase con campos de texto (como las de los proyectos Citect).
pub fn write_dbf(path: &Path, fields: &[(&str, u8)], rows: &[&[&str]]) {
    let rec_len = 1 + fields.iter().map(|f| f.1 as usize).sum::<usize>();
    let hdr_len = 32 + 32 * fields.len() + 1;
    let mut out = vec![3u8, 126, 10, 14];
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&(hdr_len as u16).to_le_bytes());
    out.extend_from_slice(&(rec_len as u16).to_le_bytes());
    out.resize(32, 0);
    for (name, len) in fields {
        let mut fd = [0u8; 32];
        fd[..name.len()].copy_from_slice(name.as_bytes());
        fd[11] = b'C';
        fd[16] = *len;
        out.extend_from_slice(&fd);
    }
    out.push(b'\r');
    for row in rows {
        out.push(b' ');
        for ((_, len), v) in fields.iter().zip(row.iter()) {
            let mut cell = cp1252(v);
            cell.resize(*len as usize, b' ');
            out.extend_from_slice(&cell);
        }
    }
    out.push(0x1A);
    std::fs::write(path, out).unwrap();
}

/// Texto en cp1252 (solo hace falta Latin-1 en las pruebas).
pub fn cp1252(s: &str) -> Vec<u8> {
    s.chars().map(|c| u8::try_from(u32::from(c)).expect("carácter fuera de Latin-1")).collect()
}

/// Proyecto falso `PO_Demo` (incluye `PLS_Include`) en `root`, como lo armaba offline.py.
pub fn fake_projects(root: &Path) {
    let main = root.join("PO_Demo");
    let lib = root.join("PLS_Include");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    write_dbf(
        &main.join("variable.dbf"),
        &[("TAG", 60), ("TYPE", 10), ("UNIT", 16), ("ENG_UNITS", 8), ("COMMENT", 48)],
        &[
            &["Feeder1\\MMXU1\\TotW", "REAL", "Feeder1", "kW", "Potencia activa total"],
            &["Feeder1\\XCBR1\\Pos", "DIGITAL", "Feeder1", "", "Posición interruptor"],
            &["Feeder2\\MMXU1\\TotW", "REAL", "Feeder2", "kW", ""],
        ],
    );
    write_dbf(
        &main.join("digalm.dbf"),
        &[("TAG", 40), ("NAME", 40), ("VAR_A", 60), ("CATEGORY", 8)],
        &[&["Feeder1_Trip", "Disparo Feeder1", "Feeder1\\XCBR1\\Pos", "1"]],
    );
    write_dbf(&main.join("units.dbf"), &[("NAME", 16), ("PROTOCOL", 16), ("PORT", 16)], &[&["Feeder1", "MODNET", "PORT1_TCP"]]);
    write_dbf(&main.join("include.dbf"), &[("NAME", 40)], &[&["PLS_Include"]]);
    write_dbf(&lib.join("variable.dbf"), &[("TAG", 60), ("COMMENT", 40)], &[&["PLS_Heartbeat", "Latido"]]);
    std::fs::write(
        main.join("Feeders.ci"),
        cp1252("// Lee potencia\r\nREAL FUNCTION FeederPower(STRING sDev)\r\n  RETURN StrToReal(TagRead(sDev + \"\\MMXU1\\TotW\"));\r\nEND\r\n"),
    )
    .unwrap();
    std::fs::write(lib.join("PlsLib.ci"), b"PRIVATE FUNCTION PlsInit()\r\nEND\r\n").unwrap();
    // Copia vieja de citect.ini dentro de un proyecto (como en PO 2024): debe ignorarse.
    std::fs::write(main.join("citect.ini"), format!("[CtEdit]\nRun={}\nUser={}\n", lib.display(), root.display())).unwrap();
}

/// Servidor MCP lanzado por stdio.
pub struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Mcp {
    /// Lanza el exe aislado de la instalación real: `ProgramData` y `ProgramFiles*` apuntan a una
    /// carpeta vacía (así no encuentra el Power Operation instalado) y se ignoran las POWEROP_* del entorno.
    pub fn start(work: &Path, env: &[(&str, String)]) -> Self {
        Self::launch(work, env, true)
    }

    /// Lanza el exe contra la instalación real de Power Operation (pruebas `live`).
    pub fn start_live(work: &Path, env: &[(&str, String)]) -> Self {
        Self::launch(work, env, false)
    }

    fn launch(work: &Path, env: &[(&str, String)], isolated: bool) -> Self {
        let mut cmd = Command::new(EXE);
        cmd.current_dir(work).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().to_uppercase().starts_with("POWEROP_") {
                cmd.env_remove(k);
            }
        }
        if isolated {
            let empty = work.join("empty-root");
            std::fs::create_dir_all(&empty).unwrap();
            for k in ["ProgramData", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
                cmd.env(k, &empty);
            }
            // Puerto cerrado: que alarm_sql nunca llegue a un servidor de alarmas real.
            cmd.env("POWEROP_ALARM_PORT", "1");
        }
        cmd.env("POWEROP_TIMEOUT_SECS", "20").env("POWEROP_AUDIT_LOG", work.join("audit.log"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("no se pudo lanzar el exe");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut mcp = Self { child, stdin, lines, next_id: 0 };
        let init = mcp.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}));
        assert!(init["result"]["serverInfo"]["name"] == "poweroperation-mcp", "initialize: {init}");
        mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }

    fn send(&mut self, msg: Value) {
        let stdin = self.stdin.as_mut().expect("stdin cerrado");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Llamada JSON-RPC; devuelve la respuesta completa.
    pub fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self.lines.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| panic!("sin respuesta a {method}"));
            let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout no es JSON-RPC ({e}): {line}"));
            if v["id"] == id {
                return v;
            }
        }
    }

    /// Llama a una herramienta: (es_error, texto).
    pub fn call(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        let res = &r["result"];
        let text = res["content"].as_array().map(|c| c.iter().filter_map(|x| x["text"].as_str()).collect::<String>()).unwrap_or_default();
        (res["isError"].as_bool().unwrap_or(false), text)
    }

    pub fn tool_names(&mut self) -> Vec<String> {
        let r = self.rpc("tools/list", json!({}));
        r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        drop(self.stdin.take());
        for _ in 0..50 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
    }
}

/// JSON de una respuesta de herramienta (falla con el texto si no lo es).
pub fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("respuesta no JSON ({e}): {text}"))
}

/// Verificaciones acumuladas: todas se evalúan y las fallidas se informan juntas.
#[derive(Default)]
pub struct Checks {
    failed: Vec<String>,
    total: usize,
}

impl Checks {
    pub fn check(&mut self, label: &str, cond: bool, detail: &str) {
        self.total += 1;
        if cond {
            eprintln!("PASS {label}");
        } else {
            let d: String = detail.chars().take(800).collect();
            eprintln!("FAIL {label}\n    {d}");
            self.failed.push(format!("{label}: {d}"));
        }
    }

    pub fn finish(self) {
        assert!(self.failed.is_empty(), "{} de {} verificaciones fallaron:\n{}", self.failed.len(), self.total, self.failed.join("\n"));
    }
}
