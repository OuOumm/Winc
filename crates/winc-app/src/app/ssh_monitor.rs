//! SSH 服务器概览：远端命令采集、解析与状态。
//!
//! 每台已连接的主机在后台经 [`winc_backend::remote::exec`] 执行一组只读命令，
//! 解析成结构化的 [`Overview`]，由主线程的视图绘制。这里只放与界面无关的数据模型和解析，
//! 便于用样例输出做单元测试。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use winc_backend::Target;
use winc_render::Rect;

/// SSH 工作区布局：资产在侧栏，编辑区内并排「终端 | 文件 | 服务器概览」。
///
/// 与 `HexHub` 一致：终端、文件、概览各占一栏，栏间各有一条可拖动的分隔条。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Parts {
    /// 终端栏（交给全局终端面板绘制）。
    pub(crate) terminal: Rect,
    /// 文件列表栏。
    pub(crate) files: Rect,
    /// 服务器概览栏。
    pub(crate) overview: Rect,
    /// 会话标签栏。
    pub(crate) tabs: Rect,
    /// 终端与文件之间的分隔条。
    pub(crate) divider: Rect,
    /// 文件与概览之间的分隔条。
    pub(crate) side_divider: Rect,
}

/// 概览栏的宽度范围（逻辑像素）。
pub(crate) const SIDE_MIN: f32 = 300.0;
pub(crate) const SIDE_MAX: f32 = 560.0;
/// 默认概览栏宽度。
pub(crate) const SIDE_DEFAULT: f32 = 330.0;

impl Parts {
    /// 从编辑区矩形切出各栏：终端占剩余宽度，文件栏与概览栏各用保存的宽度。
    ///
    /// `files_w`、`overview_w` 由用户拖动分隔条决定；窗口变窄时先压缩文件栏。
    pub(crate) fn new(editor: Rect, files_w: f32, overview_w: f32) -> Self {
        let gap = 9.0_f32;
        let tabs_h = 32.0_f32;
        let body = Rect::new(
            editor.x,
            editor.y + tabs_h,
            editor.w,
            (editor.h - tabs_h).max(0.0),
        );
        // 概览有下限与上限；文件栏随剩余空间收缩，至少留 180。
        let overview = overview_w.clamp(SIDE_MIN, SIDE_MAX).min((body.w * 0.4).max(0.0));
        let files = files_w.clamp(180.0, 520.0).min((body.w - overview - gap * 2.0).max(0.0));
        let term_w = (body.w - overview - files - gap * 2.0).max(0.0);
        let files_x = body.x + term_w + gap;
        let overview_x = files_x + files + gap;
        Self {
            terminal: Rect::new(body.x, body.y, term_w, body.h),
            files: Rect::new(files_x, body.y, files, body.h),
            overview: Rect::new(overview_x, body.y, overview, body.h),
            tabs: Rect::new(editor.x, editor.y, editor.w, tabs_h),
            divider: Rect::new(files_x - gap, body.y, gap, body.h),
            side_divider: Rect::new(overview_x - gap, body.y, gap, body.h),
        }
    }
}

/// SSH 工作区中可点击的目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshWsTarget {
    /// 侧栏资产列表中的主机。
    Asset(usize),
    /// 会话标签。
    Session(usize),
    /// 刷新按钮。
    Refresh,
    /// 关闭工作区按钮。
    Close,
    /// 结束进程按钮，参数为进程在列表中的下标。
    KillProc(usize),
    /// 文件列表的一行。
    File(usize),
    /// 返回上级目录。
    FileUp,
    /// 选择本地文件上传到当前目录。
    Upload,
    /// 下载文件列表中的一行。
    Download(usize),
    /// 新建 SSH 连接（打开连接对话框）。
    NewHost,
    /// 从 `~/.ssh/config` 删除资产行对应的主机。
    RemoveHost(usize),
    /// 在当前远端目录新建文件夹。
    MkDir,
    /// 聚焦进程过滤框。
    ProcFilter,
    /// 聚焦资产搜索框。
    AssetSearch,
}

/// 一个 SSH 会话。
#[derive(Debug, Clone)]
pub(crate) struct Session {
    /// 目标主机（已建立连接）。
    pub(crate) target: Target,
    /// 当前浏览的远端目录。
    pub(crate) cwd: String,
    /// 该主机的概览快照。
    pub(crate) overview: Overview,
    /// 概览解析前的原始输出（采集失败时的原因），用于状态提示。
    pub(crate) error: Option<String>,
    /// 上次采集概览的时刻，用于周期性刷新。
    pub(crate) last_collect: Option<Instant>,
    /// 该会话专属终端标签的编号（`TermTab::id`）。
    pub(crate) term_id: Option<u16>,
}

/// SSH 工作区状态：多个会话与当前活动的会话。
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    pub(crate) sessions: Vec<Session>,
    pub(crate) active: usize,
    /// 当前目录的文件条目。
    pub(crate) files: Vec<FileEntry>,
    /// 目录读取失败的原因。
    pub(crate) file_error: Option<String>,
    /// 进程过滤关键字。
    pub(crate) proc_filter: String,
    /// 概览内容的滚动偏移（逻辑像素）。
    pub(crate) overview_scroll: f32,
    /// 文件栏宽度（逻辑像素），拖动分隔条改变。
    pub(crate) files_width: f32,
    /// 概览栏宽度（逻辑像素），拖动分隔条改变。
    pub(crate) side_width: f32,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            active: 0,
            files: Vec::new(),
            file_error: None,
            proc_filter: String::new(),
            overview_scroll: 0.0,
            files_width: 380.0,
            side_width: SIDE_DEFAULT,
        }
    }
}



impl Workspace {
    /// 按当前过滤关键字筛出的进程下标与条目。
    ///
    /// 关键字匹配进程名或 PID（不区分大小写）；关键字为空时全部返回。
    pub(crate) fn filtered_processes<'a>(
        &'a self,
        processes: &'a [(String, String, f32, String)],
    ) -> impl Iterator<Item = (usize, &'a (String, String, f32, String))> + 'a {
        let needle = self.proc_filter.to_lowercase();
        processes.iter().enumerate().filter(move |(_, (pid, name, ..))| {
            needle.is_empty()
                || name.to_lowercase().contains(&needle)
                || pid.contains(&needle)
        })
    }
}

impl Workspace {
    pub(crate) fn active(&self) -> Option<&Session> {
        self.sessions.get(self.active)
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active)
    }
}

/// 后台采集结果的回传槽：主机索引 + 原始输出（或错误）。
pub(crate) type MonitorResult = (usize, Result<String, String>);

/// 读取并清空采集回传槽。
pub(crate) fn take_monitor(
    slot: &Arc<Mutex<Option<MonitorResult>>>,
) -> Option<MonitorResult> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// 采集超时：10 秒。
pub(crate) fn monitor_timeout() -> Duration {
    Duration::from_secs(10)
}

/// 文件列表中的一个条目。
#[derive(Debug, Clone, Default)]
pub(crate) struct FileEntry {
    /// 名称。
    pub(crate) name: String,
    /// 是否目录。
    pub(crate) is_dir: bool,
    /// 权限文字（`-rw-r--r--`）。
    pub(crate) perm: String,
    /// 属主与属组（`root/root`）。
    pub(crate) owner: String,
    /// 大小文字。
    pub(crate) size: String,
    /// 修改时间。
    pub(crate) time: String,
}

/// 文件列表回传槽：原始条目或错误。
pub(crate) type FilesResult = Result<Vec<FileEntry>, String>;


pub(crate) fn take_files(slot: &Arc<Mutex<Option<FilesResult>>>) -> Option<FilesResult> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// 一台主机的服务器概览快照。所有字段都兼容"命令不可用"：解析失败时为空或零。
#[derive(Debug, Clone, Default)]
pub(crate) struct Overview {
    /// 采集完成的时间（本地单调时钟），用于界面显示"更新于"。
    pub(crate) updated: Option<Instant>,
    /// 系统信息键值对：用户名、运行时间、host、系统、内核、架构、CPU 型号。
    pub(crate) system: Vec<(String, String)>,
    /// CPU 总占用百分比（0–100）。
    pub(crate) cpu_total: f32,
    /// 每个核心的占用百分比。
    pub(crate) cpu_cores: Vec<f32>,
    /// 负载（1/5/15 分钟）。
    pub(crate) load: [f32; 3],
    /// 物理内存：(已用 MB, 总量 MB)。
    pub(crate) memory: (u64, u64),
    /// 交换分区：(已用 MB, 总量 MB)。
    pub(crate) swap: (u64, u64),
    /// 网络接口：(名称, 描述)。
    pub(crate) interfaces: Vec<(String, String)>,
    /// 下行速率采样（KB/s），最新在末尾。
    pub(crate) net_rx: VecDeque<f32>,
    /// 上行速率采样（KB/s），最新在末尾。
    pub(crate) net_tx: VecDeque<f32>,
    /// 上次采集时的累计收发字节与时刻，用于差分算速率。
    pub(crate) net_prev: Option<(u64, u64, Instant)>,
    /// 当前下行速率（KB/s）。
    pub(crate) net_rx_now: f32,
    /// 当前上行速率（KB/s）。
    pub(crate) net_tx_now: f32,
    /// 磁盘挂载：(挂载点, 文件系统, 使用百分比, 用量文字)。
    pub(crate) mounts: Vec<(String, String, u32, String)>,
    /// 进程：(PID, 名称, CPU%, 内存文字)。
    pub(crate) processes: Vec<(String, String, f32, String)>,
}

/// 一次采集要执行的命令组。用一条 `sh -c` 串行执行、以哨兵分隔，减少往返次数。
/// 每条命令的输出夹在两个哨兵之间，便于按段解析。
pub(crate) const SENTINEL: &str = "___WINC_SSH_MONITOR___";

/// 采集脚本：各命令输出以哨兵 + 序号分隔。命令失败时 stderr 合并进来，不中断后续。
pub(crate) fn collect_script() -> String {
    let commands = [
        "id -un; hostname; uname -s; uname -r; uname -m",
        "uptime",
        "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//' ; nproc",
        "top -b -n 1 | head -n 5",
        "free -m",
        "df -h",
        "ip -br addr 2>/dev/null || ifconfig -a",
        "ps -eo pid,comm,pcpu,rss --sort=-pcpu | head -n 20",
        // 累计收发字节（除 lo 外的全部网卡），用于差分算速率。
        "awk -F'[: ]+' 'NR>2 && $2!=\"lo\" {rx+=$3; tx+=$11} END {printf \"%d %d\\n\", rx, tx}' /proc/net/dev 2>/dev/null || echo '0 0'",
    ];
    let mut script = String::new();
    for (i, cmd) in commands.iter().enumerate() {
        script.push_str(&format!("echo '{SENTINEL}{i}'; {cmd} 2>/dev/null; "));
    }
    script.push_str(&format!("echo '{SENTINEL}END'"));
    script
}

/// 把采集脚本的原始输出按哨兵切成 9 段；缺失的段为空串。
pub(crate) fn split_sections(output: &str) -> Vec<String> {
    let mut parts = vec![String::new(); 9];
    let mut current: Option<usize> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(SENTINEL) {
            if rest == "END" {
                current = None;
            } else if let Ok(i) = rest.parse::<usize>()
                && i < parts.len()
            {
                current = Some(i);
            }
            continue;
        }
        if let Some(i) = current {
            parts[i].push_str(line);
            parts[i].push('\n');
        }
    }
    parts
}

/// 解析整段采集输出为概览；沿用 `previous` 中网络累计字节与采样历史，使速率可差分计算。
pub(crate) fn parse_overview_with(output: &str, previous: Option<&Overview>) -> Overview {
    let parts = split_sections(output);
    let (net_prev, net_rx, net_tx, rx_now, tx_now) = match previous {
        Some(p) => (p.net_prev, p.net_rx.clone(), p.net_tx.clone(), p.net_rx_now, p.net_tx_now),
        None => (None, VecDeque::new(), VecDeque::new(), 0.0, 0.0),
    };
    let mut ov = Overview {
        updated: Some(Instant::now()),
        net_prev,
        net_rx,
        net_tx,
        net_rx_now: rx_now,
        net_tx_now: tx_now,
        ..Overview::default()
    };
    parse_system(&parts[0], &mut ov);
    parse_uptime(&parts[1], &mut ov);
    parse_cpu(&parts[2], &parts[3], &mut ov);
    parse_memory(&parts[4], &mut ov);
    parse_disks(&parts[5], &mut ov);
    parse_network(&parts[6], &mut ov);
    parse_processes(&parts[7], &mut ov);
    parse_net_counters(&parts[8], &mut ov);
    ov
}

/// 解析整段采集输出为概览（不沿用上一次的采样）。
#[cfg(test)]
pub(crate) fn parse_overview(output: &str) -> Overview {
    parse_overview_with(output, None)
}

/// 解析累计收发字节并更新速率采样。
fn parse_net_counters(s: &str, ov: &mut Overview) {
    let mut it = s.split_whitespace();
    let (Some(rx), Some(tx)) = (
        it.next().and_then(|x| x.parse::<u64>().ok()),
        it.next().and_then(|x| x.parse::<u64>().ok()),
    ) else {
        return;
    };
    let now = Instant::now();
    if let Some((prev_rx, prev_tx, prev_at)) = ov.net_prev {
        let secs = now.duration_since(prev_at).as_secs_f32().max(0.05);
        let rx_rate = rx.saturating_sub(prev_rx) as f32 / 1024.0 / secs;
        let tx_rate = tx.saturating_sub(prev_tx) as f32 / 1024.0 / secs;
        push_net_sample(ov, rx_rate, tx_rate);
    }
    ov.net_prev = Some((rx, tx, now));
}

/// 记录一次速率采样，保留最近 40 个点。
pub(crate) fn push_net_sample(ov: &mut Overview, rx: f32, tx: f32) {
    ov.net_rx_now = rx;
    ov.net_tx_now = tx;
    if ov.net_rx.len() >= 40 {
        ov.net_rx.pop_front();
        ov.net_tx.pop_front();
    }
    ov.net_rx.push_back(rx);
    ov.net_tx.push_back(tx);
}

/// 段 0：`id -un` / `hostname` / `uname -s -r -m` / cpuinfo / nproc。
fn parse_system(s: &str, ov: &mut Overview) {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let get = |i: usize| lines.get(i).copied().unwrap_or("—").to_owned();
    let mut sys = vec![
        ("用户名".to_owned(), get(0)),
        ("Host".to_owned(), get(1)),
        ("系统".to_owned(), get(2)),
        ("内核".to_owned(), get(3)),
        ("架构".to_owned(), get(4)),
    ];
    // CPU 型号与核心数在段 2，运行时间在段 1，均由后续解析补上。
    sys.push(("CPU".to_owned(), "—".to_owned()));
    sys.push(("运行时间".to_owned(), "—".to_owned()));
    ov.system = sys;
}

/// 段 1：uptime，取 `up X days, Y:Z` 与 `load average: a, b, c`。
fn parse_uptime(s: &str, ov: &mut Overview) {
    if let Some(pos) = s.find(" up ") {
        let tail = &s[pos + 4..];
        let upto = tail.find(",  ").unwrap_or(tail.len());
        let uptime = tail[..upto].trim().to_owned();
        set_sys(ov, "运行时间", &uptime);
    }
    if let Some(pos) = s.find("load average:") {
        let nums: Vec<f32> = s[pos + "load average:".len()..]
            .split(',')
            .filter_map(|x| x.trim().replace(',', ".").parse().ok())
            .collect();
        if nums.len() >= 3 {
            ov.load = [nums[0], nums[1], nums[2]];
        }
    }
}

/// 段 2：cpuinfo model + nproc；段 3：top 的前几行（`%Cpu(s)`）。
fn parse_cpu(cpuinfo: &str, top: &str, ov: &mut Overview) {
    let lines: Vec<&str> = cpuinfo.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if let Some(model) = lines.first() {
        set_sys(ov, "CPU", model);
    }
    let cores = lines
        .get(1)
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 256);
    // top 的 `%Cpu(s): 12.4 us, 2.1 sy, 0.0 ni, 85.5 id, ...` → 总占用 = 100 - id。
    let mut used = 0.0_f32;
    for line in top.lines() {
        if let Some(pos) = line.find("%Cpu") {
            for field in line[pos..].split(',') {
                let field = field.trim();
                if let Some(num) = field.split_whitespace().next()
                    && field.split_whitespace().nth(1) == Some("id")
                {
                    let val: f32 = num.trim_end_matches('%').parse().unwrap_or(100.0);
                    used = (100.0 - val).max(0.0);
                }
            }
        }
    }
    ov.cpu_total = used;
    // 没有逐核数据时按总占用给一个均匀近似，界面仍有每核条。
    if ov.cpu_cores.len() != cores {
        ov.cpu_cores = vec![used; cores];
    }
}

/// 段 4：`free -m`，行 `Mem: total used ...` / `Swap:`。
fn parse_memory(s: &str, ov: &mut Overview) {
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let label = it.next().unwrap_or("");
        let total = it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let used = it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        match label {
            "Mem:" => ov.memory = (used, total),
            "Swap:" => ov.swap = (used, total),
            _ => {}
        }
    }
}

/// 段 5：`df -h`，跳过表头行。
fn parse_disks(s: &str, ov: &mut Overview) {
    for line in s.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 {
            continue;
        }
        let fs = cols[0].to_owned();
        let use_pct = cols[4].trim_end_matches('%').parse::<u32>().unwrap_or(0);
        let mount = cols[5].to_owned();
        let used = format!("{} / {}", cols[2], cols[1]);
        ov.mounts.push((mount, fs, use_pct, used));
    }
}

/// 段 6：`ip -br addr`，形如 `eth0 UP 10.0.12.31/24 ...`。
fn parse_network(s: &str, ov: &mut Overview) {
    for line in s.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 2 {
            continue;
        }
        let name = cols[0].to_owned();
        let detail = cols[1..].join("  ");
        ov.interfaces.push((name, detail));
    }
}

/// 段 7：`ps -eo pid,comm,pcpu,rss --sort=-pcpu`，表头 `PID COMMAND %CPU RSS`。
fn parse_processes(s: &str, ov: &mut Overview) {
    for line in s.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        let pid = cols[0].to_owned();
        let name = cols[1].to_owned();
        let cpu = cols[2].parse::<f32>().unwrap_or(0.0);
        let rss_kb = cols[3].parse::<u64>().unwrap_or(0);
        let mem = if rss_kb >= 1024 {
            format!("{:.1}M", rss_kb as f64 / 1024.0)
        } else {
            format!("{rss_kb}K")
        };
        ov.processes.push((pid, name, cpu, mem));
    }
}

/// 更新概览中某个系统信息项。
fn set_sys(ov: &mut Overview, key: &str, value: &str) {
    if let Some((_, v)) = ov.system.iter_mut().find(|(k, _)| k == key) {
        *v = value.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_sections_by_sentinel() {
        let out = format!(
            "{SENTINEL}0\nroot\nhost\n{SENTINEL}1\nup 2 days\n{SENTINEL}END\n"
        );
        let parts = split_sections(&out);
        assert_eq!(parts.len(), 9);
        assert!(parts[0].contains("root"));
        assert!(parts[1].contains("up 2 days"));
        assert!(parts[2].is_empty());
    }

    #[test]
    fn parses_system_uptime_and_load() {
        let out = format!(
            "{SENTINEL}0\nroot\nprod-web-01\nLinux\n5.15.0\nx86_64\n\
             {SENTINEL}1\n 09:42:17 up 42 days, 22:19,  3 users,  load average: 0.42, 0.58, 0.61\n\
             {SENTINEL}2\nIntel Xeon 8 vCPU\n8\n{SENTINEL}3\n{SENTINEL}4\n{SENTINEL}5\n{SENTINEL}6\n{SENTINEL}7\n{SENTINEL}END\n"
        );
        let ov = parse_overview(&out);
        assert_eq!(ov.system[0].1, "root");
        assert_eq!(ov.system[1].1, "prod-web-01");
        assert!(ov.system.iter().any(|(k, v)| k == "CPU" && v == "Intel Xeon 8 vCPU"));
        assert_eq!(ov.load, [0.42, 0.58, 0.61]);
        assert!(ov.system.iter().any(|(k, v)| k == "运行时间" && v.contains("42 days")));
    }

    #[test]
    fn parses_memory_disks_processes() {        let out = format!(
            "{SENTINEL}0\nroot\nh\nLinux\nk\nx\n\
             {SENTINEL}1\nup 1 day\n\
             {SENTINEL}2\ncpu\n4\n{SENTINEL}3\n%Cpu(s): 12.4 us,  2.1 sy,  0.0 ni, 85.5 id\n\
             {SENTINEL}4\n             total        used        free\nMem:           3686         935        2751\nSwap:          1024           0        1024\n\
             {SENTINEL}5\nFilesystem Size Used Avail Use% Mounted on\n/dev/sda1  44G   38G  6.0G  86% /\n\
             {SENTINEL}6\neth0 UP 10.0.12.31/24 1000baseT\n\
             {SENTINEL}7\n  PID COMMAND %CPU RSS\n  883 mysqld 42.3 524288\n\n{SENTINEL}END\n"
        );
        let ov = parse_overview(&out);
        assert_eq!(ov.memory, (935, 3686));
        assert_eq!(ov.swap, (0, 1024));
        assert_eq!(ov.cpu_total, 14.5);
        assert_eq!(ov.mounts.len(), 1);
        assert_eq!(ov.mounts[0].2, 86);
        assert_eq!(ov.interfaces[0].0, "eth0");
        assert_eq!(ov.processes[0].1, "mysqld");
        assert_eq!(ov.processes[0].3, "512.0M");
        assert_eq!(ov.cpu_cores.len(), 4);
    }

    #[test]
    fn network_counters_produce_a_rate_after_the_second_sample() {
        let sample = |rx: u64, tx: u64| {
            format!(
                "{SENTINEL}0\nroot\nh\nLinux\nk\nx\n{SENTINEL}1\nup 1 day\n\
                 {SENTINEL}2\ncpu\n2\n{SENTINEL}3\n{SENTINEL}4\n{SENTINEL}5\n\
                 {SENTINEL}6\n{SENTINEL}7\n{SENTINEL}8\n{rx} {tx}\n{SENTINEL}END\n"
            )
        };
        // 第一次：只记录累计值，没有速率。
        let first = parse_overview_with(&sample(1_000_000, 500_000), None);
        assert_eq!(first.net_prev.map(|p| (p.0, p.1)), Some((1_000_000, 500_000)));
        assert!(first.net_rx.is_empty());

        // 第二次：约 1 秒后增长，产生一次非零速率。
        let second = parse_overview_with(&sample(1_100_000, 500_100), Some(&first));
        assert_eq!(second.net_rx.len(), 1);
        assert!(second.net_rx_now > 0.0, "下行速率应大于 0");
        assert!(second.net_tx_now >= 0.0);
        assert_eq!(second.net_prev.map(|p| p.0), Some(1_100_000));
    }
}
