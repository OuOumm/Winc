//! SSH 工作区：会话标签 + 远程文件 + 服务器概览。
//!
//! 该视图接管编辑区（`draw_editor` 顶部分支）。终端栏由全局终端面板绘制在
//! [`ssh_monitor::Parts::terminal`] 矩形里；这里绘制会话标签、文件列表和服务器概览。

use winc_i18n::t;
use winc_render::{Color, Frame, Rect, Stroke};

use super::*;
use crate::app::ssh_monitor::{self, Parts};

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
    let parts = Parts::new(app.layout.editor);

    // 会话标签栏。
    draw_tabs(app, frame, parts.tabs, &ws);
    // 文件列表栏。
    draw_files(app, frame, parts.files, &ws);
    // 服务器概览栏。
    draw_overview(app, frame, parts.overview, &ws);

    // 记录命中区域，供点击处理。
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

/// 绘制远程文件列表。
fn draw_files(app: &mut App, frame: &mut Frame<'_>, rect: Rect, ws: &ssh_monitor::Workspace) {
    let p = app.palette;
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    frame.rect_clipped(rect, p.popup_bg, crate::theme::RADIUS_ROW, rect);
    // 标题：当前目录 + 文件数。
    let head = Rect::new(rect.x, rect.y, rect.w, HEAD);
    let session = ws.active();
    let dir = session.map(|s| s.cwd.clone()).unwrap_or_default();
    let title = format!("{}  ·  {} items", dir, ws.files.len());
    ui_text_small(frame, app, rect.x + PAD, head, &title, p.text_muted, rect);
    // 上传按钮。
    let upload = Rect::new(rect.right() - 116.0, rect.y + 2.0, 44.0, HEAD - 4.0);
    frame.rect_clipped(upload, p.hover, crate::theme::RADIUS_ROW, rect);
    ui_text_small(frame, app, upload.x + 5.0, upload, &format!("↑ {}", t!("workspace-ssh-upload")), p.text_muted, rect);
    app.hits.ssh_ws.push((upload, crate::app::ssh_monitor::SshWsTarget::Upload));
    // 返回上级按钮。
    let up = Rect::new(rect.right() - 68.0, rect.y + 2.0, 64.0, HEAD - 4.0);
    frame.rect_clipped(up, p.hover, crate::theme::RADIUS_ROW, rect);
    ui_text_small(frame, app, up.x + 6.0, up, &format!("↑ {}", t!("workspace-ssh-up")), p.text_muted, rect);
    app.hits.ssh_ws.push((up, crate::app::ssh_monitor::SshWsTarget::FileUp));

    let list_y = rect.y + HEAD;
    let mut y = list_y;
    for (i, (name, is_dir, size, time, _perm)) in ws.files.iter().enumerate() {
        if y + ROW > rect.bottom() {
            break;
        }
        let row = Rect::new(rect.x + 2.0, y, rect.w - 4.0, ROW);
        if i % 2 == 1 {
            frame.rect_clipped(row, p.hover.alpha(0.5), 0.0, rect);
        }
        let icon = if *is_dir { "📁" } else { "📄" };
        ui_text_small(frame, app, row.x + PAD, row, icon, p.text_muted, rect);
        ui_text_small(frame, app, row.x + PAD + 20.0, row, name, p.text, rect);
        // 右侧显示大小与时间。
        let meta = format!("{size}  {time}");
        let mw = frame.measure(FontKind::Ui, app.fonts.ui_small, &meta);
        ui_text_small(frame, app, row.right() - PAD - mw, row, &meta, p.text_dim, rect);
        // 文件行右端加下载按钮。
        if !*is_dir {
            let dl = Rect::new(row.right() - 14.0, row.y + 3.0, 14.0, ROW - 6.0);
            ui_text_small(frame, app, dl.x, dl, "⇩", p.accent, rect);
            app.hits.ssh_ws.push((dl, crate::app::ssh_monitor::SshWsTarget::Download(i)));
        }
        app.hits.ssh_ws.push((row, crate::app::ssh_monitor::SshWsTarget::File(i)));
        y += ROW;
    }
    if let Some(err) = &ws.file_error {
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, list_y, rect.w, ROW), err, p.error, rect);
    }
    if ws.files.is_empty() && ws.file_error.is_none() {
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, list_y, rect.w, ROW), t!("workspace-ssh-empty"), p.text_dim, rect);
    }
}

/// 绘制服务器概览。
fn draw_overview(app: &mut App, frame: &mut Frame<'_>, rect: Rect, ws: &ssh_monitor::Workspace) {
    let p = app.palette;
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    frame.rect_clipped(rect, p.popup_bg, crate::theme::RADIUS_ROW, rect);
    let Some(session) = ws.active() else {
        return;
    };
    let ov = &session.overview;
    let mut y = rect.y + PAD;

    // 标题行：主机 + 状态。
    let live = if ov.updated.is_some() { "● " } else { "○ " };
    let title = format!("{}{} — {}", live, t!("workspace-ssh-overview"), session.target.host());
    ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, 18.0), &title, p.text, rect);
    y += 20.0;

    if let Some(err) = &session.error {
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, 18.0), err, p.error, rect);
        y += 18.0;
    }

    // 系统信息（两列）。
    for (k, v) in ov.system.iter().take(6) {
        if y + 16.0 > rect.bottom() {
            return;
        }
        let row = Rect::new(rect.x, y, rect.w, 16.0);
        ui_text_small(frame, app, rect.x + PAD, row, k, p.text_dim, rect);
        let vx = rect.x + 92.0;
        ui_text_small(frame, app, vx, row, v, p.text, rect);
        y += 16.0;
    }

    // CPU / 内存进度条。
    y = draw_bar(app, frame, rect, y, t!("workspace-ssh-cpu"), ov.cpu_total, p.accent);
    let mem_pct = if ov.memory.1 > 0 {
        ov.memory.0 as f32 / ov.memory.1 as f32 * 100.0
    } else {
        0.0
    };
    y = draw_bar(app, frame, rect, y, t!("workspace-ssh-memory"), mem_pct, p.accent.alpha(0.65));

    // 网络：速率文字 + 折线（下行蓝、上行绿）。
    if y + 40.0 < rect.bottom() {
        let rx = format_rate(ov.net_rx_now);
        let tx = format_rate(ov.net_tx_now);
        let row = Rect::new(rect.x, y, rect.w, 16.0);
        ui_text_small(frame, app, rect.x + PAD, row, &format!("↓ {rx}"), p.accent, rect);
        let label = format!("↑ {tx}");
        let lw = frame.measure(FontKind::Ui, app.fonts.ui_small, &label);
        ui_text_small(frame, app, rect.right() - PAD - lw, row, &label, p.text_muted, rect);
        y += 16.0;
        if !ov.net_rx.is_empty() {
            y = draw_spark(app, frame, rect, y, (&ov.net_rx, &ov.net_tx), (p.accent, p.text_muted));
        }
    }

    // 网络接口（前 2 个）。
    for (name, detail) in ov.interfaces.iter().take(2) {
        if y + 16.0 > rect.bottom() {
            break;
        }
        let label = format!("{name}  {detail}");
        let row = Rect::new(rect.x, y, rect.w, 16.0);
        ui_text_small(frame, app, rect.x + PAD, row, &label, p.text_dim, rect);
        y += 16.0;
    }

    // 磁盘挂载（前 4 个）。
    for (mount, _fs, pct, used) in ov.mounts.iter().take(4) {
        if y + 18.0 > rect.bottom() {
            break;
        }
        let label = format!("{mount}  {used}  {pct}%");
        let row = Rect::new(rect.x, y, rect.w, 16.0);
        ui_text_small(frame, app, rect.x + PAD, row, &label, p.text_muted, rect);
        y += 16.0;
    }

    // 进程列表（前若干行，超出裁剪）。
    if y + 20.0 < rect.bottom() {
        ui_text_small(frame, app, rect.x + PAD, Rect::new(rect.x, y, rect.w, 18.0), t!("workspace-ssh-processes"), p.text_dim, rect);
        y += 18.0;
        for (pi, (pid, name, cpu, mem)) in ov.processes.iter().take(6).enumerate() {
            if y + 16.0 > rect.bottom() {
                break;
            }
            let label = format!("{pid:>6}  {name:<16} {cpu:>5.1}%  {mem:>8}");
            let row = Rect::new(rect.x, y, rect.w, 16.0);
            ui_text_small(frame, app, rect.x + PAD, row, &label, p.text, rect);
            let kill = Rect::new(row.right() - 30.0, y, 26.0, 16.0);
            ui_text_small(frame, app, kill.x, kill, "kill", p.error, rect);
            app.hits.ssh_ws.push((kill, crate::app::ssh_monitor::SshWsTarget::KillProc(pi)));
            y += 16.0;
        }
    }
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
