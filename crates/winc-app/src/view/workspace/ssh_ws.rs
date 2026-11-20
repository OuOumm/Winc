//! SSH 工作区：会话标签 + 远程文件 + 服务器概览。
//!
//! 该视图接管编辑区（`draw_editor` 顶部分支）。终端栏由全局终端面板绘制在
//! [`ssh_monitor::Parts::terminal`] 矩形里；这里绘制会话标签、文件列表和服务器概览。

use winc_i18n::t;
use winc_render::{Color, Frame, Rect, Stroke};

use super::*;
use crate::app::ssh_monitor::{self, Parts, SshWsTarget};

/// 行高（逻辑像素）。
const ROW: f32 = 22.0;
/// 卡片标题行高。
const HEAD: f32 = 28.0;
/// 内边距。
const PAD: f32 = 10.0;

/// 绘制 SSH 工作区（终端栏之外的部分）。
pub(in crate::view) fn draw(app: &mut App, frame: &mut Frame<'_>) {
    let Some(ws) = app.remote.ws.clone() else {
        return;
    };
    let p = app.palette;
    let parts = Parts::new(app.layout.editor, ws.side_width, ws.overview_share());

    // 终端还没建好（辅助进程握手期间）时，在终端栏位置给出提示；
    // 建好后由全局终端面板在同一矩形里绘制。
    if app.terminal.tabs.is_empty() {
        frame.rect_clipped(parts.terminal, p.popup_bg, crate::theme::RADIUS_ROW, parts.terminal);
        ui_text_small(
            frame,
            app,
            parts.terminal.x + PAD,
            Rect::new(parts.terminal.x, parts.terminal.y + PAD, parts.terminal.w, 20.0),
            t!("workspace-ssh-terminal-pending"),
            p.text_dim,
            parts.terminal,
        );
    }

    // 会话标签栏。
    draw_tabs(app, frame, parts.tabs, &ws);
    // 文件列表栏。
    draw_files(app, frame, parts.files, &ws);
    // 服务器概览栏。
    draw_overview(app, frame, parts.overview, &ws);

    // 记录命中区域，供点击处理。
    // 右侧栏与终端之间的分隔条：按下的位置由命中处理换算成拖动的宽度。
    let hovered = app.pointer.hover == crate::app::Hover::SshSideDivider;
    frame.rect_clipped(
        parts.side_divider.inset(1.5, 0.0),
        if hovered { p.accent } else { p.hover },
        0.0,
        parts.side_divider,
    );
    app.hits.ssh_ws_parts = Some(parts);
}

/// 绘制会话标签：每个会话一段，右侧是刷新和关闭按钮。
fn draw_tabs(app: &mut App, frame: &mut Frame<'_>, rect: Rect, ws: &ssh_monitor::Workspace) {
    let p = app.palette;
    frame.rect_clipped(rect, p.popup_bg, crate::theme::RADIUS_ROW, rect);
    let mut x = rect.x + PAD;
    for (i, session) in ws.sessions.iter().enumerate() {
        let label = session.target.as_str().to_owned();
        let w = frame.measure(FontKind::Ui, app.fonts.ui_small, &label) + 24.0;
        let tab = Rect::new(x, rect.y + 3.0, w, rect.h - 6.0);
        let on = i == ws.active;
        frame.rect_clipped(tab, if on { p.selection } else { p.hover }, crate::theme::RADIUS_ROW, rect);
        ui_text_small(frame, app, tab.x + 10.0, tab, &label, if on { p.text } else { p.text_muted }, rect);
        app.hits.ssh_ws.push((tab, crate::app::ssh_monitor::SshWsTarget::Session(i)));
        x += w + 4.0;
    }
    // 刷新与关闭按钮。
    let close = Rect::new(rect.right() - 28.0, rect.y + 3.0, 24.0, rect.h - 6.0);
    frame.rect_clipped(close, p.hover, crate::theme::RADIUS_ROW, rect);
    ui_text_small(frame, app, close.x + 7.0, close, "✕", p.text_muted, rect);
    app.hits.ssh_ws.push((close, crate::app::ssh_monitor::SshWsTarget::Close));
    let btn = Rect::new(close.x - 28.0, rect.y + 3.0, 24.0, rect.h - 6.0);
    frame.rect_clipped(btn, p.hover, crate::theme::RADIUS_ROW, rect);
    ui_text_small(frame, app, btn.x + 7.0, btn, "⟳", p.text_muted, rect);
    app.hits.ssh_ws.push((btn, crate::app::ssh_monitor::SshWsTarget::Refresh));
}

/// 绘制远程文件列表：目录行 + 表头 + 多列文件表 + 底部统计。
fn draw_files(app: &mut App, frame: &mut Frame<'_>, rect: Rect, ws: &ssh_monitor::Workspace) {
    let p = app.palette;
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    frame.rect_clipped(rect, p.popup_bg, crate::theme::RADIUS_ROW, rect);

    // 标题行：当前目录。
    let head = Rect::new(rect.x, rect.y, rect.w, HEAD);
    let dir = ws.active().map(|s| s.cwd.clone()).unwrap_or_default();
    let title = if dir.is_empty() { "/".to_owned() } else { dir };
    let title_max = (rect.w - 190.0).max(40.0);
    let shown = truncate_end(frame, &title, app.fonts.ui_small, title_max);
    ui_text_small(frame, app, rect.x + PAD, head, &shown, p.text_muted, rect);

    // 右侧三个按钮：新建目录、上传、上级。
    let mut bx = rect.right() - 6.0;
    for (label, target) in [
        (t!("workspace-ssh-up").to_string(), SshWsTarget::FileUp),
        (t!("workspace-ssh-upload").to_string(), SshWsTarget::Upload),
        (t!("workspace-ssh-mkdir").to_string(), SshWsTarget::MkDir),
    ] {
        let text = format!("{} {}", if target == SshWsTarget::FileUp { "↑" } else { "＋" }, label);
        let w = frame.measure(FontKind::Ui, app.fonts.ui_small, &text) + 12.0;
        let b = Rect::new(bx - w, rect.y + 2.0, w, HEAD - 4.0);
        frame.rect_clipped(b, p.hover, crate::theme::RADIUS_ROW, rect);
        ui_text_small(frame, app, b.x + 6.0, b, &text, p.text_muted, rect);
        app.hits.ssh_ws.push((b, target));
        bx = b.x - 4.0;
    }

    // 表头：名称 / 修改时间 / 大小 / 权限 / 属主。
    let header_y = rect.y + HEAD;
    let header = Rect::new(rect.x, header_y, rect.w, 18.0);
    frame.rect_clipped(header, p.hover.alpha(0.25), 0.0, rect);
    let cols = column_layout(rect.w);
    for (i, name) in ["名称", "修改时间", "大小", "权限", "属主"].iter().enumerate() {
        ui_text_small(frame, app, rect.x + PAD + cols[i], header, name, p.text_dim, rect);
    }

    // 文件行。
    let mut y = header.bottom() + 2.0;
    for i in 0..ws.files.len() {
        if y + ROW > rect.bottom() - 18.0 {
            break;
        }
        let Some(entry) = ws.files.get(i) else { break };
        let row = Rect::new(rect.x + 2.0, y, rect.w - 8.0, ROW);
        if i % 2 == 1 {
            frame.rect_clipped(row, p.hover.alpha(0.22), 0.0, rect);
        }
        let icon = if entry.is_dir { "📁" } else { "📄" };
        ui_text_small(frame, app, row.x + PAD, row, icon, p.text_muted, rect);
        ui_text_small(frame, app, row.x + PAD + cols[0], row, &entry.name, p.text, rect);
        ui_text_small(frame, app, row.x + PAD + cols[1], row, &entry.time, p.text_dim, rect);
        ui_text_small(frame, app, row.x + PAD + cols[2], row, &entry.size, p.text_dim, rect);
        ui_text_small(frame, app, row.x + PAD + cols[3], row, &entry.perm, p.text_dim, rect);
        ui_text_small(frame, app, row.x + PAD + cols[4], row, &entry.owner, p.text_dim, rect);
        // 非目录行右端：下载按钮。
        if !entry.is_dir {
            let dl = Rect::new(row.right() - 16.0, row.y + 3.0, 14.0, ROW - 6.0);
            ui_text_small(frame, app, dl.x, dl, "⇩", p.accent, rect);
            app.hits.ssh_ws.push((dl, SshWsTarget::Download(i)));
        }
        app.hits.ssh_ws.push((row, SshWsTarget::File(i)));
        y += ROW;
    }

    if let Some(err) = &ws.file_error {
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, ROW), err, p.error, rect);
    } else if ws.files.is_empty() {
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, ROW), t!("workspace-ssh-empty"), p.text_dim, rect);
    }

    // 底部统计：共 N 个文件、M 个文件夹。
    let files = ws.files.iter().filter(|e| !e.is_dir).count();
    let dirs = ws.files.iter().filter(|e| e.is_dir).count();
    let foot = Rect::new(rect.x, rect.bottom() - 18.0, rect.w, 18.0);
    frame.rect_clipped(foot, p.hover.alpha(0.25), 0.0, rect);
    let text = t!(
        "workspace-ssh-file-count",
        files = files,
        dirs = dirs
    );
    ui_text_small(frame, app, rect.x + PAD, foot, &text, p.text_dim, rect);
}

/// 文件表各列的横向偏移（相对内容起点），依次为名称、时间、大小、权限、属主。
fn column_layout(width: f32) -> [f32; 5] {
    let name = 0.0;
    let time = (width * 0.40).max(120.0);
    let size = time + 108.0;
    let perm = size + 66.0;
    let owner = perm + 86.0;
    [name, time, size, perm, owner]
}

/// 绘制服务器概览：系统信息、CPU（含每核心）、内存/Swap、网络、磁盘、进程。
fn draw_overview(app: &mut App, frame: &mut Frame<'_>, rect: Rect, ws: &ssh_monitor::Workspace) {
    let p = app.palette;
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    let clip = rect;
    frame.rect_clipped(rect, p.popup_bg, crate::theme::RADIUS_ROW, clip);
    let Some(session) = ws.active() else {
        return;
    };
    let ov = &session.overview;

    // 内容整体按滚动偏移上移；标题行固定在顶部不滚。
    let scroll = ws.overview_scroll;
    let title_h = 22.0;
    // 标题行：主机 + 在线状态 + 折叠按钮。
    let live = if ov.updated.is_some() { "● " } else { "○ " };
    let title = format!("{}{} — {}", live, t!("workspace-ssh-overview"), session.target.host());
    ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, rect.y + PAD, rect.w, 18.0), &title, p.text, clip);
    let toggle = Rect::new(rect.right() - 22.0, rect.y + 2.0, 18.0, 18.0);
    ui_text_small(
        frame,
        app,
        toggle.x + 3.0,
        toggle,
        if ws.overview_open { "▾" } else { "▸" },
        p.text_muted,
        clip,
    );
    app.hits.ssh_ws.push((toggle, SshWsTarget::ToggleOverview));
    // 收起时只留标题条。
    if !ws.overview_open {
        return;
    }
    let mut y = rect.y + PAD + title_h - scroll;

    if let Some(err) = &session.error {
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, 18.0), err, p.error, clip);
        y += 18.0;
    }

    // 系统信息（两列）。
    section_title(app, frame, rect, &mut y, "系统信息");
    for (k, v) in ov.system.iter() {
        if y + 16.0 > clip.bottom() {
            return;
        }
        let row = Rect::new(rect.x, y, rect.w, 16.0);
        ui_text_small(frame, app, rect.x + PAD, row, k, p.text_dim, clip);
        let shown = truncate_end(frame, v, app.fonts.ui_small, (rect.w - 110.0).max(40.0));
        ui_text_small(frame, app, rect.x + 84.0, row, &shown, p.text, clip);
        y += 16.0;
    }

    // CPU：总占用 + 每核心。
    y += 6.0;
    section_title(app, frame, rect, &mut y, "CPU");
    y = draw_bar(app, frame, rect, y, "总占用", ov.cpu_total, p.accent);
    if !ov.cpu_cores.is_empty() && y + 20.0 < clip.bottom() {
        // 每核心两列，最多显示 8 个。
        let per = (rect.w - 2.0 * PAD - 12.0) / 2.0;
        for (i, core) in ov.cpu_cores.iter().take(8).enumerate() {
            let col = i % 2;
            let row_i = i / 2;
            let x = rect.x + PAD + col as f32 * (per + 12.0);
            let ry = y + row_i as f32 * 18.0;
            if ry + 14.0 > clip.bottom() {
                break;
            }
            let label = format!("CPU{i}");
            ui_text_small(frame, app, x, Rect::new(x, ry, per, 14.0), &label, p.text_dim, clip);
            let bar = Rect::new(x + 38.0, ry + 5.0, (per - 44.0).max(10.0), 4.0);
            frame.rect_clipped(bar, p.hover, 2.0, clip);
            let w = bar.w * (core / 100.0).clamp(0.0, 1.0);
            let color = if *core > 80.0 {
                p.error
            } else if *core > 50.0 {
                p.warning
            } else {
                p.accent
            };
            frame.rect_clipped(Rect::new(bar.x, bar.y, w, bar.h), color, 2.0, clip);
        }
        y += ov.cpu_cores.len().min(8).div_ceil(2) as f32 * 18.0 + 4.0;
    }
    if y + 16.0 < clip.bottom() {
        let load = format!("负载 {:.2} / {:.2} / {:.2}", ov.load[0], ov.load[1], ov.load[2]);
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, 16.0), &load, p.text_muted, clip);
        y += 18.0;
    }

    // 内存与 Swap。
    section_title(app, frame, rect, &mut y, "内存 / 交换");
    let mem_used = format!("{} / {}", fmt_mb(ov.memory.0), fmt_mb(ov.memory.1));
    y = draw_bar_with(app, frame, rect, y, ("物理内存", mem_used), (pct(ov.memory.0, ov.memory.1), p.accent));
    let swap_used = format!("{} / {}", fmt_mb(ov.swap.0), fmt_mb(ov.swap.1));
    y = draw_bar_with(app, frame, rect, y, ("Swap", swap_used), (pct(ov.swap.0, ov.swap.1), p.accent.alpha(0.6)));

    // 网络：速率 + 折线 + 接口。
    if y + 40.0 < clip.bottom() {
        section_title(app, frame, rect, &mut y, "网络");
        let rx = format_rate(ov.net_rx_now);
        let tx = format_rate(ov.net_tx_now);
        let row = Rect::new(rect.x, y, rect.w, 16.0);
        ui_text_small(frame, app, rect.x + PAD, row, &format!("↓ {rx}"), p.accent, clip);
        let label = format!("↑ {tx}");
        let lw = frame.measure(FontKind::Ui, app.fonts.ui_small, &label);
        ui_text_small(frame, app, rect.right() - PAD - lw, row, &label, p.text_muted, clip);
        y += 16.0;
        if !ov.net_rx.is_empty() {
            y = draw_spark(app, frame, rect, y, (&ov.net_rx, &ov.net_tx), (p.accent, p.text_muted));
        }
        for (name, detail) in ov.interfaces.iter().take(3) {
            if y + 15.0 > clip.bottom() {
                break;
            }
            let text = format!("{name}  {detail}");
            let shown = truncate_end(frame, &text, app.fonts.ui_small, rect.w - 2.0 * PAD);
            ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, 15.0), &shown, p.text_dim, clip);
            y += 15.0;
        }
    }

    // 磁盘挂载。
    if y + 20.0 < clip.bottom() {
        section_title(app, frame, rect, &mut y, "磁盘挂载");
        for (mount, fs, percent, used) in ov.mounts.iter().take(4) {
            if y + 24.0 > clip.bottom() {
                break;
            }
            let label = format!("{mount} · {fs}");
            ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, 14.0), &label, p.text_muted, clip);
            ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y + 14.0, rect.w, 12.0), used, p.text_dim, clip);
            let bar = Rect::new(rect.x + PAD, y + 27.0, rect.w - 2.0 * PAD, 3.0);
            frame.rect_clipped(bar, p.hover, 1.5, clip);
            let w = bar.w * (*percent as f32 / 100.0).clamp(0.0, 1.0);
            let color = if *percent > 85 { p.error } else if *percent > 65 { p.warning } else { p.accent };
            frame.rect_clipped(Rect::new(bar.x, bar.y, w, bar.h), color, 1.5, clip);
            y += 34.0;
        }
    }

    // 进程：过滤框 + 列表 + kill。
    if y + 22.0 < clip.bottom() {
        section_title(app, frame, rect, &mut y, "进程");
        let filter = Rect::new(rect.x + PAD, y, rect.w - 2.0 * PAD, 18.0);
        frame.rect_clipped(filter, p.hover.alpha(0.4), 4.0, clip);
        let text = if ws.proc_filter.is_empty() {
            t!("workspace-ssh-proc-filter").to_string()
        } else {
            ws.proc_filter.clone()
        };
        let color = if ws.proc_filter.is_empty() { p.text_dim } else { p.text };
        ui_text_small(frame, app, filter.x + 6.0, filter, &text, color, clip);
        app.hits
            .ssh_ws
            .push((filter, SshWsTarget::ProcFilter));
        y += 22.0;

        for (index, (pid, name, cpu, mem)) in ws.filtered_processes(&ov.processes).take(8) {
            if y + 15.0 > clip.bottom() {
                break;
            }
            let row = Rect::new(rect.x, y, rect.w, 15.0);
            let label = format!("{pid:>6}  {name}  {cpu:.1}%  {mem}");
            let shown = truncate_end(frame, &label, app.fonts.ui_small, rect.w - 2.0 * PAD - 26.0);
            ui_text_small(frame, app, rect.x + PAD, row, &shown, p.text, clip);
            let kill = Rect::new(row.right() - 30.0, y, 24.0, 15.0);
            ui_text_small(frame, app, kill.x, kill, "✕", p.error, clip);
            app.hits.ssh_ws.push((kill, SshWsTarget::KillProc(index)));
            y += 15.0;
        }
    }
}

/// 小节标题。
fn section_title(app: &mut App, frame: &mut Frame<'_>, rect: Rect, y: &mut f32, text: &str) {
    if *y + 18.0 > rect.bottom() {
        return;
    }
    ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, *y, rect.w, 16.0), text, app.palette.text_muted, rect);
    *y += 18.0;
}

/// 百分比。
fn pct(used: u64, total: u64) -> f32 {
    if total == 0 { 0.0 } else { used as f32 / total as f32 * 100.0 }
}

/// MB 数格式化成 MB/GB。
fn fmt_mb(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

/// 画一条带数值文字的进度条（标签 + 右侧数值 + 条），返回下一行的 y。
fn draw_bar_with(
    app: &mut App,
    frame: &mut Frame<'_>,
    rect: Rect,
    y: f32,
    (label, value): (&str, String),
    (pct, color): (f32, Color),
) -> f32 {
    let p = app.palette;
    if y + 24.0 > rect.bottom() {
        return y;
    }
    let row = Rect::new(rect.x, y, rect.w, 14.0);
    ui_text_small(frame, app, rect.x + PAD, row, label, p.text_dim, rect);
    let tw = frame.measure(FontKind::Ui, app.fonts.ui_small, &value);
    ui_text_small(frame, app, rect.right() - PAD - tw, row, &value, p.text, rect);
    let bar = Rect::new(rect.x + PAD, y + 15.0, rect.w - 2.0 * PAD, 4.0);
    frame.rect_clipped(bar, p.hover, 2.0, rect);
    let w = bar.w * (pct / 100.0).clamp(0.0, 1.0);
    frame.rect_clipped(Rect::new(bar.x, bar.y, w, bar.h), color, 2.0, rect);
    y + 25.0
}

/// 画一条百分比进度条，返回下一行的 y。
fn draw_bar(app: &mut App, frame: &mut Frame<'_>, rect: Rect, y: f32, label: &str, pct: f32, color: winc_render::Color) -> f32 {
    let p = app.palette;
    let row = Rect::new(rect.x, y, rect.w, 16.0);
    ui_text_small(frame, app, rect.x + PAD, row, label, p.text_dim, rect);
    let text = format!("{pct:.0}%");
    let tw = frame.measure(FontKind::Ui, app.fonts.ui_small, &text);
    ui_text_small(frame, app, rect.right() - PAD - tw, row, &text, p.text, rect);
    let bar = Rect::new(rect.x + PAD, y + 16.0, rect.w - 2.0 * PAD, 4.0);
    frame.rect_clipped(bar, p.hover, 2.0, rect);
    let fill = Rect::new(bar.x, bar.y, (bar.w * pct.clamp(0.0, 100.0) / 100.0).round(), bar.h);
    frame.rect_clipped(fill, color, 2.0, rect);
    y + 24.0
}
/// 把 KB/s 格式化成易读的速率文字。
fn format_rate(kbps: f32) -> String {
    if kbps >= 1024.0 {
        format!("{:.2} MB/s", kbps / 1024.0)
    } else if kbps >= 1.0 {
        format!("{kbps:.0} KB/s")
    } else {
        format!("{:.0} B/s", kbps * 1024.0)
    }
}

/// 画下、上两条速率折线，返回下一行的 y。
fn draw_spark(
    app: &mut App,
    frame: &mut Frame<'_>,
    rect: Rect,
    y: f32,
    (rx, tx): (
        &std::collections::VecDeque<f32>,
        &std::collections::VecDeque<f32>,
    ),
    (down, up): (Color, Color),
) -> f32 {
    let p = app.palette;
    let h = 30.0_f32;
    let box_rect = Rect::new(rect.x + PAD, y, rect.w - 2.0 * PAD, h);
    frame.rect_clipped(box_rect, p.hover, 4.0, rect);
    let max = rx.iter().chain(tx.iter()).copied().fold(1.0_f32, f32::max);
    for (values, color) in [(rx, down), (tx, up)] {
        let n = values.len();
        if n < 2 {
            continue;
        }
        let shapes: Vec<Stroke> = values
            .iter()
            .enumerate()
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| {
                let (i, a) = w[0];
                let (_, b) = w[1];
                Stroke::Line(
                    box_rect.x + i as f32 / (n - 1) as f32 * box_rect.w,
                    box_rect.bottom() - a / max * (box_rect.h - 2.0) - 1.0,
                    box_rect.x + (i + 1) as f32 / (n - 1) as f32 * box_rect.w,
                    box_rect.bottom() - b / max * (box_rect.h - 2.0) - 1.0,
                )
            })
            .collect();
        frame.stroke(&shapes, 1.2, color, rect);
    }
    y + h + 6.0
}
