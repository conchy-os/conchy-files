//! `org.conchy.Portal.FilesHost` — хост UI для демона `xdg-desktop-portal-conchy`.
//!
//! Демон вызывает `Pick` (долгий pending-вызов: ответ приходит, когда пользователь закончил)
//! и `Cancel`. Здесь только D-Bus; окна открывает GPUI-часть, получая [`HostEvent`] из канала.

use conchy_portal_proto::apps::{AppChooseRequest, AppChooseResult};
use conchy_portal_proto::files::{FilePickRequest, FilePickResult};
use conchy_portal_proto::{FILES_HOST_BUS_NAME, HOST_OBJECT_PATH, response};
use futures::channel::{mpsc, oneshot};
use zbus::interface;

/// Что хост просит сделать GUI.
pub enum HostEvent {
    /// Показать пикер. Ответ (`response`, результат) надо отправить в `reply`; если
    /// отправитель сброшен без ответа, демон получит код 2.
    Pick { id: String, req: Box<FilePickRequest>, reply: oneshot::Sender<(u32, FilePickResult)> },
    /// Показать выбор приложения («Открыть с помощью»).
    ChooseApp { id: String, req: Box<AppChooseRequest>, reply: oneshot::Sender<(u32, AppChooseResult)> },
    /// Обновить список в открытом выборе приложения.
    UpdateChoices { id: String, choices: Vec<String> },
    /// Фронтенд закрыл запрос — закрыть окно и ответить кодом 1.
    Cancel { id: String },
}

pub struct FilesHost {
    tx: mpsc::UnboundedSender<HostEvent>,
}

impl FilesHost {
    pub fn new(tx: mpsc::UnboundedSender<HostEvent>) -> Self {
        Self { tx }
    }
}

#[interface(name = "org.conchy.Portal.FilesHost")]
impl FilesHost {
    async fn pick(&self, id: String, req: FilePickRequest) -> (u32, FilePickResult) {
        let (reply, rx) = oneshot::channel();
        if self.tx.unbounded_send(HostEvent::Pick { id, req: Box::new(req), reply }).is_err() {
            return (response::OTHER, FilePickResult::default());
        }
        rx.await.unwrap_or((response::OTHER, FilePickResult::default()))
    }

    async fn choose_app(&self, id: String, req: AppChooseRequest) -> (u32, AppChooseResult) {
        let (reply, rx) = oneshot::channel();
        if self.tx.unbounded_send(HostEvent::ChooseApp { id, req: Box::new(req), reply }).is_err() {
            return (response::OTHER, AppChooseResult::default());
        }
        rx.await.unwrap_or((response::OTHER, AppChooseResult::default()))
    }

    async fn update_choices(&self, id: String, choices: Vec<String>) {
        let _ = self.tx.unbounded_send(HostEvent::UpdateChoices { id, choices });
    }

    async fn cancel(&self, id: String) {
        let _ = self.tx.unbounded_send(HostEvent::Cancel { id });
    }
}

/// Занимает `org.conchy.Portal.FilesHost` на сессионной шине. Соединение надо держать живым.
pub fn serve(tx: mpsc::UnboundedSender<HostEvent>) -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::session()?
        .serve_at(HOST_OBJECT_PATH, FilesHost::new(tx))?
        .name(FILES_HOST_BUS_NAME)?
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, executor::block_on};

    #[test]
    fn pick_waits_for_reply_and_cancel_is_forwarded() {
        let (tx, mut rx) = mpsc::unbounded();
        let host = FilesHost::new(tx);
        let t = std::thread::spawn(move || {
            block_on(async {
                let HostEvent::Pick { id, reply, .. } = rx.next().await.unwrap() else { panic!("Pick") };
                assert_eq!(id, "/r/1");
                let _ = reply.send((0, FilePickResult { writable: true, ..Default::default() }));
                let HostEvent::Cancel { id } = rx.next().await.unwrap() else { panic!("Cancel") };
                assert_eq!(id, "/r/1");
            })
        });
        let (code, res) = block_on(host.pick("/r/1".into(), FilePickRequest::default()));
        assert_eq!(code, 0);
        assert!(res.writable);
        block_on(host.cancel("/r/1".into()));
        t.join().unwrap();
    }

    #[test]
    fn dropped_reply_is_response_2() {
        let (tx, mut rx) = mpsc::unbounded();
        let host = FilesHost::new(tx);
        let t = std::thread::spawn(move || {
            block_on(async { drop(rx.next().await) }) // окно «упало», ответа нет
        });
        let (code, _) = block_on(host.pick("/r/2".into(), FilePickRequest::default()));
        assert_eq!(code, response::OTHER);
        t.join().unwrap();
    }
}
