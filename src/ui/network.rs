//! Сетевые адреса в строке пути (`sftp://`, `smb://`, `ftp://`, `dav(s)://`)
//! и раздел «Сеть» в боковой панели.

use std::path::PathBuf;

use conchy_files::places::{self, Target};
use conchy_files::remote::{self, MountError, Remote};
use gpui::{AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, div, prelude::*, px, rgb};

use super::dialogs::Dialog;
use super::field::TextField;
use super::view::{icon, side_row, symbolic, tooltip};
use super::{BORDER, FilesWindow, TEXT};

impl FilesWindow {
    /// Подключает сетевой ресурс (в фоне) и открывает его. Пароль спрашивается,
    /// только если без него не пустили (ключи SSH и агент пробуются первыми).
    pub(super) fn mount_remote(&mut self, r: Remote, password: Option<String>, cx: &mut Context<Self>) {
        self.notice(format!("Подключение к {}…", r.label()), cx);
        let tried_password = password.is_some();
        let job = cx.background_executor().spawn({
            let r = r.clone();
            async move { r.mount(password.as_deref()) }
        });
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(path) => {
                        this.remotes = remote::mounted();
                        this.navigate(Target::Dir(path), cx);
                    }
                    Err(MountError::NeedPassword) => {
                        this.dialog = Some(Dialog::Password {
                            error: tried_password.then(|| "Пароль не подошёл".to_string()),
                            remote: r,
                            field: TextField::new("", 0..0),
                        });
                    }
                    Err(e) => {
                        this.dialog = Some(Dialog::Errors {
                            title: format!("Не удалось подключиться к {}", r.label()),
                            errors: vec![(PathBuf::from(r.label()), e.to_string())],
                        })
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn unmount_remote(&mut self, mp: PathBuf, cx: &mut Context<Self>) {
        if self.current_dir().is_some_and(|c| c.starts_with(&mp)) {
            self.navigate(Target::Dir(places::home()), cx);
        }
        let job = cx.background_executor().spawn({
            let mp = mp.clone();
            async move { remote::unmount(&mp) }
        });
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                this.remotes = remote::mounted();
                if let Err(e) = res {
                    this.dialog = Some(Dialog::Errors { title: "Не удалось отключить сетевую папку".into(), errors: vec![(mp, e)] });
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Раздел «Сеть»: подключённые сетевые папки с кнопкой отключения.
    pub(super) fn render_network(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let cur = self.current_dir().map(PathBuf::from);
        self.remotes
            .iter()
            .enumerate()
            .map(|(i, mp)| {
                let name = mp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                // «sftp-ivan@server» → «ivan@server», схема уходит в подсказку.
                let (scheme, label) = name.split_once('-').unwrap_or(("", &name));
                let label = label.replace('_', "/");
                let target = Target::Dir(mp.clone());
                let active = cur.as_ref().is_some_and(|c| c.starts_with(mp));
                let path = mp.clone();
                let icon_path = conchy_files::icons::icons().lookup(&["folder-remote", "network-server", "folder"], 16);
                side_row(("remote", i), active)
                    .tooltip(tooltip(format!("{scheme}://{label}\n{}", mp.display())))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.navigate(target.clone(), cx)))
                    .child(icon(icon_path.as_ref(), 16.))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(label))
                    .child(
                        div()
                            .id(("unmount", i))
                            .size(px(22.))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .hover(|s| s.bg(rgb(BORDER)))
                            .tooltip(tooltip("Отключить"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.unmount_remote(path.clone(), cx);
                                }),
                            )
                            .child(symbolic("media-eject", "⏏", rgb(TEXT))),
                    )
                    .into_any_element()
            })
            .collect()
    }
}
