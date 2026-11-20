//! 远程模块：经 `ssh` 连接其他主机，并把其上的目录作为工作区打开。
//!
//! 连接在后台线程上建立：启动系统的 `ssh`，在远端按需上传并启动服务进程（见
//! [`winc_backend::remote::Deploy`]），再取得目录在远端的真实路径。`ssh` 需要用户输入口令或
//! 确认主机密钥时，经 [`crate::askpass`] 以询问对话框提出。登录方式、主机密钥和其余设置
//! 都取自用户的 `ssh` 配置，这里不保存任何凭据。
//!
//! 模块关闭时命令不可用，不启动 `ssh`；关闭模块会断开已有的连接。

pub(crate) mod mounts;
pub(crate) mod picker;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use winc_backend::Target;
use winc_backend::remote::{self, ConnectError, Connection, Deploy, Exec, Program, Stage};
use winc_editor::git::ASKPASS_ENDPOINT;
use winc_i18n::t;

use crate::app::NoticeKind;
use crate::app::animation::Card;
use crate::app::git::form::{FormDialog, FormKind, FormPlan};
use crate::app::git::question::Asker;
use crate::app::lifecycle::notice::NoticeAction;
use crate::app::terminal::shells::ShellChoice;
use crate::app::{App, TextField, UserEvent};
use crate::askpass::{Listener, Question};
use crate::layout::DialogPart;

/// 指定服务进程所在目录的环境变量；未设置时在安装位置中查找（见 [`server_dirs`]）。
const SERVER_DIR: &str = "WINC_SERVER_DIR";

/// 服务进程的文件名前缀，其后是它所运行的系统（写法见 [`Program::system`]）。
const SERVER_PREFIX: &str = "winc-server-";

/// 把 shell 集成脚本写到远端的脚本：参数是成对的相对路径和内容，写到主目录下的
/// `winc_pty::scripts::REMOTE_DIR`。先写到旁边再改名替换：正在启动的 shell 不会读到写了一半的
/// 脚本。目录和文件只有用户本人可以访问。
const INSTALL_SHELL: &str = concat!(
    r#"umask 077; d=$HOME/$1; shift; while [ $# -gt 1 ]; do f=$d/$1; "#,
    r#"mkdir -p "${f%/*}" && printf %s "$2" > "$f.new" && mv -f "$f.new" "$f" || exit 1; "#,
    r#"shift 2; done"#,
);

/// 集成脚本已经写好的主机：在这些主机上开的终端注入 shell 集成。
static SHELL_READY: Mutex<Vec<Target>> = Mutex::new(Vec::new());

/// 各台主机上检测到的 shell，供终端的 shell 菜单列出。
static SHELLS: Mutex<Vec<(Target, Vec<ShellChoice>)>> = Mutex::new(Vec::new());

/// 列出主机上的 shell 最长等待的时间：5 秒，与查询远端环境的时限相同，超时按没有检测到处理。
const SHELLS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 返回 `target` 上检测到的 shell，登录 shell 排在最前；尚未检测完或检测失败时为空。
pub(crate) fn shells(target: &Target) -> Vec<ShellChoice> {
    let known = SHELLS.lock().unwrap_or_else(PoisonError::into_inner);
    let found = known.iter().find(|(host, _)| host == target);
    found.map(|(_, shells)| shells.clone()).unwrap_or_default()
}

/// 测试中直接记下 `target` 上的 shell，代替经连接检测；用到它的测试只在 Unix 上编译。
#[cfg(all(test, unix))]
pub(crate) fn set_shells(target: &Target, shells: Vec<ShellChoice>) {
    let mut known = SHELLS.lock().unwrap_or_else(PoisonError::into_inner);
    known.retain(|(host, _)| host != target);
    known.push((target.clone(), shells));
}

/// 列出 `target` 上的 shell 并记下来，一次往返；每次连接都重新检测。
fn detect_shells(target: &Target) {
    let mut exec = Exec::new("sh")
        .arg("-c")
        .arg(crate::app::terminal::shells::REMOTE_DETECT)
        .arg("winc");
    for path in crate::app::terminal::shells::remote_candidates() {
        exec = exec.arg(path);
    }
    let home = winc_backend::virtual_path(target, b"/");
    let deadline = std::time::Instant::now() + SHELLS_TIMEOUT;
    let output = remote::exec(&home, &exec, &|| std::time::Instant::now() >= deadline);
    let found = output.ok().map_or_else(Vec::new, |output| {
        crate::app::terminal::shells::remote_shells(&String::from_utf8_lossy(&output.stdout))
    });
    let mut known = SHELLS.lock().unwrap_or_else(PoisonError::into_inner);
    known.retain(|(host, _)| host != target);
    known.push((target.clone(), found));
}

/// 是否 `target` 上的 shell 集成脚本已经写好。
pub(crate) fn shell_ready(target: &Target) -> bool {
    let ready = SHELL_READY.lock().unwrap_or_else(PoisonError::into_inner);
    ready.contains(target)
}

/// 把 shell 集成脚本写到 `target` 上，一次往返；写不成（主目录只读）时那台主机上的终端不带
/// 集成，其余不受影响。脚本每次连接都重写：内容随本程序的版本变化。
fn install_shell_scripts(target: &Target) {
    let mut exec = Exec::new("sh")
        .arg("-c")
        .arg(INSTALL_SHELL)
        .arg("winc")
        .arg(winc_pty::scripts::REMOTE_DIR);
    for (name, content) in winc_pty::scripts::remote() {
        exec = exec.arg(name).arg(content.as_ref());
    }
    let home = winc_backend::virtual_path(target, b"/");
    let written = remote::exec(&home, &exec, &|| false).is_ok_and(|output| output.code == Some(0));
    let mut ready = SHELL_READY.lock().unwrap_or_else(PoisonError::into_inner);
    ready.retain(|known| known != target);
    if written {
        ready.push(target.clone());
    }
}

/// 取得目录在远端的真实路径的脚本，参数是用户输入的目录。
///
/// 相对路径和以 `~/` 开头的路径相对于主目录，空的输入表示主目录；`pwd -P` 给出不经过符号
/// 链接的路径。目录不存在时依次给出主目录、该目录的绝对路径和仍然存在的最近一级上级目录
/// （进不去时为主目录），各占一行，退出码为 [`MISSING`]。
const RESOLVE: &str = concat!(
    r#"p=$1; case $p in ""|"~") p=. ;; "~/"*) p=${p#"~/"} ;; esac; "#,
    r#"cd || exit 1; if [ -e "$p" ]; then cd -- "$p" && pwd -P; exit; fi; h=$(pwd -P); "#,
    r#"case $p in /*) w=$p ;; *) w=$h/$p ;; esac; printf '%s\n%s\n' "$h" "$w"; "#,
    r#"while [ ! -d "$p" ]; do p=$(dirname -- "$p"); done; "#,
    r#"cd -- "$p" 2>/dev/null || cd; pwd -P; exit 3"#,
);

/// [`RESOLVE`] 在目录不存在时的退出码。
const MISSING: i32 = 3;

/// 在远端找到的目录（虚拟路径）。
#[derive(Debug)]
enum Reached {
    /// 要打开的目录。
    Folder(PathBuf),
    /// 要打开的目录 `wanted` 已不存在：`home` 是主目录，`nearest` 是仍然存在的最近一级上级
    /// 目录。
    Missing {
        wanted: PathBuf,
        home: PathBuf,
        nearest: PathBuf,
    },
}

/// 连接成功之后做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum After {
    /// 打开取得的目录。
    Open,
    /// 以取得的目录为起点，在面板中浏览并选择要打开的目录。
    Browse,
    /// 连接中断后自动重新连接到已打开的目录：不向用户提问，失败时改为提示并提供按钮。
    Restore,
    /// 连接成功后进入 SSH 管理工作区（终端 + 文件 + 服务器概览），不打开目录为编辑器工作区。
    SshWorkspace,
}

impl Reached {
    /// 返回找到的目录；要打开的目录已不存在时返回它本身，按打开的目录被删除处理。
    fn into_folder(self) -> PathBuf {
        match self {
            Reached::Folder(folder) | Reached::Missing { wanted: folder, .. } => folder,
        }
    }
}

/// 一次连接的结果：找到的目录，或失败的原因（已是界面语言的文字）。
type Outcome = Result<Reached, String>;

/// 远程模块的状态。
pub(crate) struct RemoteState {
    /// 正在连接的主机；没有连接在进行时为 `None`。
    pub(crate) connecting: Option<Target>,
    /// 后台连接的结果，由 [`App::poll_remote`] 取走。
    outcome: Arc<Mutex<Option<Outcome>>>,
    /// `ssh` 的输入请求。
    questions: (Sender<Question>, Receiver<Question>),
    /// 用户是否关闭了本次连接的询问对话框：随后的失败按取消提示。
    pub(in crate::app) declined: bool,
    /// 是否已提示过当前目录所在主机的连接中断：重新连接之前不重复提示。
    lost: bool,
    /// 中止正在进行的连接；没有连接在进行时为 `None`。
    abort: Option<remote::Abort>,
    /// 正在进行的连接在登录之后到达的阶段，由后台写入；还在连接网络或登录时为 `None`。
    stage: Arc<Mutex<Option<Stage>>>,
    /// 正在进行的连接成功之后做什么。
    after: After,
    /// 连接对话框中填写的主机和端口；不是经对话框连接时为 `None`。对话框暂时让位给询问、
    /// 或连接失败后重新显示时按它恢复。
    form: Option<(String, String)>,
    /// 用户取消了自动的重新连接：此后不再自动重试，直到用户自己连接。
    paused: bool,
    /// 接下来对连接对话框的关闭是程序进行的（进入下一步、让位给询问），不是用户取消。
    stepping: bool,
    /// 选择面板在后台读好的目录。
    pub(in crate::app) listed: picker::Listings,
    /// 后台启动好的对远端目录的监视，由 `App::poll_remote_watcher` 取走。
    pub(in crate::app) watching: Arc<Mutex<Option<winc_editor::Watcher>>>,
    /// 正在进行的连接成功、打开目录之后要打开的文件，及参数中写的目录（见
    /// [`App::open_remote`]）；没有时为 `None`。
    files: Option<(PathBuf, Vec<crate::cli::Target>)>,
    /// SSH 资产侧栏中选中的主机下标；进入视图或列表变化时会被夹到有效范围内。
    pub(crate) asset_selection: usize,
    /// 连接对话框是「添加 SSH 连接」打开的：成功后写进 `~/.ssh/config`。
    pub(crate) adding_host: bool,
    /// 进程过滤框是否在接收键盘输入。
    pub(crate) proc_editing: bool,
    /// 资产搜索关键字。
    pub(crate) asset_filter: String,
    /// 资产搜索框是否在接收键盘输入。
    pub(crate) asset_editing: bool,
    /// SSH 工作区（终端 + 文件 + 服务器概览）；没有会话时为 `None`。
    pub(crate) ws: Option<crate::app::ssh_monitor::Workspace>,
    /// 后台采集概览的回传槽。
    pub(crate) monitor: Arc<Mutex<Option<crate::app::ssh_monitor::MonitorResult>>>,
    /// 后台读取文件列表的回传槽。
    pub(crate) ssh_files: Arc<Mutex<Option<crate::app::ssh_monitor::FilesResult>>>,
}

impl Default for RemoteState {
    fn default() -> Self {
        Self {
            connecting: None,
            outcome: Arc::default(),
            questions: channel(),
            declined: false,
            lost: false,
            abort: None,
            stage: Arc::default(),
            after: After::Open,
            form: None,
            paused: false,
            stepping: false,
            listed: picker::Listings::default(),
            watching: Arc::default(),
            files: None,
            asset_selection: 0,
            adding_host: false,
            proc_editing: false,
            asset_filter: String::new(),
            asset_editing: false,
            ws: None,
            monitor: Arc::default(),
            ssh_files: Arc::default(),
        }
    }
}

/// 项目名菜单中列出的主机：菜单项按下标引用，打开菜单时更新（见 [`refresh_hosts`]）。
static HOSTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// 列出过的主机名。菜单项的文字要求 `'static`：每个不同的名字只保留一份，数量以用户配置过
/// 的主机为限。
static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// 返回项目名菜单中列出的主机，顺序同用户的 `ssh` 配置。
pub(crate) fn hosts() -> Vec<&'static str> {
    HOSTS.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// 用户的 `ssh` 配置文件（`~/.ssh/config`）的路径；不知道主目录时为 `None`。
fn ssh_config() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".ssh").join("config"))
}

/// 配置文件 `path` 不存在时建一个空的，连同它所在的目录。
///
/// 权限按 `ssh` 的要求：目录只有用户本人可以进入，文件只有用户本人可以读写。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 配置"
)]
fn ensure_config(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let (mut dir, mut file) = (std::fs::DirBuilder::new(), std::fs::OpenOptions::new());
    dir.recursive(true);
    file.write(true).create_new(true);
    #[cfg(unix)]
    {
        std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    }
    if let Some(parent) = path.parent() {
        dir.create(parent)?;
    }
    file.open(path).map(drop)
}

/// 选密钥文件时的起始目录：用户的 `~/.ssh`；不存在时为 `None`，由系统决定。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 目录"
)]
pub(in crate::app) fn key_dir() -> Option<PathBuf> {
    ssh_config()?
        .parent()
        .filter(|dir| dir.is_dir())
        .map(Path::to_path_buf)
}

/// 把密钥文件的路径 `path` 写成输入框中的样子：主目录 `home` 以内的部分以 `~/` 开头，写法
/// 同 `ssh`。
fn key_text(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.to_string_lossy()),
        None => path.to_string_lossy().into_owned(),
    }
}

/// 是否密钥文件 `key` 存在；开头的 `~/` 按用户的主目录展开，写法同 `ssh`。无法判断时返回真。
#[allow(clippy::disallowed_methods, reason = "只访问本机：用户的密钥文件")]
fn key_exists(key: &str) -> bool {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let rest = key.strip_prefix("~/");
    let rest = rest.or_else(|| key.strip_prefix("~\\").filter(|_| cfg!(windows)));
    match (rest, home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest).is_file(),
        // 其他以 `~` 开头的写法（`~用户/…`）由 `ssh` 展开，这里无法判断，不拦。
        _ if key.starts_with('~') => true,
        _ => Path::new(key).is_file(),
    }
}

/// 重新读取用户的 `ssh` 配置（`~/.ssh/config` 及其 `Include` 引入的文件）中的主机，返回个数。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 配置"
)]
pub(crate) fn refresh_hosts() -> usize {
    let mut found = Vec::new();
    if let Some(path) = ssh_config() {
        hosts_in(&path, 0, &mut found);
    }
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    let listed: Vec<&'static str> = (found.into_iter())
        .map(|host| match names.iter().find(|known| **known == host) {
            Some(known) => *known,
            None => {
                let kept: &'static str = host.leak();
                names.push(kept);
                kept
            }
        })
        .collect();
    let count = listed.len();
    *HOSTS.lock().unwrap_or_else(PoisonError::into_inner) = listed;
    count
}

/// `Include` 嵌套的最大层数（层），同 OpenSSH 读取配置时的上限。
const INCLUDE_DEPTH: usize = 16;

/// 返回 `ssh` 配置文件 `path` 及其 `Include` 引入的各个文件中 `Host` 行列出的主机，不重复。
///
/// `Include` 的路径相对于配置文件所在的目录（`~/.ssh`），开头的 `~/` 是用户的主目录，最后
/// 一段可以带通配符；嵌套不超过 [`INCLUDE_DEPTH`] 层。读不到的文件略过。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 配置"
)]
fn hosts_in(path: &Path, depth: usize, hosts: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for host in config_hosts(&text) {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    if depth == INCLUDE_DEPTH {
        return;
    }
    let base = ssh_config().and_then(|config| config.parent().map(Path::to_path_buf));
    let home = base.as_deref().and_then(Path::parent);
    for included in arguments(&text, "include") {
        let pattern = match (included.strip_prefix("~/"), home, &base) {
            (Some(rest), Some(home), _) => home.join(rest),
            (None, _, Some(base)) => base.join(&included),
            _ => PathBuf::from(&included),
        };
        for file in matching_files(&pattern) {
            hosts_in(&file, depth + 1, hosts);
        }
    }
}

/// 返回与 `pattern` 相符的文件，按名字排序：最后一段不带通配符时就是它本身，带 `*`、`?` 时
/// 是所在目录中名字相符的各个文件。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 配置"
)]
fn matching_files(pattern: &Path) -> Vec<PathBuf> {
    let name = pattern.file_name().and_then(|name| name.to_str());
    let (Some(name), Some(dir)) = (name, pattern.parent()) else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return vec![pattern.to_path_buf()];
    }
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    let mut files: Vec<PathBuf> = entries
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|found| wild(name, found))
        })
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

/// 是否名字 `name` 与通配的写法 `pattern` 相符：`*` 是任意多个字符，`?` 是一个字符。
fn wild(pattern: &str, name: &str) -> bool {
    let (pattern, name): (Vec<char>, Vec<char>) =
        (pattern.chars().collect(), name.chars().collect());
    // `reach[i]`：写法的当前前缀能否与名字的前 `i` 个字符相符。
    let mut reach = vec![false; name.len() + 1];
    reach[0] = true;
    for symbol in pattern {
        let mut next = vec![false; name.len() + 1];
        next[0] = reach[0] && symbol == '*';
        for i in 1..=name.len() {
            next[i] = match symbol {
                '*' => reach[i] || next[i - 1],
                '?' => reach[i - 1],
                other => reach[i - 1] && other == name[i - 1],
            };
        }
        reach = next;
    }
    reach[name.len()]
}

/// 返回 `ssh` 配置 `text` 中关键字为 `keyword`（小写）的各行的参数，按出现的顺序。
fn arguments(text: &str, keyword: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        // 关键字与参数之间是空白，或一个 `=`（两侧可有空白）。
        let line = line.trim();
        let split = line.find(|c: char| c.is_whitespace() || c == '=');
        let (word, rest) = line.split_at(split.unwrap_or(line.len()));
        if !word.eq_ignore_ascii_case(keyword) {
            continue;
        }
        let rest = rest.trim_start().strip_prefix('=').unwrap_or(rest);
        // 参数以空白分隔，含空白的参数写在双引号内。
        let mut quoted = false;
        let words = rest.split(|c: char| {
            quoted ^= c == '"';
            !quoted && c.is_whitespace()
        });
        found.extend(
            words
                .map(|word| word.trim_matches('"'))
                .filter(|word| !word.is_empty())
                .map(str::to_owned),
        );
    }
    found
}

/// 返回 `ssh` 配置 `text` 中 `Host` 行列出的主机，按出现的顺序，不重复。
///
/// 只取可以直接连接的名字：带通配符（`*`、`?`）和取反（`!`）的是匹配规则，不是主机；不符合
/// [`Target`] 要求的名字也略过。
fn config_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for name in arguments(text, "host") {
        let pattern = name.contains(['*', '?', '!']);
        if !pattern && Target::new(&name).is_some() && !hosts.contains(&name) {
            hosts.push(name);
        }
    }
    hosts
}

/// 连接对话框「正在连接」时 `FormDialog::running` 的取值：它不是 Git 操作的编号，只表示
/// 按钮暂不可用。
const CONNECTING: u64 = u64::MAX;

/// 把用户输入的 `主机`、`主机:端口`、`主机:目录` 或 `主机:端口:目录` 拆成连接目标和目录；没有
/// 目录时目录为空。主机之后全是数字的一段是端口。主机或端口不符合 [`Target`] 的要求时返回
/// `None`。
fn parse_destination(input: &str) -> Option<(Target, &str)> {
    let input = input.trim();
    let Some((host, rest)) = input.split_once(':') else {
        return Some((Target::new(input)?, ""));
    };
    let (port, folder) = match rest.split_once(':') {
        Some((port, folder)) if is_port(port) => (Some(port), folder),
        None if is_port(rest) => (Some(rest), ""),
        _ => (None, rest),
    };
    let target = match port {
        Some(port) => Target::new(&format!("{host}:{port}"))?,
        None => Target::new(host)?,
    };
    Some((target, folder.trim()))
}

/// `text` 是否像端口：非空，全是数字。数值是否有效由 [`Target`] 判断。
fn is_port(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `name` 是否为「系统-架构」的写法（如 `Linux-x86_64`）：目录中名字相近的其他文件不算
/// 服务进程。
fn is_system(name: &str) -> bool {
    let word = |part: &str| {
        !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    name.split_once('-')
        .is_some_and(|(system, machine)| word(system) && word(machine))
}

/// 返回可能放有服务进程的目录，按查找的顺序。
///
/// 环境变量 [`SERVER_DIR`] 指定时只看它。否则是本程序所在的目录（Windows 的安装目录和便携版、
/// 开发时的构建目录），以及各系统安装包中放置数据文件的位置：macOS 应用包的 `Resources`，
/// Linux 上与 `bin` 并列的 `lib/winc`。
fn server_dirs() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os(SERVER_DIR) {
        return vec![PathBuf::from(dir)];
    }
    let program = std::env::current_exe().ok();
    let Some(beside) = program.as_deref().and_then(Path::parent) else {
        return Vec::new();
    };
    let mut dirs = vec![beside.to_path_buf()];
    if let Some(prefix) = beside.parent() {
        if cfg!(target_os = "macos") {
            dirs.push(prefix.join("Resources"));
        } else if cfg!(unix) {
            dirs.push(prefix.join("lib/winc"));
        }
    }
    dirs
}

/// 读取随本程序发布的各个系统的服务进程；同一个系统的程序出现在多个目录中时取先找到的。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：服务进程在本程序的安装位置中"
)]
fn programs() -> Vec<Program> {
    let entries = server_dirs()
        .into_iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten();
    let mut programs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let system = name.to_str().and_then(|n| n.strip_prefix(SERVER_PREFIX));
        let Some(system) = system.filter(|system| is_system(system)) else {
            continue;
        };
        if programs
            .iter()
            .any(|known: &Program| known.system == system)
        {
            continue;
        }
        let Ok(content) = std::fs::read(entry.path()) else {
            continue;
        };
        programs.push(Program {
            system: system.to_owned(),
            // 摘要只用来区分不同版本的程序，作为远端的目录名。
            digest: format!("{:016x}", winc_core::hash::fnv(content.iter().copied())),
            content,
        });
    }
    programs
}

/// 连接状况变化的通知：参数为真表示连接意外中断，为假表示对端有无响应的状况变了。
type Closed = Arc<dyn Fn(bool) + Send + Sync>;

/// 建立到 `target` 的连接并登记；`ssh` 的输入请求交给 `listener`，没有时不允许它提问。连接
/// 意外中断时调用 `closed`。
fn connect(
    target: &Target,
    listener: Option<&Listener>,
    closed: Option<&Closed>,
    abort: &remote::Abort,
    report: &dyn Fn(Stage),
) -> Result<(), String> {
    let deploy = Deploy {
        dir: String::new(),
        programs: programs(),
    };
    if deploy.programs.is_empty() {
        return Err(t!("workspace-remote-no-server").to_owned());
    }
    let askpass = listener.and_then(Listener::askpass);
    // 探测间隔和连接时限只在用户没有为该主机设置时添加，不覆盖用户的设置。
    let options = remote::Ssh {
        batch: askpass.is_none(),
        ..remote::Ssh::filling(target)
    };
    let mut command: Command = deploy
        .ssh_command(target, options)
        .map_err(|e| e.to_string())?;
    if let Some(askpass) = askpass {
        // `SSH_ASKPASS_REQUIRE` 使 `ssh` 不依赖 `DISPLAY` 和控制终端。
        command
            .env("SSH_ASKPASS", &askpass.program)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(ASKPASS_ENDPOINT, &askpass.endpoint);
    }
    let connection = Connection::deploy_reporting(command, &deploy, abort, report).map_err(
        |error| match error {
            ConnectError::Unsupported(system) => {
                t!("workspace-remote-unsupported-system", system = system)
            }
            other => other.to_string(),
        },
    )?;
    if let Some(closed) = closed.cloned() {
        let stalled = Arc::clone(&closed);
        connection.on_close(move || closed(true));
        connection.on_stall(move || stalled(false));
    }
    remote::register(target.clone(), connection);
    // 写集成脚本不耽误打开目录：在另一个线程上进行，写好之后开的终端才带集成。
    let host = target.clone();
    std::thread::spawn(move || {
        install_shell_scripts(&host);
        detect_shells(&host);
    });
    Ok(())
}

/// 返回 `folder` 在 `target` 上的真实路径所对应的虚拟路径；不存在时给出最近一级上级目录，
/// 不是目录或无法进入时返回原因。
fn resolve(target: &Target, folder: &str, abort: &remote::Abort) -> std::io::Result<Outcome> {
    let home = winc_backend::virtual_path(target, b"/");
    let exec = Exec::new("sh")
        .arg("-c")
        .arg(RESOLVE)
        .arg("winc")
        .arg(folder);
    let output = remote::exec(&home, &exec, &|| abort.is_aborted())?;
    let text = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let path = |bytes| winc_backend::virtual_path(target, bytes);
    Ok(match output.code {
        Some(0) => Ok(Reached::Folder(path(text))),
        Some(MISSING) => {
            let mut lines = text.splitn(3, |&b| b == b'\n');
            let mut next = || path(lines.next().unwrap_or_default());
            let (home, wanted, nearest) = (next(), next(), next());
            Ok(Reached::Missing {
                wanted,
                home,
                nearest,
            })
        }
        _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
    })
}

/// 把 `files` 中位于参数中写的目录 `wanted` 之下的路径换到远端取得的目录 `folder` 之下：
/// `folder` 是不经过符号链接的真实路径，可能与参数的写法不同。
fn rebase(files: &[crate::cli::Target], wanted: &Path, folder: &Path) -> Vec<crate::cli::Target> {
    let moved = |file: &crate::cli::Target| {
        let rest = file.path.strip_prefix(wanted).ok()?;
        Some(crate::cli::Target {
            path: folder.join(rest),
            ..file.clone()
        })
    };
    files
        .iter()
        .map(|f| moved(f).unwrap_or_else(|| f.clone()))
        .collect()
}

/// 连接 `target`（已有连接时沿用）并取得 `folder` 的虚拟路径。
fn open(
    target: &Target,
    folder: &str,
    listener: Option<&Listener>,
    closed: Option<&Closed>,
    abort: &remote::Abort,
    report: &dyn Fn(Stage),
) -> Outcome {
    let existing = remote::is_connected(target);
    if !existing {
        connect(target, listener, closed, abort, report)?;
    }
    match resolve(target, folder, abort) {
        Ok(outcome) => outcome,
        // 沿用的连接已经中断：重新连接一次。用户取消之后不再连接。
        Err(_) if existing && !abort.is_aborted() => {
            connect(target, listener, closed, abort, report)?;
            resolve(target, folder, abort).map_err(|e| e.to_string())?
        }
        Err(error) => Err(error.to_string()),
    }
}

/// 把一条 `Host` 记录追加到用户的 `~/.ssh/config`。
///
/// 已经有同名 `Host` 时不重复写入。写入前确保目录与文件的权限符合 `ssh` 的要求
/// （目录 0700、文件 0600）。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 配置"
)]
fn append_to_ssh_config(host: &str, port: Option<u16>, key: &str) -> bool {
    let Some(path) = ssh_config() else {
        return false;
    };
    if ensure_config(&path).is_err() {
        return false;
    }
    // 已有同名 Host 就不重复加。
    if let Ok(text) = std::fs::read_to_string(&path)
        && config_hosts(&text).iter().any(|h| h == host)
    {
        return true;
    }
    let mut block = String::new();
    if !std::fs::read_to_string(&path)
        .map(|t| t.trim_end().ends_with("\n\n") || t.trim().is_empty())
        .unwrap_or(true)
    {
        block.push('\n');
    }
    block.push_str(&format!("Host {host}\n"));
    block.push_str(&format!("    HostName {host}\n"));
    if let Some(port) = port {
        block.push_str(&format!("    Port {port}\n"));
    }
    if !key.trim().is_empty() {
        block.push_str(&format!("    IdentityFile {}\n", key.trim()));
    }
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, block.as_bytes()))
        .is_ok()
}

/// 从用户的 `~/.ssh/config` 中删掉 `Host` 等于 `host` 的那一段（连同其后的选项行）。
///
/// 只删第一处精确匹配的 `Host` 段；`Host a b` 这样一行多个名字时只去掉该名字，
/// 名字全部去掉时才删整段。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 配置"
)]
fn remove_from_ssh_config(host: &str) -> bool {
    let Some(path) = ssh_config() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // 新的 Host 段开始：重置跳过状态。
        if let Some(rest) = trimmed
            .strip_prefix("Host ")
            .or_else(|| trimmed.strip_prefix("host "))
        {
            if skipping {
                skipping = false;
            }
            let names: Vec<&str> = rest.split_whitespace().collect();
            if names.contains(&host) {
                changed = true;
                let kept: Vec<&str> = names.iter().copied().filter(|n| *n != host).collect();
                if kept.is_empty() {
                    // 整段删掉：跳过本行及后续选项行。
                    skipping = true;
                    continue;
                }
                out.push_str(&format!("Host {}\n", kept.join(" ")));
                continue;
            }
        }
        if skipping {
            // 选项行（缩进）随段跳过；空行或注释也一并跳过，直到下一个 Host。
            if trimmed.is_empty() || trimmed.starts_with('#') || line.starts_with(' ') || line.starts_with('\t') {
                continue;
            }
            skipping = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !changed {
        return false;
    }
    std::fs::write(&path, out).is_ok() && refresh_hosts() > 0
}

/// 读取远端目录 `dir`（虚拟路径）的条目，解析 `ls -lA` 的输出。
///
/// 列序：权限、链接数、属主、属组、大小、日期、时间、名称。
fn read_remote_dir(dir: &std::path::Path) -> Result<Vec<crate::app::ssh_monitor::FileEntry>, String> {
    use crate::app::ssh_monitor;
    let deadline = std::time::Instant::now() + ssh_monitor::monitor_timeout();
    let exec = Exec::new("ls")
        .arg("-lA")
        .arg("--time-style=+%Y-%m-%d %H:%M")
        .limit(1 << 20);
    let out = remote::exec(dir, &exec, &|| std::time::Instant::now() >= deadline)
        .map_err(|e| e.to_string())?;
    if out.code != Some(0) {
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(if err.is_empty() {
            format!("ls 退出码 {:?}", out.code)
        } else {
            err
        });
    }
    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 {
            continue;
        }
        let perm = cols[0].to_owned();
        let is_dir = perm.starts_with('d');
        let owner = format!("{}/{}", cols[2], cols[3]);
        let size = cols[4].to_owned();
        // 时间是倒数第 3、2 列，名称是最后一列（可能含空格，取剩余部分）。
        let time = format!("{} {}", cols[cols.len() - 3], cols[cols.len() - 2]);
        let name_start = line
            .match_indices(&format!("{} {}", cols[cols.len() - 3], cols[cols.len() - 2]))
            .next()
            .map(|(i, m)| i + m.len())
            .unwrap_or(0);
        let name = line[name_start..].trim().to_owned();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        entries.push(ssh_monitor::FileEntry {
            name,
            is_dir,
            perm,
            owner,
            size,
            time,
        });
    }
    entries.sort_by(|a, b| {
        (b.is_dir, a.name.to_lowercase()).cmp(&(a.is_dir, b.name.to_lowercase()))
    });
    Ok(entries)
}

/// 从 `~/.ssh/config` 查出主机 `host` 的连接信息：(用户, 地址, 端口)。
///
/// 用于资产列表的地址副行。`HostName` 缺省时用别名本身；用户和端口读不到时为 `None`。
#[allow(
    clippy::disallowed_methods,
    reason = "只访问本机：用户主目录中的 ssh 配置"
)]
pub(crate) fn host_endpoint(host: &str) -> Option<(Option<String>, String, Option<u16>)> {
    let text = std::fs::read_to_string(ssh_config()?).ok()?;
    let mut user = None;
    let mut addr = host.to_owned();
    let mut port = None;
    let mut matched = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("Host ")
            .or_else(|| trimmed.strip_prefix("host "))
        {
            // 命中目标主机的那一段才开始收集；遇到下一段停止。
            if matched {
                break;
            }
            matched = rest.split_whitespace().any(|n| n == host);
            continue;
        }
        if !matched {
            continue;
        }
        let (key, value) = match trimmed.split_once(char::is_whitespace) {
            Some(pair) => (pair.0.to_lowercase(), pair.1.trim()),
            None => continue,
        };
        match key.as_str() {
            "hostname" if !value.is_empty() => addr = value.to_owned(),
            "user" if !value.is_empty() => user = Some(value.to_owned()),
            "port" => port = value.parse::<u16>().ok(),
            _ => {}
        }
    }
    Some((user, addr, port))
}

/// 本机下载目录：用户主目录下的 `Downloads`，没有时回退主目录或临时目录。
fn download_dir() -> std::path::PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(std::path::PathBuf::from);
    home.as_ref()
        .map(|h| h.join("Downloads"))
        .filter(|p| winc_backend::fs::is_dir(p))
        .or_else(|| home.clone())
        .unwrap_or_else(std::env::temp_dir)
}

impl App {
    /// 远程模块是否启用。
    pub(crate) fn remote_enabled(&self) -> bool {
        self.config.remote_enabled.unwrap_or(false)
    }

    /// SSH 资产侧栏当前列出的主机（`~/.ssh/config` 中的 `Host`）。
    ///
    /// 首次进入视图时由 [`App::ssh_refresh`] 读取一次；这里只返回已记下的名字。
    pub(crate) fn ssh_assets(&self) -> Vec<String> {
        let mut list: Vec<String> = hosts().into_iter().map(str::to_owned).collect();
        // 本次连过、但配置里还没有的主机也列出来，避免用户手动连上后列表里找不到。
        for session in self.remote.ws.as_ref().map(|w| w.sessions.as_slice()).unwrap_or(&[]) {
            let name = session.target.as_str();
            if !list.iter().any(|h| h == name) {
                list.push(name.to_owned());
            }
        }
        list
    }

    /// 重新读取 `~/.ssh/config`，并把选中项夹到有效范围。返回主机个数。
    pub(in crate::app) fn ssh_refresh(&mut self) -> usize {
        let count = refresh_hosts();
        if self.remote.asset_selection >= count && count > 0 {
            self.remote.asset_selection = count - 1;
        }
        if count == 0 {
            self.remote.asset_selection = 0;
        }
        self.request_redraw();
        count
    }

    /// 从 SSH 资产列表连接选中的主机，进入 SSH 管理工作区。
    pub(crate) fn ssh_connect_selected(&mut self) {
        let Some(host) = hosts().into_iter().nth(self.remote.asset_selection).map(str::to_owned)
        else {
            return;
        };
        self.ssh_connect_host(&host);
    }

    /// 连接主机 `host`（若已连接则沿用），完成后进入 SSH 管理工作区。
    pub(crate) fn ssh_connect_host(&mut self, host: &str) {
        let Some(target) = Target::new(host) else {
            self.flash(t!("workspace-ssh-invalid-host", host = host));
            return;
        };
        // 已连接：直接进入工作区，无需再走一遍连接流程。
        if remote::is_connected(&target) {
            let home = winc_backend::native_lossy(&winc_backend::virtual_path(&target, b"/"))
                .into_owned();
            self.ssh_workspace_connected(target, home);
            return;
        }
        self.remote.form = None;
        // SSH 面板发起的连接一律进入管理工作区，避免沿用上一次残留的 After。
        self.remote.after = After::SshWorkspace;
        self.remote_open(target, String::new(), After::SshWorkspace);
    }

    /// 连接成功后建立会话、打开远端终端并开始采集概览。
    pub(crate) fn ssh_workspace_connected(&mut self, target: Target, home: String) {
        use crate::app::ssh_monitor::Session;

        // 从「添加 SSH 连接」进来：把主机写进 ~/.ssh/config，之后资产列表里就能看到。
        if std::mem::take(&mut self.remote.adding_host) {
            let port = target.port();
            let key = self
                .state
                .remote_key(target.as_str())
                .unwrap_or_default()
                .to_owned();
            if append_to_ssh_config(target.as_str(), port, &key) {
                self.ssh_refresh();
            }
        }

        // 同一主机已有会话则激活它，否则新建。
        let ws = self.remote.ws.get_or_insert_with(Default::default);
        let existing = ws.sessions.iter().position(|s| s.target == target);
        let active = match existing {
            Some(i) => {
                ws.active = i;
                i
            }
            None => {
                ws.sessions.push(Session {
                    target: target.clone(),
                    cwd: home.clone(),
                    overview: Default::default(),
                    error: None,
                    last_collect: None,
                    term_id: None,
                });
                ws.active = ws.sessions.len() - 1;
                ws.active
            }
        };
        ws.files.clear();
        let _ = home;

        // 每个会话一个专属终端：已有标签就激活它，没有就新建并记下编号。
        self.ssh_switch_terminal(active);
        // 焦点放进终端：连上之后直接可以敲命令、回答密码提示。
        self.focus = crate::app::Focus::Terminal;

        // 读取初始目录并采集概览。
        self.ssh_read_files();
        self.ssh_collect(active);
        self.request_redraw();
    }

    /// 切换到会话 `index` 的专属终端：已有标签则激活，没有则新建并记下编号。
    ///
    /// 先按会话记下的 `term_id` 查找；`term_id` 还没有时按 `cwd`（该会话主机虚拟路径根）
    /// 认领一次。标签的 `cwd` 会被 shell 的 OSC 7 改写，因此只作为初次认领的手段。
    pub(crate) fn ssh_switch_terminal(&mut self, index: usize) {
        let Some((target, home, known_id)) = self
            .remote
            .ws
            .as_ref()
            .and_then(|ws| ws.sessions.get(index))
            .map(|s| (s.target.clone(), s.cwd.clone(), s.term_id))
        else {
            return;
        };
        let cwd = winc_backend::virtual_path(&target, home.as_bytes())
            .to_string_lossy()
            .into_owned()
            .into_bytes();

        // 1) 已有标签：优先按记下的 id，其次按 cwd 认领一次。
        let found = known_id
            .and_then(|id| self.terminal.tabs.iter().position(|t| t.panes().any(|p| p.id == id)))
            .or_else(|| self.terminal.tabs.iter().position(|t| t.cwd == cwd));
        if let Some(term) = found {
            self.terminal.active = term;
            let id = self.terminal.tabs[term].id;
            if let Some(session) = self
                .remote
                .ws
                .as_mut()
                .and_then(|ws| ws.sessions.get_mut(index))
            {
                session.term_id = Some(id);
            }
            self.request_redraw();
            return;
        }

        // 还没有：新建一个（异步进入 want 队列，建好后 cwd 即为该虚拟路径）。
        let shell = shells(&target).first().map(|s| s.command.clone());
        // 清掉拆分目标，避免这个终端被并进别的标签。
        self.terminal.split_into = None;
        self.new_terminal_tab_with(cwd, shell);
    }

    /// 关闭 SSH 工作区（断开会话、回到普通编辑区）。
    pub(crate) fn ssh_close_workspace(&mut self) {
        if let Some(ws) = self.remote.ws.take() {
            for session in ws.sessions {
                remote::disconnect(&session.target);
            }
        }
        self.request_redraw();
    }

    /// 在后台采集当前活动主机的服务器概览。
    pub(crate) fn ssh_collect(&mut self, index: usize) {
        use crate::app::ssh_monitor;
        let Some(target) = self
            .remote
            .ws
            .as_ref()
            .and_then(|ws| ws.sessions.get(index))
            .map(|s| s.target.clone())
        else {
            return;
        };
        let slot = Arc::clone(&self.remote.monitor);
        self.spawn_blocking_then(UserEvent::SshMonitorReady, move || {
            let dir = winc_backend::virtual_path(&target, b"/");
            let deadline = std::time::Instant::now() + ssh_monitor::monitor_timeout();
            let exec = Exec::new("sh")
                .arg("-c")
                .arg(ssh_monitor::collect_script())
                .limit(1 << 20);
            let result = remote::exec(&dir, &exec, &|| std::time::Instant::now() >= deadline)
                .map_err(|e| e.to_string())
                .and_then(|out| {
                    if out.code == Some(0) {
                        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
                    } else {
                        let err = String::from_utf8_lossy(&out.stderr).into_owned();
                        Err(if err.is_empty() {
                            format!("命令退出码 {:?}", out.code)
                        } else {
                            err
                        })
                    }
                });
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some((index, result));
        });
    }

    /// 上传本地文件（或目录）`local` 到当前会话目录。
    ///
    /// 走文件树既有的复制通道：流式传输、显示进度、可取消、目录递归、重名自动改名，
    /// 并保留 Unix 权限位。
    pub(crate) fn ssh_upload(&mut self, local: std::path::PathBuf) {
        let Some(session) = self.remote.ws.as_ref().and_then(|ws| ws.active()) else {
            return;
        };
        let dir = winc_backend::virtual_path(&session.target, session.cwd.as_bytes());
        self.copy_entries_into(vec![local], dir);
    }

    /// 下载当前会话目录中名为 `name` 的条目到本机下载目录。
    ///
    /// 同样走复制通道：有进度、可取消、支持目录，重名不覆盖（改名为副本）。
    pub(crate) fn ssh_download(&mut self, name: String) {
        let Some(session) = self.remote.ws.as_ref().and_then(|ws| ws.active()) else {
            return;
        };
        let dir = winc_backend::virtual_path(&session.target, session.cwd.as_bytes());
        let remote = winc_backend::join(&dir, &name);
        let local = download_dir();
        self.download(vec![remote], local);
    }

    /// SSH 过滤框（资产搜索 / 进程过滤）的按键：可打印字符追加、退格删除、回车/Esc 结束。
    ///
    /// 返回是否已消费该按键；未消费时交回常规按键处理。
    pub(crate) fn ssh_filter_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        use winit::event::ElementState;
        use winit::keyboard::{Key, NamedKey};
        // 当前接收输入的是哪一个框。
        let target = if self.remote.asset_editing {
            0
        } else if self.remote.proc_editing {
            1
        } else {
            return false;
        };
        match &event.logical_key {
            Key::Character(text) => {
                if event.state != ElementState::Pressed {
                    return true;
                }
                self.ssh_filter_push(target, text);
                true
            }
            Key::Named(NamedKey::Backspace) => {
                if event.state == ElementState::Pressed {
                    self.ssh_filter_pop(target);
                }
                true
            }
            Key::Named(NamedKey::Enter | NamedKey::Escape) => {
                self.remote.asset_editing = false;
                self.remote.proc_editing = false;
                self.request_redraw();
                true
            }
            _ => false,
        }
    }

    /// 往过滤框 `target`（0 资产、1 进程）追加文字。
    fn ssh_filter_push(&mut self, target: u8, text: &str) {
        match target {
            0 => {
                self.remote.asset_filter.push_str(text);
                // 过滤后选中项可能越界，回到第一项。
                self.remote.asset_selection = 0;
            }
            _ => {
                if let Some(ws) = self.remote.ws.as_mut() {
                    ws.proc_filter.push_str(text);
                }
            }
        }
        self.request_redraw();
    }

    /// 从过滤框 `target` 删除最后一个字符。
    fn ssh_filter_pop(&mut self, target: u8) {
        match target {
            0 => {
                self.remote.asset_filter.pop();
                self.remote.asset_selection = 0;
            }
            _ => {
                if let Some(ws) = self.remote.ws.as_mut() {
                    ws.proc_filter.pop();
                }
            }
        }
        self.request_redraw();
    }

    /// 处理 SSH 工作区中的点击。
    pub(crate) fn press_ssh_ws(&mut self, target: crate::app::ssh_monitor::SshWsTarget) {
        use crate::app::ssh_monitor::SshWsTarget;
        match target {
            SshWsTarget::Asset(i) => {
                self.remote.asset_selection = i;
                self.ssh_connect_selected();
            }
            SshWsTarget::KillProc(index) => {
                let Some((target, pid)) = self
                    .remote
                    .ws
                    .as_ref()
                    .and_then(|ws| ws.active())
                    .and_then(|s| {
                        s.overview
                            .processes
                            .get(index)
                            .map(|p| (s.target.clone(), p.0.clone()))
                    })
                else {
                    return;
                };
                let slot = Arc::clone(&self.remote.monitor);
                let active = self.remote.ws.as_ref().map(|ws| ws.active).unwrap_or(0);
                self.spawn_blocking_then(UserEvent::SshMonitorReady, move || {
                    let dir = winc_backend::virtual_path(&target, b"/");
                    // 先结束进程，再重新采集概览，一次回传完成刷新。
                    let kill = Exec::new("kill").arg("-9").arg(&pid).limit(4096);
                    let _ = remote::exec(&dir, &kill, &|| false);
                    let deadline = std::time::Instant::now() + crate::app::ssh_monitor::monitor_timeout();
                    let exec = Exec::new("sh")
                        .arg("-c")
                        .arg(crate::app::ssh_monitor::collect_script())
                        .limit(1 << 20);
                    let result = remote::exec(&dir, &exec, &|| std::time::Instant::now() >= deadline)
                        .map_err(|e| e.to_string())
                        .and_then(|out| {
                            if out.code == Some(0) {
                                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
                            } else {
                                Err(String::from_utf8_lossy(&out.stderr).into_owned())
                            }
                        });
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some((active, result));
                });
            }
            SshWsTarget::NewHost => {
                // 打开连接对话框：填主机、端口、密钥，连上后写进 ~/.ssh/config。
                self.remote.adding_host = true;
                self.open_connect_dialog("", "", false);
            }
            SshWsTarget::RemoveHost(i) => {
                let host = self.ssh_assets().into_iter().nth(i);
                if let Some(host) = host {
                    let removed = remove_from_ssh_config(&host);
                    self.ssh_refresh();
                    if removed {
                        self.flash(t!("workspace-ssh-removed", host = host));
                    }
                }
            }
            SshWsTarget::MkDir => {
                // 在当前远端目录新建一个文件夹（默认名 new-folder，重名时递增），完成后刷新列表。
                let Some(session) = self.remote.ws.as_ref().and_then(|ws| ws.active()) else {
                    return;
                };
                let target = session.target.clone();
                let dir = session.cwd.clone();
                let slot = Arc::clone(&self.remote.ssh_files);
                self.spawn_blocking_then(UserEvent::SshFilesReady, move || {
                    let base = winc_backend::virtual_path(&target, dir.as_bytes());
                    for i in 0..100 {
                        let name = if i == 0 {
                            "new-folder".to_owned()
                        } else {
                            format!("new-folder-{i}")
                        };
                        match winc_backend::fs::create_dir(winc_backend::join(&base, &name)) {
                            Ok(()) => break,
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                            Err(_) => break,
                        }
                    }
                    // 建完立刻读一次目录，结果交给主线程。
                    let result = read_remote_dir(&base);
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
                });
            }
            SshWsTarget::AssetSearch => {
                self.remote.asset_editing = true;
                self.remote.proc_editing = false;
                self.request_redraw();
            }
            SshWsTarget::ProcFilter => {
                // 聚焦进程过滤框：之后键入的字符进入过滤关键字。
                self.remote.proc_editing = true;
                self.remote.asset_editing = false;
                self.request_redraw();
            }
            SshWsTarget::ToggleOverview => {
                if let Some(ws) = self.remote.ws.as_mut() {
                    ws.overview_open = !ws.overview_open;
                }
                self.relayout();
                self.request_redraw();
            }
            SshWsTarget::Close => self.ssh_close_workspace(),
            SshWsTarget::Session(i) => {
                let valid = self.remote.ws.as_ref().is_some_and(|ws| i < ws.sessions.len());
                if valid {
                    self.remote.ws.as_mut().unwrap().active = i;
                    // 切到该会话自己的终端。
                    self.ssh_switch_terminal(i);
                    self.ssh_read_files();
                    self.ssh_collect(i);
                    self.request_redraw();
                }
            }
            SshWsTarget::Refresh => {
                if let Some(i) = self.remote.ws.as_ref().map(|ws| ws.active) {
                    self.ssh_read_files();
                    self.ssh_collect(i);
                }
            }
            SshWsTarget::Upload => {
                // 选择本地文件后上传到当前目录。
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    self.ssh_upload(path);
                }
            }
            SshWsTarget::Download(i) => {
                let name = self
                    .remote
                    .ws
                    .as_ref()
                    .and_then(|ws| ws.files.get(i))
                    .map(|f| f.name.clone());
                if let Some(name) = name {
                    self.ssh_download(name);
                }
            }
            SshWsTarget::FileUp => {
                if let Some(session) = self.remote.ws.as_mut().and_then(|ws| ws.active_mut()) {
                    session.cwd = if session.cwd == "/" || session.cwd.is_empty() {
                        "/".to_owned()
                    } else {
                        match session.cwd.rfind('/') {
                            Some(0) | None => "/".to_owned(),
                            Some(i) => session.cwd[..i].to_owned(),
                        }
                    };
                }
                self.ssh_read_files();
                self.request_redraw();
            }
            SshWsTarget::File(i) => {
                // 目录：双击进入（这里单击即进入，原型交互简化）；文件：打开到编辑器。
                let entry = self
                    .remote
                    .ws
                    .as_ref()
                    .and_then(|ws| ws.files.get(i))
                    .cloned();
                let Some(entry) = entry else {
                    return;
                };
                let (name, is_dir) = (entry.name, entry.is_dir);
                if is_dir {
                    if let Some(session) = self.remote.ws.as_mut().and_then(|ws| ws.active_mut()) {
                        session.cwd = if session.cwd == "/" {
                            format!("/{name}")
                        } else {
                            format!("{}/{name}", session.cwd)
                        };
                    }
                    self.ssh_read_files();
                    self.request_redraw();
                } else if let Some(session) = self.remote.ws.as_ref().and_then(|ws| ws.active()) {
                    let path = winc_backend::virtual_path(
                        &session.target,
                        format!("{}/{}", session.cwd, name).as_bytes(),
                    );
                    self.open_path(&path);
                }
            }
        }
    }

    /// 在后台读取当前目录的文件列表。
    pub(crate) fn ssh_read_files(&mut self) {
        let Some(session) = self.remote.ws.as_ref().and_then(|ws| ws.active()) else {
            return;
        };
        let target = session.target.clone();
        let dir_name = session.cwd.clone();
        let slot = Arc::clone(&self.remote.ssh_files);
        self.spawn_blocking_then(UserEvent::SshFilesReady, move || {
            let dir = winc_backend::virtual_path(&target, dir_name.as_bytes());
            let result = read_remote_dir(&dir);
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
        });
    }

    /// 主线程取回文件列表。
    pub(crate) fn poll_ssh_files(&mut self) {
        use crate::app::ssh_monitor;
        let Some(result) = ssh_monitor::take_files(&self.remote.ssh_files) else {
            return;
        };
        if let Some(ws) = self.remote.ws.as_mut() {
            match result {
                Ok(files) => {
                    ws.files = files;
                    ws.file_error = None;
                }
                Err(error) => {
                    ws.files.clear();
                    ws.file_error = Some(error);
                }
            }
        }
        self.request_redraw();
    }

    /// 主线程取回采集结果，解析成概览。
    pub(crate) fn poll_ssh_monitor(&mut self) {
        use crate::app::ssh_monitor;
        let Some((index, result)) = ssh_monitor::take_monitor(&self.remote.monitor) else {
            return;
        };
        if let Some(session) = self
            .remote
            .ws
            .as_mut()
            .and_then(|ws| ws.sessions.get_mut(index))
        {
            match result {
                Ok(text) => {
                    // 沿用上一份概览中的网络累计字节，使速率能差分计算。
                    session.overview =
                        ssh_monitor::parse_overview_with(&text, Some(&session.overview));
                    session.error = None;
                    session.last_collect = Some(std::time::Instant::now());
                }
                Err(error) => session.error = Some(error),
            }
        }
        self.request_redraw();
    }

    /// 连接主机 `host`（项目名菜单中列出的主机），连接后在面板中浏览其上的目录。
    pub(in crate::app) fn browse_remote_host(&mut self, host: &str) {
        self.open_connect_dialog(host, "", true);
    }

    /// 打开连接远端主机的对话框，填好主机 `host` 和端口 `port`（可以为空）；`start` 为真且
    /// 填写的内容有效时立即开始连接。
    ///
    /// 连接期间对话框一直在：按钮显示「正在连接…」，失败的原因写在对话框里，改了再连；成功
    /// 后进入选择目录的面板。取消回到主机面板。
    pub(in crate::app) fn open_connect_dialog(&mut self, host: &str, port: &str, start: bool) {
        self.remote.form = Some((host.to_owned(), port.to_owned()));
        // 已有的对话框（例如上一次的）由这一个替换，不算用户取消。
        if matches!(
            self.overlays.form_dialog.as_ref().map(|d| &d.kind),
            Some(FormKind::Connect)
        ) {
            self.remote.stepping = true;
            self.close_form_dialog();
        }
        let mut name = TextField::default();
        name.insert(host);
        self.show_form_dialog(FormKind::Connect, name, DialogPart::Branch);
        // 为这台主机指定过的密钥文件。
        let key = (Target::with_port(host, port.parse().ok()))
            .and_then(|target| self.state.remote_key(target.as_str()))
            .unwrap_or_default()
            .to_owned();
        if let Some(dialog) = &mut self.overlays.form_dialog {
            dialog.second = TextField::with_filter(|c| c.is_ascii_digit());
            dialog.second.insert(port);
            dialog.third.insert(&key);
        }
        if start {
            self.submit_form_dialog();
        }
    }

    /// 连接对话框按当前填写的内容能不能连接，以及主按钮上的字。
    pub(in crate::app) fn connect_plan(&self, dialog: &FormDialog) -> FormPlan {
        let (host, port) = (dialog.name.text().trim(), dialog.second.text().trim());
        let key = dialog.third.text().trim();
        let problem = if !host.is_empty() && Target::with_port(host, None).is_none() {
            Some(t!("workspace-remote-dialog-invalid-host"))
        } else if !port.is_empty() && Target::with_port("host", port.parse().ok()).is_none() {
            Some(t!("workspace-remote-dialog-invalid-port"))
        } else if !key.is_empty() && !key_exists(key) {
            // `ssh` 对不存在的密钥文件只是跳过，随后的失败看不出原因：在这里先说明。
            Some(t!("workspace-remote-dialog-missing-key"))
        } else {
            None
        };
        FormPlan {
            enabled: !host.is_empty() && problem.is_none(),
            problem: problem.map(str::to_owned),
            confirm: match (dialog.running, self.remote_stage()) {
                (None, _) => t!("workspace-remote-dialog-connect"),
                (Some(_), None) => t!("workspace-remote-dialog-connecting"),
                (Some(_), Some(Stage::Uploading)) => t!("workspace-remote-dialog-installing"),
                (Some(_), Some(Stage::Starting)) => t!("workspace-remote-dialog-starting"),
            },
            base: String::new(),
            base_enabled: false,
            dir: None,
        }
    }

    /// 提交连接对话框：开始连接，对话框留着显示进度。
    pub(in crate::app) fn submit_connect(&mut self) {
        let Some(dialog) = &mut self.overlays.form_dialog else {
            return;
        };
        let (host, port, key) = (
            dialog.name.text().trim().to_owned(),
            dialog.second.text().trim().to_owned(),
            dialog.third.text().trim().to_owned(),
        );
        let Some(target) = Target::with_port(&host, port.parse().ok().filter(|_| !port.is_empty()))
        else {
            return;
        };
        dialog.running = Some(CONNECTING);
        dialog.error = None;
        // 密钥文件按主机记下：此后经最近的目录、重新连接和终端登录这台主机时都用它。
        if self.state.remote_key(target.as_str()).unwrap_or_default() != key {
            self.update_state(|state| state.set_remote_key(target.as_str(), &key));
        }
        self.remote.form = Some((host, port));
        // 从 SSH 面板「新建」进来时进入管理工作区；否则是编辑器原有的「打开远端目录」流程。
        let after = if self.remote.adding_host {
            After::SshWorkspace
        } else {
            After::Browse
        };
        self.remote_open(target, String::new(), after);
        self.request_redraw();
    }

    /// 在编辑器中打开用户的 `ssh` 配置文件，没有时先建一个空的：主机的别名、密钥、跳板机等
    /// 都在其中设置，对本程序和终端里的 `ssh` 同样生效。
    pub(in crate::app) fn open_ssh_config(&mut self) {
        let Some(path) = ssh_config() else {
            return;
        };
        match ensure_config(&path) {
            Ok(()) => self.open_path(&path),
            Err(error) => self.notify(
                NoticeKind::Error,
                t!("workspace-remote-config-failed", error = error.to_string()),
            ),
        }
    }

    /// 在系统的面板中选好了密钥文件：填进连接对话框的密钥一行，焦点移到那里。
    pub(in crate::app) fn key_file_picked(&mut self, path: &Path) {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
        let text = key_text(path, home.as_deref().map(Path::new));
        let dialog =
            (self.overlays.form_dialog.as_mut()).filter(|d| matches!(d.kind, FormKind::Connect));
        if let Some(dialog) = dialog {
            dialog.third.set_text("");
            dialog.third.insert(&text);
            dialog.focus = DialogPart::Third;
            dialog.error = None;
        }
        self.request_redraw();
    }

    /// 连接对话框即将被程序关闭（进入下一步、让位给询问）：这次关闭不算用户取消。
    pub(in crate::app) fn connect_dialog_steps_aside(&mut self) {
        let shown = self
            .overlays
            .form_dialog
            .as_ref()
            .map(|dialog| &dialog.kind);
        self.remote.stepping = matches!(shown, Some(FormKind::Connect));
    }

    /// 连接对话框关闭时调用。用户取消（关闭按钮、`Esc`、「取消」）时中止正在进行的连接，回到
    /// 主机面板；程序自己关闭时不做任何事。
    pub(in crate::app) fn connect_dialog_closed(&mut self) {
        if std::mem::take(&mut self.remote.stepping) {
            return;
        }
        self.remote.form = None;
        self.cancel_remote();
        self.open_remote_picker();
    }

    /// 询问答复之后回到连接对话框：连接仍在进行时按「正在连接」显示。
    pub(in crate::app) fn restore_connect_dialog(&mut self) {
        let Some((host, port)) = self.remote.form.clone() else {
            return;
        };
        if self.remote.connecting.is_none() {
            return;
        }
        self.open_connect_dialog(&host, &port, false);
        if let Some(dialog) = &mut self.overlays.form_dialog {
            dialog.running = Some(CONNECTING);
        }
        self.turn_card(Card::Dialog);
    }

    /// 最近列表中的目录 `folder` 在远端主机上时，在后台连接（已有连接时沿用）并打开，返回真；
    /// 本机的目录返回假，由调用方按通常的方式打开。
    ///
    /// 远端的目录是否存在要向远端查询，不能在界面线程上判断。
    pub(in crate::app) fn reopen_remote(&mut self, folder: &Path) -> bool {
        let Some(location) = winc_backend::locate(folder) else {
            return false;
        };
        if self.remote_enabled() {
            let path = String::from_utf8_lossy(&location.path).into_owned();
            self.remote_open(location.target, path, After::Open);
        } else {
            self.notify(NoticeKind::Warning, t!("workspace-remote-disabled"));
        }
        true
    }

    /// 打开 `ssh://` 参数指定的远端项目 `open` 并切到前台：本窗口开着该目录时直接打开其中的
    /// 文件；否则在后台连接（已有连接时沿用），打开目录之后再打开文件，带行号的跳到该位置。
    pub(crate) fn open_remote(&mut self, open: crate::cli::RemoteOpen) {
        self.raise();
        if self.editor.root() == Some(open.folder.as_path()) {
            self.open_files(&open.files);
            return;
        }
        if !self.remote_enabled() {
            self.notify(NoticeKind::Warning, t!("workspace-remote-disabled"));
            return;
        }
        let Some(at) = winc_backend::locate(&open.folder) else {
            return;
        };
        // 已有连接在进行时 `remote_open` 只提示，这次的文件不记下。
        if self.remote.connecting.is_none() {
            self.remote.files = Some((open.folder, open.files));
        }
        let path = String::from_utf8_lossy(&at.path).into_owned();
        self.remote_open(at.target, path, After::Open);
    }

    /// 启动时调用：上次退出时开着的目录在远端主机上时，重新连接并打开它，标签按那个目录的
    /// 现场恢复。连接在后台进行，窗口先显示开始页；需要口令时照常询问。
    pub(crate) fn restore_remote_folder(&mut self) {
        let wanted = self.editor.root().is_none() && self.remote_enabled();
        if let Some(folder) = self.state.last.clone().filter(|_| wanted) {
            self.reopen_remote(&folder);
        }
    }

    /// 在后台连接 `target`（已有连接时沿用）并取得其上目录 `folder` 的真实路径（空表示主目录），
    /// 完成后按 `after` 打开它或从它开始浏览；同时只进行一次连接。
    fn remote_open(&mut self, target: Target, folder: String, after: After) {
        // 提示留在右上角并带「取消」：登录可能在等待网络，没有时限。
        if let Some(busy) = &self.remote.connecting {
            let message = t!("workspace-remote-connecting", host = busy.as_str());
            self.notify_pinned(message, NoticeAction::CancelConnect);
            return;
        }
        // 经对话框连接时进度和取消都在对话框里，不另外提示。
        if self.remote.form.is_none() {
            let host = target.as_str();
            let message = match after {
                After::Restore => t!("workspace-remote-reconnecting", host = host),
                After::Open | After::Browse | After::SshWorkspace => {
                    t!("workspace-remote-connecting", host = host)
                }
            };
            self.notify_pinned(message, NoticeAction::CancelConnect);
        }
        // 自动重新连接不打扰用户：需要口令或确认时直接失败，由用户点「重新连接」再来。
        let quiet = after == After::Restore;
        // 用户自己发起的连接解除「不再自动重试」。
        self.remote.paused &= quiet;
        remote::set_identity(&target, self.state.remote_key(target.as_str()));
        let stage = Arc::clone(&self.remote.stage);
        *stage.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let abort = remote::Abort::default();
        self.remote.abort = Some(abort.clone());
        self.remote.connecting = Some(target.clone());
        self.remote.after = after;
        self.remote.declined = false;
        let slot = Arc::clone(&self.remote.outcome);
        let wake = self.link.proxy.clone();
        let staged = wake.clone();
        let questions = self.remote.questions.0.clone();
        self.spawn_blocking_then(UserEvent::RemoteConnected, move || {
            // 端点只在连接期间存在，`listener` 在任务结束时被丢弃。
            let closed = wake.clone().map(|proxy| {
                let target = target.clone();
                let notify = move |ended: bool| {
                    let _ = proxy.send_event(match ended {
                        true => UserEvent::RemoteClosed(target.clone()),
                        false => UserEvent::RemoteHealth,
                    });
                };
                Arc::new(notify) as Closed
            });
            let listener = wake.filter(|_| !quiet).and_then(|proxy| {
                Listener::bind(false, move |question| {
                    let _ = questions.send(question);
                    let _ = proxy.send_event(UserEvent::RemoteQuestion);
                })
            });
            let report = |reached| {
                *stage.lock().unwrap_or_else(PoisonError::into_inner) = Some(reached);
                if let Some(proxy) = &staged {
                    let _ = proxy.send_event(UserEvent::RemoteStage);
                }
            };
            let (listener, closed) = (listener.as_ref(), closed.as_ref());
            let outcome = open(&target, &folder, listener, closed, &abort, &report);
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        });
    }

    /// 正在进行的连接在登录之后到达的阶段；没有连接在进行、还在连接网络或登录时为 `None`。
    fn remote_stage(&self) -> Option<Stage> {
        let stage = *(self.remote.stage.lock()).unwrap_or_else(PoisonError::into_inner);
        stage.filter(|_| self.remote.connecting.is_some())
    }

    /// 连接到达了新的阶段：更新右上角的提示；连接对话框按钮上的字在绘制时读取。
    pub(in crate::app) fn remote_stage_changed(&mut self) {
        let (Some(stage), Some(target)) = (self.remote_stage(), &self.remote.connecting) else {
            return;
        };
        if self.notice.action == Some(NoticeAction::CancelConnect) {
            let host = target.as_str();
            // 只换文字：提示条留在原处，不重新播放入场动画。
            self.notice.text = Some(match stage {
                Stage::Uploading => t!("workspace-remote-installing", host = host),
                Stage::Starting => t!("workspace-remote-starting", host = host),
            });
        }
        self.request_redraw();
    }

    /// 处理后台连接的结果：成功时打开目录，失败时说明原因；用户取消输入时只提示已取消。
    pub(in crate::app) fn poll_remote(&mut self) {
        let outcome = self
            .remote
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (Some(outcome), Some(target)) = (outcome, self.remote.connecting.take()) else {
            return;
        };
        let files = self.remote.files.take();
        self.end_remote_questions();
        // 取消与完成同时发生时以取消为准：不打开用户已经放弃的目录。
        let aborted = (self.remote.abort.take()).is_some_and(|abort| abort.is_aborted());
        let outcome = match outcome {
            Ok(_) if aborted => Err(String::new()),
            other => other,
        };
        // 连接的提示到此为止；它的按钮已经没有可取消的对象。
        if self.notice.action == Some(NoticeAction::CancelConnect) {
            self.dismiss_notice();
        }
        // 经对话框连接：失败的原因写在对话框里（用户取消了询问时不写），改了可以再连。
        if let (Err(error), Some((host, port))) = (&outcome, self.remote.form.clone()) {
            let declined = std::mem::take(&mut self.remote.declined);
            self.open_connect_dialog(&host, &port, false);
            if let Some(dialog) = &mut self.overlays.form_dialog {
                dialog.error = (!declined).then(|| error.clone());
            }
            return;
        }
        match outcome {
            Ok(Reached::Missing {
                wanted,
                home,
                nearest,
            }) if self.remote_enabled() && self.remote.after == After::Open => {
                // 与本机最近列表中找不到的目录相同：移出最近列表；连接保留，从最近一级上级目录
                // 开始选择别的目录。
                self.remote.lost = false;
                if self.state.recent.contains(&wanted) {
                    self.forget_missing_recent(&wanted);
                } else {
                    let path = winc_editor::display_path(&wanted, None);
                    self.notify(
                        NoticeKind::Error,
                        t!("workspace-folder-missing", path = path),
                    );
                }
                let native = |path: &Path| winc_backend::native_lossy(path).into_owned();
                self.browse_remote_folder(target, native(&home), native(&nearest));
            }
            Ok(reached) if self.remote_enabled() && self.remote.after == After::Browse => {
                let folder = reached.into_folder();
                // 下一步：关闭连接对话框，在面板中选择目录。
                self.remote.form = None;
                self.connect_dialog_steps_aside();
                if self.remote.stepping {
                    self.close_form_dialog();
                }
                let home = winc_backend::native_lossy(&folder).into_owned();
                self.open_remote_browser(target, home);
            }
            Ok(reached) if self.remote_enabled() && self.remote.after == After::SshWorkspace => {
                // 连接成功：关闭连接对话框，进入 SSH 管理工作区。
                self.remote.form = None;
                if self.overlays.form_dialog.is_some() {
                    self.close_form_dialog();
                }
                let folder = reached.into_folder();
                let home = winc_backend::native_lossy(&folder).into_owned();
                self.ssh_workspace_connected(target, home);
            }
            Ok(reached) if self.remote_enabled() => {
                let folder = reached.into_folder();
                self.remote.lost = false;
                if self.editor.root() == Some(folder.as_path()) {
                    // 重新连接到已打开的目录：按窗口重新获得焦点处理，重读文件树和文档。
                    self.check_disk();
                } else {
                    self.open_folder(&folder);
                }
                if let Some((wanted, files)) = files {
                    self.open_files(&rebase(&files, &wanted, &folder));
                }
            }
            // 连接期间模块被关闭：关闭时这条连接尚未登记，在这里断开。
            Ok(_) => remote::disconnect(&target),
            Err(_) if std::mem::take(&mut self.remote.declined) => {
                self.remote.paused = self.remote.after == After::Restore;
                self.flash(t!("workspace-remote-canceled"));
            }
            // 自动重新连接没有成功：说明连接已断开，留一个按钮；每次中断只提示一次，之后窗口
            // 重新获得焦点时仍会再试。
            Err(_) if self.remote.after == After::Restore => {
                let root = self.editor.root().map(Path::to_path_buf);
                if let Some(root) = root.filter(|_| !self.remote.lost) {
                    self.remote.lost = true;
                    self.notify_with(
                        NoticeKind::Error,
                        t!("workspace-remote-lost", host = target.as_str()),
                        NoticeAction::Reconnect(root),
                    );
                }
            }
            Err(error) => {
                // 重新连接失败后仍是断开的状态：下次获得焦点时再次提示。
                self.remote.lost = false;
                self.notify(
                    NoticeKind::Error,
                    t!(
                        "workspace-remote-failed",
                        host = target.as_str(),
                        error = error
                    ),
                );
            }
        }
    }

    /// 处理 `ssh` 送达的输入请求；没有连接在进行时丢弃，askpass 程序按取消处理。
    pub(in crate::app) fn poll_remote_questions(&mut self) {
        while let Ok(question) = self.remote.questions.1.try_recv() {
            if self.remote.connecting.is_none() {
                continue;
            }
            // 是口令询问、且为这台主机记过口令：直接答，不再弹对话框。
            if question.is_password()
                && let Some(target) = self.remote.connecting.clone()
                && let Some(saved) = self.state.remote_password(target.as_str())
            {
                let saved = saved.to_owned();
                question.reply(saved);
                continue;
            }
            self.open_question(Asker::Remote, question);
        }
    }

    /// 到 `target` 的连接意外中断：打开的目录在该主机上时自动重新连接一次（不向用户提问）；
    /// 不成功时提示，并提供重新连接的按钮。
    ///
    /// 没有定时的重试：连接中断时、之后窗口每次重新获得焦点时各试一次（见
    /// [`App::check_remote`]），电脑从睡眠中醒来、网络恢复后回到窗口即可接上。
    pub(in crate::app) fn remote_closed(&mut self, target: &Target) {
        let root = self.editor.root().map(Path::to_path_buf);
        let Some(root) = root.filter(|root| {
            winc_backend::locate(root).is_some_and(|location| location.target == *target)
        }) else {
            return;
        };
        // 连接对话框开着时不抢它的位置：那里的连接结束后，下一次获得焦点再试。
        let busy = self.remote.connecting.is_some() || self.remote.form.is_some();
        if busy || self.remote.paused || !self.remote_enabled() {
            return;
        }
        let Some(location) = winc_backend::locate(&root) else {
            return;
        };
        let path = String::from_utf8_lossy(&location.path).into_owned();
        self.remote_open(location.target, path, After::Restore);
    }

    /// 窗口重新获得焦点时调用：打开的目录所在主机的连接已不可用时重新连接（见
    /// [`App::remote_closed`]）。连接是进程内各窗口共用的，中断的通知只送到发起连接的窗口。
    pub(in crate::app) fn check_remote(&mut self) {
        let target = (self.editor.root())
            .and_then(winc_backend::locate)
            .map(|location| location.target);
        if let Some(target) = target.filter(|target| !remote::is_connected(target)) {
            self.remote_closed(&target);
        }
    }

    /// 记为正在连接 `devbox`，返回这次连接的中止入口；不启动 `ssh`。用到它的测试只在 Unix 上
    /// 编译。
    #[cfg(all(test, unix))]
    pub(in crate::app) fn begin_connection_for_test(&mut self) -> remote::Abort {
        let abort = remote::Abort::default();
        self.remote.abort = Some(abort.clone());
        self.remote.connecting = Target::new("devbox");
        abort
    }

    /// 取消正在进行的连接（提示上的「取消」）：结束 `ssh`，后台的连接随即以失败返回；服务
    /// 进程已经启动、正在取得目录的路径时同样有效。
    pub(in crate::app) fn cancel_remote(&mut self) {
        if let Some(abort) = &self.remote.abort {
            self.remote.declined = true;
            abort.abort();
        }
    }

    /// 命令「断开 SSH 连接」：关闭打开的远端目录，并断开到该主机的连接。
    ///
    /// 连接是进程内各窗口共用的：其他窗口打开着同一主机上的目录时，它们会提示连接已断开。
    pub(in crate::app) fn remote_disconnect(&mut self) {
        let target = (self.editor.root())
            .and_then(winc_backend::locate)
            .map(|location| location.target);
        let Some(target) = target else {
            return;
        };
        // 已提示过：关闭目录的过程中不再弹出连接中断的提示。
        self.remote.lost = true;
        self.close_folder();
        remote::disconnect(&target);
    }

    /// 关闭远程模块时调用：断开全部连接。
    pub(in crate::app) fn stop_remote(&mut self) {
        remote::disconnect_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_in_included_files_are_listed_once() {
        let dir = std::env::temp_dir().join(format!("winc-ssh-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d")).unwrap();
        let at = |name: &str| dir.join(name).display().to_string();
        let main = format!(
            "Host first\nInclude {} {}\nHost last\n",
            at("d/*.conf"),
            at("gone")
        );
        std::fs::write(dir.join("config"), main).unwrap();
        std::fs::write(dir.join("d/b.conf"), "Host beta first\n").unwrap();
        std::fs::write(dir.join("d/a.conf"), "Host alpha\n").unwrap();
        std::fs::write(dir.join("d/skipped.txt"), "Host hidden\n").unwrap();
        // 引入自己的文件不会一直读下去。
        let again = format!("Host loop\nInclude {}\n", at("d/c.conf"));
        std::fs::write(dir.join("d/c.conf"), again).unwrap();
        let mut hosts = Vec::new();
        hosts_in(&dir.join("config"), 0, &mut hosts);
        assert_eq!(hosts, ["first", "last", "alpha", "beta", "loop"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chosen_key_file_is_written_relative_to_the_home_folder() {
        let home = Path::new("/home/me");
        let shown = |path| key_text(Path::new(path), Some(home));
        assert_eq!(shown("/home/me/.ssh/id_ed25519"), "~/.ssh/id_ed25519");
        assert_eq!(shown("/keys/work"), "/keys/work");
        // 只是名字的前缀相同，不在主目录之下。
        assert_eq!(shown("/home/mercury/key"), "/home/mercury/key");
        assert_eq!(key_text(Path::new("/keys/work"), None), "/keys/work");
    }

    #[test]
    fn a_missing_ssh_config_is_created_empty_and_an_existing_one_is_kept() {
        let home = std::env::temp_dir().join(format!("winc-ssh-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let path = home.join(".ssh").join("config");
        ensure_config(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(path.parent().unwrap()), mode(&path)), (0o700, 0o600));
        }
        std::fs::write(&path, "Host devbox\n").unwrap();
        ensure_config(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"Host devbox\n");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn destinations_split_into_a_host_and_a_folder() {
        let parsed = |input| {
            parse_destination(input).map(|(target, folder)| (target.as_str().to_owned(), folder))
        };
        let host = "me@devbox".to_owned();
        assert_eq!(
            parsed("me@devbox:/srv/app"),
            Some((host.clone(), "/srv/app"))
        );
        assert_eq!(parsed("  me@devbox:~/x y  "), Some((host.clone(), "~/x y")));
        assert_eq!(parsed("me@devbox"), Some((host.clone(), "")));
        assert_eq!(parsed("me@devbox:"), Some((host, "")));
        // 主机之后的内容原样作为目录，其中的 `:` 不再拆分。
        assert_eq!(parsed("box:/a:b"), Some(("box".to_owned(), "/a:b")));
        // 主机之后全是数字的一段是端口，其后才是目录。
        assert_eq!(
            parsed("me@box:22022"),
            Some(("me@box:22022".to_owned(), ""))
        );
        assert_eq!(
            parsed("me@box:22022:"),
            Some(("me@box:22022".to_owned(), ""))
        );
        assert_eq!(
            parsed("me@box:22022:~/app"),
            Some(("me@box:22022".to_owned(), "~/app"))
        );
        assert_eq!(
            parsed("box:22:/srv:x"),
            Some(("box:22".to_owned(), "/srv:x"))
        );
        assert_eq!(parsed("box:~/2024"), Some(("box".to_owned(), "~/2024")));
        // 不是有效的端口：不当作目录去连接。
        assert_eq!(parsed("box:0"), None);
        assert_eq!(parsed("box:99999"), None);
        // 不能作为 `ssh` 主机参数的输入被拒绝：选项、空白和 shell 的特殊字符。
        for input in [
            "",
            ":/srv",
            "-oProxyCommand=x:/",
            "a b:/",
            "a;b:/",
            "$(x):/",
        ] {
            assert_eq!(parsed(input), None, "{input:?}");
        }
    }

    #[test]
    fn hosts_come_from_the_host_lines_of_the_ssh_configuration() {
        let config = "\
# comment
Host devbox
  HostName 10.0.0.5
  User me
host = staging  \"build box\" *.internal !bastion
Host *
  ServerAliveInterval 30
HOST=prod devbox
Match host other
Hostname not-a-host
Include extra
Host -oProxyCommand=x a;b
";
        // 通配和取反的规则、重复的名字、不能作为 `ssh` 主机参数的名字都不列出。
        assert_eq!(config_hosts(config), ["devbox", "staging", "prod"]);
        assert!(config_hosts("").is_empty());
        // `Include` 的参数和通配的文件名。
        let config = "Include config.d/* \"~/My Hosts/extra\"\ninclude=one\nHost x\n";
        assert_eq!(
            arguments(config, "include"),
            ["config.d/*", "~/My Hosts/extra", "one"]
        );
        assert!(wild("*.conf", "work.conf") && wild("*", "anything") && wild("a?c", "abc"));
        assert!(!wild("*.conf", "work.conf.bak") && !wild("a?c", "ac") && !wild("", "x"));
        assert!(wild("", "") && wild("**", ""));
    }

    #[test]
    fn only_files_named_after_a_system_are_server_programs() {
        for name in ["Linux-x86_64", "Linux-aarch64", "Darwin-arm64"] {
            assert!(is_system(name), "{name}");
        }
        // 构建目录中带哈希后缀的文件、调试信息和其他附属文件。
        for name in [
            "",
            "0a1b2c3d4e5f6071",
            "Linux-x86_64.d",
            "Linux-",
            "-x86_64",
            "a-b-c",
        ] {
            assert!(!is_system(name), "{name}");
        }
    }

    /// 经真实的 `ssh` 连接并取得主目录：需要环境变量 `WINC_TEST_SSH_TARGET`（免输入即可登录的
    /// 主机）和 `WINC_SERVER_DIR`（其中有该主机所用系统的服务进程）。
    #[test]
    #[ignore = "需要一台可以免输入登录的主机"]
    fn over_ssh() {
        let target = Target::new(&std::env::var("WINC_TEST_SSH_TARGET").unwrap()).unwrap();
        let home = open(&target, "~", None, None, &remote::Abort::default(), &|_| {})
            .unwrap()
            .into_folder();
        assert!(winc_backend::fs::is_dir(&home), "{}", home.display());
        let sub = open(
            &target,
            ".ssh",
            None,
            None,
            &remote::Abort::default(),
            &|_| {},
        )
        .unwrap()
        .into_folder();
        assert_eq!(sub, home.join(".ssh"));
        // 不存在的目录：给出主目录和仍然存在的最近一级上级目录。
        let missing = open(
            &target,
            "/winc/no/such/folder",
            None,
            None,
            &remote::Abort::default(),
            &|_| {},
        );
        let Ok(Reached::Missing {
            wanted,
            home: found,
            nearest,
        }) = missing
        else {
            panic!("{missing:?}");
        };
        let root = winc_backend::virtual_path(&target, b"/");
        assert_eq!(
            wanted,
            winc_backend::virtual_path(&target, b"/winc/no/such/folder")
        );
        assert_eq!((found, nearest), (home.clone(), root));
        let gone = home.join(".ssh/winc-no-such-folder/inner");
        let gone = winc_backend::locate(&gone).unwrap().path;
        let gone = String::from_utf8_lossy(&gone).into_owned();
        let missing = open(
            &target,
            &gone,
            None,
            None,
            &remote::Abort::default(),
            &|_| {},
        );
        assert!(
            matches!(&missing, Ok(Reached::Missing { nearest, .. }) if *nearest == sub),
            "{missing:?}"
        );

        // 那台主机上登录 shell 的 `PATH` 和按名字查找程序：格式化工具由此找到。
        let path = winc_editor::remote_env::login_path(&home).unwrap();
        assert!(path.split(':').any(|dir| dir == "/usr/bin"), "{path}");
        let sh = winc_editor::remote_env::find_program(&home, "sh").unwrap();
        assert!(sh.is_absolute() && sh.ends_with("sh"), "{}", sh.display());
        assert_eq!(
            winc_editor::remote_env::find_program(&home, "winc-no-such-tool"),
            None
        );

        // shell 集成脚本在连接之后写到那台主机上，只有用户本人可以读。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !shell_ready(&target) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(shell_ready(&target));
        // 那台主机上的 shell 随后列出，供终端的 shell 菜单使用。
        while shells(&target).is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let found = shells(&target);
        assert!(found.iter().all(|shell| shell.command.starts_with('/')));
        assert!(!found.is_empty());
        for (name, content) in winc_pty::scripts::remote() {
            let file = home.join(winc_pty::scripts::REMOTE_DIR).join(name);
            assert_eq!(
                winc_backend::fs::read_to_string(&file).unwrap(),
                content,
                "{name}"
            );
            let mode = winc_backend::fs::metadata(&file)
                .unwrap()
                .unix
                .unwrap()
                .mode;
            assert_eq!(mode & 0o077, 0, "{name}");
        }
        remote::disconnect(&target);
    }

    #[test]
    fn appended_host_blocks_are_recognized_and_removable() {
        // 追加格式（与 append_to_ssh_config 一致）能被 config_hosts 识别。
        let text = "Host alpha\n    HostName alpha\n    Port 2222\n    IdentityFile ~/.ssh/id\n";
        assert_eq!(config_hosts(text), vec!["alpha".to_owned()]);

        // 多行 Host 段：第二个 Host 之前的选项不会串到它身上。
        let text = "Host alpha\n    HostName alpha\nHost beta\n    HostName beta\n";
        assert_eq!(config_hosts(text), vec!["alpha".to_owned(), "beta".to_owned()]);

        // 通配与取反不算具体主机。
        let text = "Host *\n    ServerAliveInterval 30\nHost !bad gamma\n";
        assert_eq!(config_hosts(text), vec!["gamma".to_owned()]);
    }
}
