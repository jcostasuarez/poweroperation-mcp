//! Utilidades de texto: Citect trabaja en ANSI (Windows-1252), el MCP en UTF-8.

use std::ffi::CString;

use anyhow::{Result, bail};
use encoding_rs::WINDOWS_1252;

/// Decodifica bytes ANSI (o UTF-8 si lo son) a String.
pub fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => WINDOWS_1252.decode_without_bom_handling(bytes).0.into_owned(),
    }
}

/// Decodifica un buffer C terminado en NUL.
pub fn decode_cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    decode(&buf[..end])
}

/// Codifica a Windows-1252 (los caracteres no representables se reemplazan).
pub fn encode(s: &str) -> Vec<u8> {
    WINDOWS_1252.encode(s).0.into_owned()
}

/// Cadena C en ANSI para pasar a CtAPI.
pub fn cstring(s: &str) -> Result<CString> {
    if s.contains('\0') {
        bail!("El texto no puede contener caracteres NUL");
    }
    Ok(CString::new(encode(s))?)
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}\n… [truncado: {} caracteres en total]", s.chars().count())
}

/// Coincidencia con comodines `*` y `?`, sin distinguir mayúsculas.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Patrón de filtro: con comodines se compara completo; sin ellos, "contiene".
pub fn filter_match(pattern: &str, text: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        wildcard_match(pattern, text)
    } else {
        text.to_lowercase().contains(&pattern.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard_match("Feeder*", "FEEDER_01\\MMXU1\\TotW"));
        assert!(wildcard_match("*mmxu?*", "Feeder\\MMXU1\\A"));
        assert!(!wildcard_match("Feeder?", "Feeder10"));
        assert!(filter_match("totw", "Feeder\\MMXU1\\TotW"));
    }

    #[test]
    fn ansi_roundtrip() {
        let s = "Interruptor común – señal";
        assert_eq!(decode(&encode(s)), s);
        assert_eq!(decode_cstr(b"abc\0zzz"), "abc");
    }
}
