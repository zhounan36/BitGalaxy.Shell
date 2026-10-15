//! 远程指标采集：单次 exec 拉取全部指标（POSIX/Linux 目标），标记分段解析。
use bg_core::protocol::{MetricsDisk, MetricsProcess, MetricsSample};

fn mark(name: &str) -> String {
    format!("==BGSH:{name}==")
}

/// 与 Node 版逐条对齐的命令序列；各段以 ==BGSH:<name>== 标记分隔
pub fn build_metrics_command() -> String {
    format!(
        "echo {h}; hostname; echo {u}; uname -sr; echo {up}; cat /proc/uptime 2>/dev/null || echo '0 0'; \
         echo {l}; cat /proc/loadavg 2>/dev/null || echo '0 0 0 0/0 0'; \
         echo {m}; free -b 2>/dev/null | awk 'NR==2{{print $2,$3}}' || echo '0 0'; \
         echo {d}; df -B1 -P 2>/dev/null | tail -n +2 || true; \
         echo {p}; ps -eo pid,pcpu,pmem,comm --sort=-pcpu 2>/dev/null | head -n 11 || true; \
         echo {e}",
        h = mark("HOSTNAME"),
        u = mark("UNAME"),
        up = mark("UPTIME"),
        l = mark("LOAD"),
        m = mark("MEM"),
        d = mark("DF"),
        p = mark("PS"),
        e = mark("END"),
    )
}

fn section<'a>(out: &'a str, name: &str) -> &'a str {
    let start = out.find(&mark(name)).map(|i| i + mark(name).len());
    let Some(start) = start else { return "" };
    let end = out[start..].find("==BGSH:").map(|i| start + i).unwrap_or(out.len());
    out[start..end].trim_matches(|c: char| c == '\r' || c == '\n').trim()
}

fn parse_f(s: &str) -> f64 {
    s.parse().unwrap_or(0.0)
}

pub fn parse_metrics(stdout: &str) -> MetricsSample {
    let load: Vec<&str> = section(stdout, "LOAD").split_whitespace().collect();
    let mem: Vec<&str> = section(stdout, "MEM").split_whitespace().collect();

    let disks: Vec<MetricsDisk> = section(stdout, "DF")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let c: Vec<&str> = line.trim().split_whitespace().collect();
            if c.len() < 6 {
                return None;
            }
            let target = c[5].to_string();
            if target.is_empty() {
                return None;
            }
            Some(MetricsDisk {
                source: c[0].to_string(),
                target,
                size_b: parse_f(c[1]),
                used_b: parse_f(c[2]),
                used_pct: c[4].trim_end_matches('%').parse().unwrap_or(0.0),
            })
        })
        .collect();

    let processes: Vec<MetricsProcess> = section(stdout, "PS")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .skip(1)
        .map(|line| {
            let c: Vec<&str> = line.trim().split_whitespace().collect();
            MetricsProcess {
                pid: c.first().copied().unwrap_or_default().to_string(),
                cpu_pct: c.get(1).map(|s| parse_f(s)).unwrap_or(0.0),
                mem_pct: c.get(2).map(|s| parse_f(s)).unwrap_or(0.0),
                command: if c.len() > 3 { c[3..].join(" ") } else { String::new() },
            }
        })
        .collect();

    MetricsSample {
        hostname: section(stdout, "HOSTNAME")
            .lines()
            .next()
            .unwrap_or("unknown")
            .to_string(),
        kernel: section(stdout, "UNAME").lines().next().unwrap_or("").to_string(),
        uptime_sec: section(stdout, "UPTIME")
            .split_whitespace()
            .next()
            .map(|s| parse_f(s).round() as i64)
            .unwrap_or(0),
        load1: load.first().map(|s| parse_f(s)).unwrap_or(0.0),
        load5: load.get(1).map(|s| parse_f(s)).unwrap_or(0.0),
        load15: load.get(2).map(|s| parse_f(s)).unwrap_or(0.0),
        mem_total_b: mem.first().map(|s| parse_f(s)).unwrap_or(0.0),
        mem_used_b: mem.get(1).map(|s| parse_f(s)).unwrap_or(0.0),
        disks,
        processes,
        ts: bg_core::stores::now_iso(),
    }
}
