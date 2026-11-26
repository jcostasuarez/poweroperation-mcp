//! Lector de tablas dBase (.dbf), el formato de la base de configuración de proyectos Citect /
//! Plant SCADA / Power Operation (variable.dbf, digalm.dbf, units.dbf, ...). Solo lectura.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::text::decode;

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub kind: char,
    pub len: usize,
    offset: usize,
}

pub struct Dbf {
    pub fields: Vec<Field>,
    data: Vec<u8>,
    header_len: usize,
    record_len: usize,
    /// Registros declarados en la cabecera (incluye los marcados como borrados).
    pub declared: usize,
}

impl Dbf {
    pub fn open(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("No se pudo leer {}", path.display()))?;
        Self::parse(data).with_context(|| format!("{} no es un DBF válido", path.display()))
    }

    pub fn parse(data: Vec<u8>) -> Result<Self> {
        if data.len() < 32 {
            bail!("archivo demasiado corto");
        }
        let declared = u32::from_le_bytes(data[4..8].try_into()?) as usize;
        let header_len = u16::from_le_bytes([data[8], data[9]]) as usize;
        let record_len = u16::from_le_bytes([data[10], data[11]]) as usize;
        if header_len < 33 || record_len == 0 || header_len > data.len() {
            bail!("cabecera inválida (header={header_len}, record={record_len})");
        }
        let mut fields = Vec::new();
        let mut pos = 32;
        let mut offset = 1; // byte 0 de cada registro = marca de borrado
        while pos + 32 <= header_len && data[pos] != 0x0D {
            let d = &data[pos..pos + 32];
            let name_end = d[..11].iter().position(|&b| b == 0).unwrap_or(11);
            let name = String::from_utf8_lossy(&d[..name_end]).trim().to_string();
            let kind = d[11] as char;
            let len = if kind == 'C' { d[16] as usize | ((d[17] as usize) << 8) } else { d[16] as usize };
            fields.push(Field { name, kind, len, offset });
            offset += len;
            pos += 32;
        }
        if offset > record_len {
            bail!("los campos ({offset} bytes) no entran en el registro ({record_len} bytes)");
        }
        Ok(Self { fields, data, header_len, record_len, declared })
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// Recorre los registros vigentes: `f(número_de_registro, valores)`; devolver `false` corta.
    pub fn for_each(&self, mut f: impl FnMut(usize, Vec<String>) -> bool) {
        for i in 0..self.declared {
            let start = self.header_len + i * self.record_len;
            let Some(rec) = self.data.get(start..start + self.record_len) else { break };
            match rec[0] {
                b'*' => continue,
                0x1A => break,
                _ => {}
            }
            let values = self.fields.iter().map(|fd| value(fd, &rec[fd.offset..fd.offset + fd.len])).collect();
            if !f(i + 1, values) {
                break;
            }
        }
    }

    pub fn count(&self) -> usize {
        let mut n = 0;
        self.for_each(|_, _| {
            n += 1;
            true
        });
        n
    }
}

fn value(fd: &Field, raw: &[u8]) -> String {
    match fd.kind {
        'L' => match raw.first() {
            Some(b'T' | b't' | b'Y' | b'y') => "TRUE".into(),
            Some(b'F' | b'f' | b'N' | b'n') => "FALSE".into(),
            _ => String::new(),
        },
        'D' => {
            let s = decode(raw);
            let s = s.trim();
            if s.len() == 8 { format!("{}-{}-{}", &s[..4], &s[4..6], &s[6..]) } else { s.to_string() }
        }
        _ => decode(raw).trim().to_string(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Construye un DBF mínimo (para pruebas).
    pub fn build(fields: &[(&str, u8)], rows: &[(&[&str], bool)]) -> Vec<u8> {
        let header_len = 32 + 32 * fields.len() + 1;
        let record_len = 1 + fields.iter().map(|f| f.1 as usize).sum::<usize>();
        let mut d = vec![0u8; 32];
        d[0] = 0x03;
        d[4..8].copy_from_slice(&(rows.len() as u32).to_le_bytes());
        d[8..10].copy_from_slice(&(header_len as u16).to_le_bytes());
        d[10..12].copy_from_slice(&(record_len as u16).to_le_bytes());
        for (name, len) in fields {
            let mut fd = [0u8; 32];
            fd[..name.len()].copy_from_slice(name.as_bytes());
            fd[11] = b'C';
            fd[16] = *len;
            d.extend_from_slice(&fd);
        }
        d.push(0x0D);
        for (vals, deleted) in rows {
            d.push(if *deleted { b'*' } else { b' ' });
            for ((_, len), v) in fields.iter().zip(vals.iter()) {
                let mut cell = crate::text::encode(v);
                cell.resize(*len as usize, b' ');
                d.extend_from_slice(&cell);
            }
        }
        d.push(0x1A);
        d
    }

    #[test]
    fn parse_records() {
        let data = build(
            &[("TAG", 20), ("UNIT", 10), ("COMMENT", 30)],
            &[
                (&["Feeder1\\MMXU1\\TotW", "IODev1", "Potencia activa"], false),
                (&["Borrado", "IODev1", ""], true),
                (&["Feeder1\\XCBR1\\Pos", "IODev1", "Posición interruptor"], false),
            ],
        );
        let dbf = Dbf::parse(data).unwrap();
        assert_eq!(dbf.fields.len(), 3);
        assert_eq!(dbf.field_index("unit"), Some(1));
        let mut rows = vec![];
        dbf.for_each(|n, v| {
            rows.push((n, v));
            true
        });
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1[0], "Feeder1\\MMXU1\\TotW");
        assert_eq!(rows[1].0, 3);
        assert_eq!(rows[1].1[2], "Posición interruptor");
    }
}
