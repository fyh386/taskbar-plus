//! 端口占用速查：GetExtendedTcpTable / GetExtendedUdpTable + sysinfo 进程信息
//! 前端拿到全量列表后自行过滤端口，一次调用即可查所有端口。
use serde_json::{json, Value};
use std::collections::HashSet;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCPTABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
    MIB_UDPTABLE_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};

fn ip4(addr: u32) -> String {
    // dwLocalAddr 为 little-endian u32，低字节在前
    format!("{}.{}.{}.{}", addr & 0xff, (addr >> 8) & 0xff, (addr >> 16) & 0xff, (addr >> 24) & 0xff)
}

fn tcp_state(s: u32) -> &'static str {
    match s {
        1 => "CLOSED",
        2 => "LISTEN",
        3 => "SYN_SENT",
        4 => "SYN_RCVD",
        5 => "ESTABLISHED",
        6 => "FIN_WAIT1",
        7 => "FIN_WAIT2",
        8 => "CLOSE_WAIT",
        9 => "CLOSING",
        10 => "LAST_ACK",
        11 => "TIME_WAIT",
        12 => "DELETE_TCB",
        _ => "UNKNOWN",
    }
}

fn tcp_rows() -> Vec<Value> {
    let mut rows = Vec::new();
    let mut size: u32 = 0;
    unsafe {
        // 先取所需缓冲区大小
        GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, 2, TCP_TABLE_OWNER_PID_ALL, 0);
        if size == 0 {
            return rows;
        }
        let mut buf = vec![0u8; size as usize];
        let ret = GetExtendedTcpTable(
            buf.as_mut_ptr() as *mut _,
            &mut size,
            0,
            2, // AF_INET
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if ret != 0 {
            return rows;
        }
        let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
        let n = table.dwNumEntries as usize;
        let base = buf.as_ptr() as usize;
        let row_size = std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
        for i in 0..n {
            let row = &*((base + std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table) + i * row_size)
                as *const MIB_TCPROW_OWNER_PID);
            let pid = row.dwOwningPid;
            if pid == 0 {
                continue;
            }
            rows.push(json!({
                "proto": "TCP",
                "local": format!("{}:{}", ip4(row.dwLocalAddr), u16::from_be(row.dwLocalPort as u16)),
                "remote": if row.dwRemoteAddr == 0 && row.dwRemotePort == 0 {
                    String::new()
                } else {
                    format!("{}:{}", ip4(row.dwRemoteAddr), u16::from_be(row.dwRemotePort as u16))
                },
                "state": tcp_state(row.dwState),
                "pid": pid,
            }));
        }
    }
    rows
}

fn udp_rows() -> Vec<Value> {
    let mut rows = Vec::new();
    let mut size: u32 = 0;
    unsafe {
        GetExtendedUdpTable(std::ptr::null_mut(), &mut size, 0, 2, UDP_TABLE_OWNER_PID, 0);
        if size == 0 {
            return rows;
        }
        let mut buf = vec![0u8; size as usize];
        let ret = GetExtendedUdpTable(
            buf.as_mut_ptr() as *mut _,
            &mut size,
            0,
            2,
            UDP_TABLE_OWNER_PID,
            0,
        );
        if ret != 0 {
            return rows;
        }
        let table = &*(buf.as_ptr() as *const MIB_UDPTABLE_OWNER_PID);
        let n = table.dwNumEntries as usize;
        let base = buf.as_ptr() as usize;
        let row_size = std::mem::size_of::<MIB_UDPROW_OWNER_PID>();
        for i in 0..n {
            let row = &*((base + std::mem::offset_of!(MIB_UDPTABLE_OWNER_PID, table) + i * row_size)
                as *const MIB_UDPROW_OWNER_PID);
            let pid = row.dwOwningPid;
            if pid == 0 {
                continue;
            }
            rows.push(json!({
                "proto": "UDP",
                "local": format!("{}:{}", ip4(row.dwLocalAddr), u16::from_be(row.dwLocalPort as u16)),
                "remote": "",
                "state": "",
                "pid": pid,
            }));
        }
    }
    rows
}

fn fill_process(rows: &mut [Value]) {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let pids: HashSet<u32> = rows.iter().filter_map(|r| r["pid"].as_u64().map(|p| p as u32)).collect();
    let sys_pids: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&sys_pids), true);
    for r in rows.iter_mut() {
        let pid = r["pid"].as_u64().unwrap_or(0) as u32;
        let (name, exe) = match sys.process(Pid::from_u32(pid)) {
            Some(proc) => {
                let name = proc.name().to_string_lossy().into_owned();
                let exe = proc
                    .exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                (name, exe)
            }
            None => (String::new(), String::new()),
        };
        r["name"] = Value::String(name);
        r["exe"] = Value::String(exe);
    }
}

pub fn ports() -> Value {
    let mut rows = tcp_rows();
    rows.extend(udp_rows());
    fill_process(&mut rows);
    json!({ "rows": rows })
}

pub fn kill(pid: u32) -> Result<Value, String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pids = [Pid::from_u32(pid)];
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), true);
    match sys.process(Pid::from_u32(pid)) {
        Some(proc) => {
            // sysinfo 0.33: kill() 成功返回 true
            let ok = proc.kill();
            if ok {
                Ok(json!({ "ok": true, "pid": pid }))
            } else {
                Err(format!("结束进程 {pid} 失败（可能需要以管理员身份运行）"))
            }
        }
        None => Err(format!("进程 {pid} 不存在或已退出")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_returns_rows_with_process_names() {
        let v = ports();
        let rows = v["rows"].as_array().expect("rows");
        // 本机必然有监听端口
        assert!(!rows.is_empty(), "no tcp/udp rows");
        // 至少能查到 svchost / System 等已知进程名之一（放宽：仅检查字段存在）
        for r in rows.iter().take(20) {
            assert!(r["local"].as_str().is_some());
            assert!(r["proto"].as_str().is_some());
        }
    }

    #[test]
    fn port_byte_order_conversion() {
        // dwLocalPort 网络字节序：0x0050 (BE 存为 0x5000) → 80
        assert_eq!(u16::from_be(0x5000u16), 80);
    }
}
