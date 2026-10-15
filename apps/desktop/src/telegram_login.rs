use crate::{AppWindow, telegram_session::SessionStore};
use launcher_core::{
    LauncherCore, OfflineProfile,
    auth::telegram::{TelegramAuthError, TelegramAuthGate, TelegramChallenge, TelegramSession},
};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Handle;

pub struct TelegramLogin {
    window: slint::Weak<AppWindow>,
    runtime: Handle,
    pub gate: TelegramAuthGate,
    core: LauncherCore,
    store: SessionStore,
    generation: Cell<u64>,
    challenge: RefCell<Option<TelegramChallenge>>,
}
impl TelegramLogin {
    pub fn bind(
        window: &AppWindow,
        runtime: Handle,
        gate: TelegramAuthGate,
        core: LauncherCore,
        store: SessionStore,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            window: window.as_weak(),
            runtime,
            gate,
            core,
            store,
            generation: Cell::new(0),
            challenge: RefCell::new(None),
        });
        window.set_auth_visible(true);
        window.set_auth_message("Войдите через Telegram, чтобы запустить игру. При первом входе бот предложит выбрать ник.".into());
        let c = this.clone();
        window.on_auth_requested(move || c.start());
        let c = this.clone();
        window.on_auth_cancelled(move || c.cancel());
        let c = this.clone();
        window.on_auth_open_requested(move || {
            if let Some(c) = c.challenge.borrow().as_ref() {
                open_telegram(&c.url);
            }
        });
        let c = this.clone();
        window.on_auth_logout_requested(move || c.logout());
        this.restore();
        this
    }
    fn show_error(&self, text: &str) {
        if let Some(w) = self.window.upgrade() {
            w.set_auth_busy(false);
            w.set_auth_message(text.into());
        }
    }
    fn accept(&self, session: TelegramSession) -> Result<(), &'static str> {
        let profile = OfflineProfile::new(&session.user.nickname)
            .map_err(|_| "Сервер вернул некорректный ник.")?;
        self.store
            .save(session.clone())
            .map_err(|_| "Не удалось безопасно сохранить сессию. Повторите вход.")?;
        self.core.set_offline_profile(profile);
        if let Some(w) = self.window.upgrade() {
            w.set_nickname_input(session.user.nickname.as_str().into());
            w.set_nickname_error("".into());
            w.set_auth_verified(true);
            w.set_auth_visible(false);
            w.set_auth_busy(false);
            w.set_auth_link("".into());
            if !w.get_loading() {
                w.set_play_enabled(true);
            }
        }
        self.gate.set_session(Some(session));
        Ok(())
    }
    fn restore(self: &Rc<Self>) {
        let session = match self.store.load() {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(_) => {
                let _ = self.store.remove();
                self.show_error("Не удалось прочитать сохранённую сессию. Войдите снова.");
                return;
            }
        };
        let Some(w) = self.window.upgrade() else {
            return;
        };
        w.set_auth_busy(true);
        w.set_auth_message("Проверяем сохранённую сессию…".into());
        let c = self.clone();
        let client = self.gate.client.clone();
        let token = session.token.clone();
        let task = self.runtime.spawn(async move { client.me(&token).await });
        let generation = self.generation.get();
        let _ = slint::spawn_local(async move {
            let result = task.await;
            if c.generation.get() != generation {
                return;
            }
            match result {
                Ok(Ok(user)) if user.registration_status != "banned" => {
                    let mut session = session;
                    session.user = user;
                    if let Err(e) = c.accept(session) {
                        c.show_error(e);
                    }
                }
                Ok(Err(TelegramAuthError::Unauthorized)) => {
                    let _ = c.store.remove();
                    c.show_error("Сессия истекла или аккаунт заблокирован. Войдите снова.");
                }
                _ => c.show_error("Не удалось проверить сессию. Проверьте сеть и повторите вход."),
            }
        });
    }
    fn start(self: &Rc<Self>) {
        if self.window.upgrade().is_none_or(|w| w.get_auth_busy()) {
            return;
        }
        self.cancel();
        let Some(w) = self.window.upgrade() else {
            return;
        };
        w.set_auth_visible(true);
        w.set_auth_busy(true);
        w.set_auth_message("Создаём запрос входа…".into());
        let generation = self.generation.get();
        let client = self.gate.client.clone();
        let task = self.runtime.spawn(async move { client.begin().await });
        let c = self.clone();
        let _ = slint::spawn_local(async move {
            let result = task.await;
            if c.generation.get() != generation {
                if let Ok(Ok(challenge)) = result {
                    let client = c.gate.client.clone();
                    c.runtime.spawn(async move {
                        client.cancel(&challenge).await;
                    });
                }
                return;
            }
            let challenge = match result {
                Ok(Ok(challenge)) => challenge,
                Ok(Err(e)) => {
                    c.show_error(&e.to_string());
                    return;
                }
                Err(_) => {
                    c.show_error("Не удалось начать вход.");
                    return;
                }
            };
            *c.challenge.borrow_mut() = Some(challenge.clone());
            if let Some(w) = c.window.upgrade() {
                w.set_auth_link(challenge.url.as_str().into());
                w.set_auth_message(format!("Код запроса: {}\nОткройте бота и подтвердите вход с этим кодом. Ссылка действует 5 минут.", challenge.code).into());
            }
            let mut failures = 0;
            loop {
                if c.generation.get() != generation || c.window.upgrade().is_none() {
                    return;
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                if now >= u128::from(challenge.expires_at) {
                    c.show_error("Время входа истекло. Нажмите «Войти через Telegram» ещё раз.");
                    return;
                }
                let client = c.gate.client.clone();
                let attempt = challenge.clone();
                let task = c.runtime.spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    client.status(&attempt).await
                });
                let result = task.await;
                if c.generation.get() != generation {
                    return;
                }
                match result {
                    Ok(Ok(status)) if status == "confirmed" => break,
                    Ok(Ok(status)) if status == "pending" => failures = 0,
                    Ok(Ok(_)) => {
                        c.show_error("Вход отклонён, отменён или истёк. Создайте новый запрос.");
                        return;
                    }
                    _ => {
                        failures += 1;
                        if failures >= 5 {
                            c.show_error("Связь с сервером потеряна. Повторите вход.");
                            return;
                        }
                    }
                }
            }
            let client = c.gate.client.clone();
            let task = c
                .runtime
                .spawn(async move { client.exchange(&challenge).await });
            let result = task.await;
            if c.generation.get() != generation {
                if let Ok(Ok(session)) = result {
                    let client = c.gate.client.clone();
                    c.runtime.spawn(async move {
                        let _ = client.logout(&session.token).await;
                    });
                }
                return;
            }
            match result {
                Ok(Ok(session)) => {
                    if let Err(e) = c.accept(session) {
                        c.show_error(e);
                    }
                }
                Ok(Err(e)) => c.show_error(&e.to_string()),
                Err(_) => c.show_error("Не удалось получить сессию. Повторите вход."),
            }
        });
    }
    fn cancel(&self) {
        self.generation.set(self.generation.get() + 1);
        if let Some(challenge) = self.challenge.borrow_mut().take() {
            let client = self.gate.client.clone();
            self.runtime.spawn(async move {
                client.cancel(&challenge).await;
            });
        }
        if let Some(w) = self.window.upgrade() {
            w.set_auth_busy(false);
            w.set_auth_link("".into());
            w.set_auth_message("Войдите через Telegram, чтобы запустить игру.".into());
        }
    }
    fn logout(self: &Rc<Self>) {
        if self.window.upgrade().is_none_or(|w| w.get_loading()) {
            return;
        }
        self.cancel();
        let session = self.gate.session();
        self.gate.set_session(None);
        self.core.clear_offline_profile();
        if let Some(w) = self.window.upgrade() {
            w.set_auth_verified(false);
            w.set_auth_visible(true);
            w.set_nickname_input("".into());
        }
        if self.store.remove().is_err() {
            self.show_error(
                "Сессия отключена, но её файл не удалось удалить. Проверьте права на папку данных.",
            );
        }
        if let Some(session) = session {
            let client = self.gate.client.clone();
            let c = self.clone();
            let task = self
                .runtime
                .spawn(async move { client.logout(&session.token).await });
            let generation = self.generation.get();
            let _ = slint::spawn_local(async move {
                if !matches!(task.await, Ok(Ok(()))) && c.generation.get() == generation {
                    c.show_error("Вы вышли на этом устройстве. Серверный отзыв сессии не подтверждён: проверьте сеть.");
                }
            });
        }
    }
}
fn open_telegram(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // URL has been restricted to HTTPS t.me by TelegramAuthClient::begin.
        let _ = std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x08000000)
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = url;
}
