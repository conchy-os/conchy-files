//! «Устройства» в боковой панели: подключение, отключение и безопасное извлечение
//! через UDisks2, уведомления о результате.

use std::path::PathBuf;
use std::time::Duration;

use conchy_files::devices::{self, Volume};
use conchy_files::model::human_size;
use conchy_files::places::{self, Target};
use conchy_files::{icons, notify};
use futures::StreamExt;
use gpui::{
    AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, Pixels, Point, Timer, div, prelude::*, px, rgb,
};

use super::actions::Cmd;
use super::dialogs::{Dialog, Menu, MenuItem};
use super::view::{icon, side_row, symbolic, tooltip, usage_bar};
use super::{BORDER, DIM, FilesWindow, TEXT};

impl FilesWindow {
    /// Перечитывает список разделов из UDisks2 и таблицу монтирования (в фоне).
    pub(super) fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        let job = cx.background_executor().spawn(async { (devices::volumes(), places::mounts()) });
        cx.spawn(async move |this, cx| {
            let (vols, mounts) = job.await;
            let _ = this.update(cx, |this, cx| {
                this.udisks = vols.is_ok();
                this.volumes = vols.unwrap_or_default();
                this.mounts = mounts;
                cx.notify();
            });
        })
        .detach();
    }

    /// Подписка на сигналы UDisks2: флешку вставили, вынули, смонтировали.
    /// Пачка сигналов (их бывает десяток на одно событие) даёт одно обновление.
    pub(super) fn watch_devices(&mut self, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        devices::watch(move || {
            let _ = tx.unbounded_send(());
        });
        cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                Timer::after(Duration::from_millis(250)).await;
                while rx.try_recv().is_ok() {}
                if this.update(cx, |this, cx| this.refresh_devices(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        self.refresh_devices(cx);
    }

    pub(super) fn device_cmd(&mut self, cmd: Cmd, cx: &mut Context<Self>) {
        let ix = match cmd {
            Cmd::OpenDevice(i) | Cmd::Mount(i) | Cmd::Unmount(i) | Cmd::Eject(i) => i,
            _ => return,
        };
        let Some(v) = self.volumes.get(ix).cloned() else { return };
        match cmd {
            Cmd::OpenDevice(_) => match &v.mount_point {
                Some(mp) => self.navigate(Target::Dir(mp.clone()), cx),
                None => self.mount_volume(v, true, cx),
            },
            Cmd::Mount(_) => self.mount_volume(v, false, cx),
            Cmd::Unmount(_) => self.unmount_volume(v, false, cx),
            Cmd::Eject(_) => self.unmount_volume(v, true, cx),
            _ => {}
        }
    }

    fn mount_volume(&mut self, v: Volume, open: bool, cx: &mut Context<Self>) {
        if !self.device_busy.insert(v.object.clone()) {
            return;
        }
        let job = cx.background_executor().spawn({
            let v = v.clone();
            async move { devices::mount(&v) }
        });
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                this.device_busy.remove(&v.object);
                match res {
                    Ok(mp) if open => this.navigate(Target::Dir(mp), cx),
                    Ok(_) => {}
                    Err(e) => {
                        this.dialog = Some(Dialog::Errors {
                            title: format!("Не удалось подключить «{}»", v.name()),
                            errors: vec![(PathBuf::from(&v.device), e)],
                        })
                    }
                }
                this.refresh_devices(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Отключение или безопасное извлечение. Если окно открыто на этом устройстве,
    /// сначала уходим в домашнюю папку: иначе пользователь останется в пустоте.
    fn unmount_volume(&mut self, v: Volume, eject: bool, cx: &mut Context<Self>) {
        if !self.device_busy.insert(v.object.clone()) {
            return;
        }
        if let (Some(mp), Some(cur)) = (&v.mount_point, self.current_dir()) {
            if cur.starts_with(mp) {
                self.navigate(Target::Dir(places::home()), cx);
            }
        }
        let all = self.volumes.clone();
        let job = cx.background_executor().spawn({
            let v = v.clone();
            async move {
                let res = if eject { devices::eject(&v, &all) } else { devices::unmount(&v) };
                // Системное уведомление тоже из фона: это обращение к D-Bus.
                let notified = res.is_ok() && eject && notify::send(&format!("«{}» можно извлечь", v.name()), "Устройство отключено", "media-removable");
                (res, notified)
            }
        });
        cx.spawn(async move |this, cx| {
            let (res, notified) = job.await;
            let _ = this.update(cx, |this, cx| {
                this.device_busy.remove(&v.object);
                match res {
                    Ok(()) if eject && !notified => this.notice(format!("«{}» можно извлечь", v.name()), cx),
                    Ok(()) if !eject => this.notice(format!("«{}» отключено", v.name()), cx),
                    Ok(()) => {}
                    Err(e) => {
                        let what = if eject { "извлечь" } else { "отключить" };
                        this.dialog = Some(Dialog::Errors {
                            title: format!("Не удалось {what} «{}»", v.name()),
                            errors: vec![(PathBuf::from(&v.device), e)],
                        });
                    }
                }
                this.refresh_devices(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn open_device_menu(&mut self, ix: usize, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(v) = self.volumes.get(ix) else { return };
        let mounted = v.mount_point.is_some();
        let items = vec![
            Some(MenuItem::new("Открыть", "", Cmd::OpenDevice(ix), true)),
            None,
            Some(MenuItem::new("Подключить", "", Cmd::Mount(ix), !mounted)),
            Some(MenuItem::new("Отключить", "", Cmd::Unmount(ix), mounted)),
            Some(MenuItem::new("Безопасно извлечь", "", Cmd::Eject(ix), v.can_eject())),
        ];
        self.menu = Some(Menu { pos, items });
        cx.notify();
    }

    /// Раздел «Устройства»: «Файловая система», затем разделы из UDisks2, затем
    /// прочие точки монтирования, которых UDisks2 не знает (или когда его нет).
    pub(super) fn render_devices(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let cur = self.current_dir().map(PathBuf::from);
        let in_place = self.places.iter().any(|p| p.target == self.location);
        // Подсвечиваем самое длинное подходящее место: внутри флешки — флешку, а не корень.
        let mut points: Vec<PathBuf> = self.mounts.iter().map(|m| m.mount_point.clone()).collect();
        points.extend(self.volumes.iter().filter_map(|v| v.mount_point.clone()));
        let best = cur.as_ref().and_then(|c| points.iter().filter(|p| c.starts_with(p)).max_by_key(|p| p.as_os_str().len()).cloned());
        let active = |p: &PathBuf| !in_place && best.as_ref() == Some(p);

        let mut out = Vec::new();
        let by_udisks: Vec<PathBuf> = self.volumes.iter().filter_map(|v| v.mount_point.clone()).collect();
        for (i, m) in self.mounts.iter().enumerate() {
            let is_root = m.mount_point == PathBuf::from("/");
            if !is_root && by_udisks.contains(&m.mount_point) {
                continue;
            }
            let target = Target::Dir(m.mount_point.clone());
            let tip = format!("{} · {} · свободно {} из {}", m.device, m.fs_type, human_size(m.free), human_size(m.total));
            out.push(
                super::dnd::drop_target(side_row(("mount", i), active(&m.mount_point)), Target::Dir(m.mount_point.clone()), cx)
                    .h(px(36.))
                    .tooltip(tooltip(tip))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.navigate(target.clone(), cx)))
                    .child(icon(icons::icons().lookup(&[m.icon(), "drive-harddisk"], 16).as_ref(), 16.))
                    .child(name_and_bar(m.name.clone(), Some(m.used_fraction()), None))
                    .into_any_element(),
            );
        }

        for (i, v) in self.volumes.iter().enumerate() {
            let busy = self.device_busy.contains(&v.object);
            let space = v.mount_point.as_ref().and_then(|mp| places::statvfs(mp));
            let used = space.map(|(t, f)| if t == 0 { 0.0 } else { 1.0 - f as f32 / t as f32 });
            let mut tip = format!("{} · {} · {}", v.device, v.fs_type, human_size(v.size));
            if !v.model.is_empty() {
                tip.push_str(&format!(" · {}", v.model));
            }
            match (&v.mount_point, space) {
                (Some(mp), Some((t, f))) => tip.push_str(&format!("\n{} · свободно {} из {}", mp.display(), human_size(f), human_size(t))),
                (None, _) => tip.push_str("\nНе подключено: щёлкните, чтобы подключить"),
                _ => {}
            }
            let status = if busy {
                Some(if v.mount_point.is_some() { "Отключение…" } else { "Подключение…" })
            } else {
                None
            };
            let is_active = v.mount_point.as_ref().is_some_and(|mp| active(mp));
            let row = side_row(("volume", i), is_active);
            let row = match &v.mount_point {
                Some(mp) => super::dnd::drop_target(row, Target::Dir(mp.clone()), cx),
                None => row,
            };
            let mut row = row
                .h(px(36.))
                .tooltip(tooltip(tip))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.device_cmd(Cmd::OpenDevice(i), cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.open_device_menu(i, ev.position, cx);
                    }),
                )
                .child(icon(icons::icons().lookup(&[v.icon(), "drive-harddisk"], 16).as_ref(), 16.))
                .child(name_and_bar(v.name(), if v.mount_point.is_some() { used } else { None }, status));
            if v.mount_point.is_none() && !busy {
                row = row.text_color(rgb(DIM));
            }
            // Кнопка «извлечь», как в Thunar, — только у подключённых съёмных.
            if v.mount_point.is_some() && v.can_eject() && !busy {
                row = row.child(
                    div()
                        .id(("eject", i))
                        .size(px(22.))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .hover(|s| s.bg(rgb(BORDER)))
                        .tooltip(tooltip("Безопасно извлечь"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.device_cmd(Cmd::Eject(i), cx);
                            }),
                        )
                        .child(symbolic("media-eject", "⏏", rgb(TEXT))),
                );
            }
            out.push(row.into_any_element());
        }
        out
    }
}

/// Имя устройства и под ним полоска занятого места (или статус «Подключение…»).
fn name_and_bar(name: String, used: Option<f32>, status: Option<&'static str>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(3.))
        .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
        .when_some(status, |d, s| d.child(div().text_size(px(11.)).text_color(rgb(DIM)).child(s)))
        .when(status.is_none(), |d| d.children(used.map(usage_bar)))
}
