//! SSH 资产侧栏：列出 `~/.ssh/config` 中的主机，显示连接状态与统计。
//!
//! 这是 SSH 侧栏视图的绘制。资产来自远程模块已有的主机读取（`App::ssh_assets`），
//! 不保存任何凭据。点击第 4 段「SSH」标签切换进来；再次点击标签会重新读取主机列表。

use winc_i18n::t;
use winc_render::{Frame, Rect};

use super::*;
use super::sidebar::ContextBar;

/// 资产列表一行的高度（逻辑像素），与提交列表行高一致。
/// 资产行高（两行：名称、地址）。
const ROW: f32 = 38.0;
/// 列表两列的左右内边距（逻辑像素）。
const PAD: f32 = 12.0;

/// 绘制 SSH 资产视图：第二行上下文栏 + 资产列表 + 底部统计。
pub(in crate::view) fn draw(app: &mut App, frame: &mut Frame<'_>) {
    let p = app.palette;
    let sidebar = app.layout.sidebar;
    let header = app.layout.sidebar_header;

    // 上下文栏（项目与分支），与其他视图一致。SSH 视图没有右端工具按钮，占满标题行。
    let bar = ContextBar::new(app, frame);
    super::sidebar::draw_context_bar(app, frame, &bar, header.right() - 8.0);

    // 资产列表区域：第二行以下到侧栏底部（上方留出搜索框）。
    let search_h = 26.0;
    let list = Rect::new(
        sidebar.x,
        header.bottom() + 4.0 + search_h,
        sidebar.w,
        (sidebar.bottom() - header.bottom() - 4.0 - search_h).max(0.0),
    );
    let list_clip = list.intersect(sidebar);

    // 资产搜索框：按主机名、地址或端口筛选。
    let search = Rect::new(
        sidebar.x + 8.0,
        header.bottom() + 4.0,
        (sidebar.w - 16.0).max(0.0),
        search_h - 4.0,
    );
    frame.rect_clipped(search, p.popup_bg, crate::theme::RADIUS_ROW, sidebar);
    let (text, color) = if app.remote.asset_filter.is_empty() {
        (t!("workspace-ssh-search").to_string(), p.text_dim)
    } else {
        (app.remote.asset_filter.clone(), p.text)
    };
    ui_text_small(frame, app, search.x + 8.0, search, &text, color, sidebar);
    app.hits
        .ssh_ws
        .push((search, crate::app::ssh_monitor::SshWsTarget::AssetSearch));

    let assets_all = app.ssh_assets();
    let filter = app.remote.asset_filter.to_lowercase();
    // 保留原始下标：点击与移除都按配置中的顺序引用。
    let assets: Vec<(usize, String)> = assets_all
        .into_iter()
        .enumerate()
        .filter(|(_, h)| filter.is_empty() || h.to_lowercase().contains(&filter))
        .collect();
    let title = t!("workspace-sidebar-ssh-assets");
    let title_row = Rect::new(list.x, list.y, list.w, 24.0);
    ui_text_small(
        frame,
        app,
        list.x + PAD,
        title_row,
        format!("{title}  ·  {}", assets.len()).as_str(),
        p.text_muted,
        list_clip,
    );

    // 「＋ 新建」按钮：打开连接对话框，填写主机、端口、密钥。
    let new_btn = Rect::new(list.right() - 62.0, list.y + 1.0, 54.0, 22.0);
    frame.rect_clipped(new_btn, p.hover, crate::theme::RADIUS_ROW, list_clip);
    ui_text_small(
        frame,
        app,
        new_btn.x + 6.0,
        new_btn,
        &format!("＋ {}", t!("workspace-ssh-new")),
        p.text_muted,
        list_clip,
    );
    app.hits.ssh_ws.push((new_btn, crate::app::ssh_monitor::SshWsTarget::NewHost));

    let mut y = title_row.bottom() + 2.0;
    if assets.is_empty() {
        let text = t!("workspace-sidebar-ssh-no-host");
        ui_text_small(frame, app, list.x + PAD, Rect::new(list.x, y, list.w, ROW), text, p.text_dim, list_clip);
        return;
    }

    for (real, host) in &assets {
        let row = Rect::new(list.x + 4.0, y, list.w - 8.0, ROW);
        if row.bottom() > list.bottom() + ROW {
            break;
        }
        let selected = *real == app.remote.asset_selection;
        if selected {
            frame.rect_clipped(row, p.selection, crate::theme::RADIUS_ROW, list_clip);
        }
        // 状态点：能连的主机统一用强调色（真实连通性在连接时确认）。
        let dot = Rect::new(row.x + PAD - 6.0, row.y + 16.0, 6.0, 6.0);
        frame.rect_clipped(dot, p.accent, 3.0, list_clip);
        // 第一行主机名，第二行地址。
        let name_row = Rect::new(row.x, row.y, row.w, 19.0);
        ui_text(frame, app, row.x + PAD + 8.0, name_row, host, p.text, list_clip);
        if let Some((user, addr, port)) = crate::app::remote::host_endpoint(host) {
            let at = user.map(|u| format!("{u}@")).unwrap_or_default();
            let p2 = port.map(|x| format!(":{x}")).unwrap_or_default();
            let addr_row = Rect::new(row.x, row.y + 18.0, row.w, 17.0);
            ui_text_small(frame, app, row.x + PAD + 8.0, addr_row, &format!("{at}{addr}{p2}"), p.text_dim, list_clip);
        }
        // 行尾的移除按钮：从 ssh 配置里删掉这台主机。
        let remove = Rect::new(row.right() - 18.0, row.y + 12.0, 14.0, 15.0);
        ui_text_small(frame, app, remove.x, remove, "✕", p.text_dim, list_clip);
        app.hits
            .ssh_ws
            .push((remove, crate::app::ssh_monitor::SshWsTarget::RemoveHost(*real)));
        app.hits.ssh_ws.push((row, crate::app::ssh_monitor::SshWsTarget::Asset(*real)));
        y += ROW;
    }

    // 底部提示：再次点击 SSH 标签刷新；连接用「连接到 SSH 主机」命令。
    let hint = t!("workspace-sidebar-ssh-hint");
    let hint_row = Rect::new(sidebar.x, sidebar.bottom() - 24.0, sidebar.w, 24.0);
    ui_text_small(frame, app, sidebar.x + PAD, hint_row, hint, p.text_dim, hint_row.intersect(sidebar));
}
