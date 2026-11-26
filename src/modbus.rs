//! Cliente Modbus TCP de SOLO LECTURA (FC1 coils, FC2 entradas discretas, FC3 holding registers, FC4 input
//! registers). No existe ninguna función de escritura: `Area` solo representa códigos de lectura, así que el MCP
//! no puede armar un FC5/6/15/16 aunque se lo pidan.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::ModbusTarget;

/// Área de memoria Modbus que se puede leer (con la notación Schneider que usa el proyecto).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// FC1, `%M`.
    Coils,
    /// FC2, `%I`.
    Discrete,
    /// FC3, `%MW`.
    Holding,
    /// FC4, `%IW`.
    Input,
}

impl Area {
    pub fn fc(self) -> u8 {
        match self {
            Area::Coils => 1,
            Area::Discrete => 2,
            Area::Holding => 3,
            Area::Input => 4,
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            Area::Coils => "%M",
            Area::Discrete => "%I",
            Area::Holding => "%MW",
            Area::Input => "%IW",
        }
    }

    pub fn is_bits(self) -> bool {
        matches!(self, Area::Coils | Area::Discrete)
    }

    /// Máximo por pedido según la especificación Modbus.
    pub fn max_per_request(self) -> u16 {
        if self.is_bits() { 2000 } else { 125 }
    }

    /// `coils`/`%M`/`1`, `discrete`/`%I`/`2`, `holding`/`%MW`/`3`, `input`/`%IW`/`4`.
    pub fn parse(s: &str) -> Result<Area> {
        match s.trim().to_ascii_lowercase().as_str() {
            "coils" | "coil" | "%m" | "1" | "fc1" => Ok(Area::Coils),
            "discrete" | "inputs" | "%i" | "2" | "fc2" => Ok(Area::Discrete),
            "holding" | "registers" | "%mw" | "3" | "fc3" => Ok(Area::Holding),
            "input" | "input_registers" | "%iw" | "4" | "fc4" => Ok(Area::Input),
            o => bail!("Área Modbus desconocida '{o}': usá coils (%M), discrete (%I), holding (%MW) o input (%IW)"),
        }
    }
}

/// `%MW4000` → (Holding, 4000); `%M4005` → (Coils, 4005); `%IW10`, `%I3`. Acepta minúsculas y sin `%`.
pub fn parse_address(s: &str) -> Result<(Area, u16)> {
    let t = s.trim().trim_start_matches('%').to_ascii_uppercase();
    let (area, num) = if let Some(n) = t.strip_prefix("MW") {
        (Area::Holding, n)
    } else if let Some(n) = t.strip_prefix("IW") {
        (Area::Input, n)
    } else if let Some(n) = t.strip_prefix('M') {
        (Area::Coils, n)
    } else if let Some(n) = t.strip_prefix('I') {
        (Area::Discrete, n)
    } else {
        bail!("Dirección '{s}' inválida: usá %MWn, %IWn, %Mn o %In");
    };
    let n = num.parse::<u16>().map_err(|_| anyhow!("Dirección '{s}' inválida: número fuera de 0..65535"))?;
    Ok((area, n))
}

fn exception_text(code: u8) -> &'static str {
    match code {
        1 => "función no soportada (o área no habilitada en el PLC)",
        2 => "dirección fuera de rango en el PLC",
        3 => "cantidad o valor inválido",
        4 => "falla del dispositivo",
        6 => "dispositivo ocupado",
        10 => "gateway: camino no disponible",
        11 => "gateway: el equipo destino no respondió",
        _ => "excepción desconocida",
    }
}

/// Conexión TCP con tiempo máximo.
pub fn connect(t: &ModbusTarget, timeout: Duration) -> Result<TcpStream> {
    let addrs: Vec<_> = (t.host.as_str(), t.port)
        .to_socket_addrs()
        .with_context(|| format!("no se pudo resolver {}", t.host))?
        .collect();
    let mut last = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, timeout) {
            Ok(s) => {
                s.set_read_timeout(Some(timeout))?;
                s.set_write_timeout(Some(timeout))?;
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow!(
        "sin conexión a {}:{} ({})",
        t.host,
        t.port,
        last.map(|e| e.to_string()).unwrap_or_else(|| "sin direcciones".into())
    ))
}

/// Lee `count` elementos desde `start` (un solo pedido; `count` ≤ `max_per_request`). Bits como 0/1.
pub fn read_once(s: &mut TcpStream, unit: u8, tid: u16, area: Area, start: u16, count: u16) -> Result<Vec<u16>> {
    if count == 0 || count > area.max_per_request() {
        bail!("count debe estar entre 1 y {} para {}", area.max_per_request(), area.prefix());
    }
    let fc = area.fc();
    let req = [
        (tid >> 8) as u8,
        tid as u8,
        0,
        0,
        0,
        6,
        unit,
        fc,
        (start >> 8) as u8,
        start as u8,
        (count >> 8) as u8,
        count as u8,
    ];
    s.write_all(&req).context("error al enviar el pedido Modbus")?;
    let mut head = [0u8; 7];
    s.read_exact(&mut head).context("el PLC no respondió (tiempo agotado o conexión cerrada)")?;
    let rtid = u16::from_be_bytes([head[0], head[1]]);
    let len = u16::from_be_bytes([head[4], head[5]]) as usize;
    if rtid != tid || len < 2 || len > 260 {
        bail!("respuesta Modbus inválida (transacción {rtid}, largo {len})");
    }
    let mut body = vec![0u8; len - 1];
    s.read_exact(&mut body).context("respuesta Modbus incompleta")?;
    if body[0] == fc | 0x80 {
        let code = body.get(1).copied().unwrap_or(0);
        bail!("excepción Modbus {code}: {} ({}{start}, {count} elemento(s))", exception_text(code), area.prefix());
    }
    if body[0] != fc || body.len() < 2 {
        bail!("respuesta Modbus con función {} (se esperaba {fc})", body[0]);
    }
    let data = &body[2..];
    if data.len() != body[1] as usize {
        bail!("respuesta Modbus con largo de datos inconsistente");
    }
    let n = count as usize;
    if area.is_bits() {
        if data.len() < n.div_ceil(8) {
            bail!("respuesta Modbus corta: {} bytes para {n} bits", data.len());
        }
        Ok((0..n).map(|i| u16::from((data[i / 8] >> (i % 8)) & 1)).collect())
    } else {
        if data.len() < 2 * n {
            bail!("respuesta Modbus corta: {} bytes para {n} registros", data.len());
        }
        Ok((0..n).map(|i| u16::from_be_bytes([data[2 * i], data[2 * i + 1]])).collect())
    }
}

/// Lee un rango de cualquier largo (lo parte en pedidos válidos) por una sola conexión.
pub fn read_range(t: &ModbusTarget, area: Area, start: u16, count: u32, timeout: Duration) -> Result<Vec<u16>> {
    if u32::from(start) + count > 65_536 {
        bail!("el rango {}{start} + {count} pasa de la dirección 65535", area.prefix());
    }
    let mut s = connect(t, timeout)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut addr = u32::from(start);
    let end = u32::from(start) + count;
    let mut tid: u16 = 1;
    while addr < end {
        let n = (end - addr).min(u32::from(area.max_per_request())) as u16;
        out.extend(read_once(&mut s, t.unit, tid, area, addr as u16, n)?);
        addr += u32::from(n);
        tid = tid.wrapping_add(1);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // La lectura contra un PLC falso (rangos partidos, bits, excepciones) la cubre tests/offline.rs.
    #[test]
    fn direcciones_schneider_y_solo_lectura() {
        assert_eq!(parse_address("%MW4000").unwrap(), (Area::Holding, 4000));
        assert_eq!(parse_address("%m4005").unwrap(), (Area::Coils, 4005));
        assert_eq!(parse_address("IW12").unwrap(), (Area::Input, 12));
        assert_eq!(parse_address("%I3").unwrap(), (Area::Discrete, 3));
        for (bad, msg) in [("%QW1", "usá %MWn"), ("%M", "fuera de 0..65535"), ("%MW70000", "fuera de 0..65535"), ("%MW-1", "inválida")] {
            let e = parse_address(bad).unwrap_err().to_string();
            assert!(e.contains(msg), "{bad}: {e}");
        }
        assert_eq!(Area::parse("%MW").unwrap(), Area::Holding);
        assert!(Area::parse("6").unwrap_err().to_string().contains("desconocida"));
        // El MCP solo lee el PLC: FC1 a FC4, nunca funciones de escritura (5, 6, 15, 16).
        let fcs: Vec<u8> = [Area::Coils, Area::Discrete, Area::Holding, Area::Input].iter().map(|a| a.fc()).collect();
        assert_eq!(fcs, [1, 2, 3, 4]);
    }
}
